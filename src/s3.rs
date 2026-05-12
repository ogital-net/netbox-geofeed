//! Minimal S3 `PutObject` client with in-tree AWS `SigV4` signing.
//!
//! Uses [`reqwest`] for HTTP and [`aws_lc_rs`] for SHA-256 / HMAC primitives.
//! Credentials are read from the standard environment variables:
//!
//! - `AWS_ACCESS_KEY_ID` (required)
//! - `AWS_SECRET_ACCESS_KEY` (required)
//! - `AWS_SESSION_TOKEN` (optional; for STS / role-chained sessions)
//!
//! Region resolution: explicit `--s3-region` flag → `AWS_REGION` env var.
//!
//! Object metadata applied to every upload:
//! - `Content-Type: text/csv; charset=utf-8`
//! - `Cache-Control: max-age=300, public`
//!
//! Atomicity: S3 `PutObject` is itself atomic at the object level — clients
//! never observe a partially-written object — so no temp-key dance is
//! required.

use anyhow::{Context as _, anyhow};
use aws_lc_rs::{digest, hmac};
use reqwest::Client;
use std::time::SystemTime;

const CONTENT_TYPE: &str = "text/csv; charset=utf-8";
const CACHE_CONTROL: &str = "max-age=300, public";
const SERVICE: &str = "s3";
const ALGORITHM: &str = "AWS4-HMAC-SHA256";

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Resolved credentials + region pulled from the environment / CLI flags.
pub struct S3Config {
    pub access_key: String,
    pub secret_key: String,
    pub session_token: Option<String>,
    pub region: String,
}

impl S3Config {
    /// Build a config from environment variables, optionally overriding
    /// `AWS_REGION` with a CLI-supplied value.
    ///
    /// # Errors
    ///
    /// Returns an error if `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, or
    /// the region (neither flag nor env) are missing.
    pub fn from_env(region_override: Option<&str>) -> anyhow::Result<Self> {
        let access_key = std::env::var("AWS_ACCESS_KEY_ID").context("AWS_ACCESS_KEY_ID not set")?;
        let secret_key =
            std::env::var("AWS_SECRET_ACCESS_KEY").context("AWS_SECRET_ACCESS_KEY not set")?;
        let session_token = std::env::var("AWS_SESSION_TOKEN").ok();
        let region = region_override
            .map(str::to_owned)
            .or_else(|| std::env::var("AWS_REGION").ok())
            .context("region not set (pass --s3-region or set AWS_REGION)")?;
        Ok(Self {
            access_key,
            secret_key,
            session_token,
            region,
        })
    }
}

/// Upload `content` to `s3://<bucket>/<key>` via a single signed `PutObject`.
///
/// # Errors
///
/// Returns an error if the HTTP request fails or S3 returns a non-2xx status.
pub async fn put_object(
    client: &Client,
    cfg: &S3Config,
    bucket: &str,
    key: &str,
    content: Vec<u8>,
) -> anyhow::Result<()> {
    put_object_to(
        client,
        cfg,
        bucket,
        key,
        content,
        &default_endpoint(&cfg.region),
    )
    .await
}

/// Build the default virtual-hosted-style endpoint for a region.
fn default_endpoint(region: &str) -> String {
    format!("https://s3.{region}.amazonaws.com")
}

/// Upload `content` to `<endpoint>/<bucket>/<key>`.
///
/// Path-style addressing is used so the same code path works against
/// `MinIO` / `wiremock` test servers without DNS gymnastics. AWS S3 still
/// accepts path-style for legacy compatibility.
async fn put_object_to(
    client: &Client,
    cfg: &S3Config,
    bucket: &str,
    key: &str,
    content: Vec<u8>,
    endpoint: &str,
) -> anyhow::Result<()> {
    let now = SystemTime::now();
    let (amz_date, date_stamp) = format_dates(now);

    let host = endpoint
        .strip_prefix("https://")
        .or_else(|| endpoint.strip_prefix("http://"))
        .unwrap_or(endpoint)
        .trim_end_matches('/');

    let canonical_uri = format!("/{bucket}/{}", uri_encode(key, false));
    let url = format!("{endpoint}/{bucket}/{}", uri_encode(key, false));

    let payload_hash = hex_sha256(&content);

    // Canonical headers (must be sorted by lowercase header name).
    let mut headers: Vec<(String, String)> = vec![
        ("cache-control".into(), CACHE_CONTROL.into()),
        ("content-type".into(), CONTENT_TYPE.into()),
        ("host".into(), host.to_owned()),
        ("x-amz-content-sha256".into(), payload_hash.clone()),
        ("x-amz-date".into(), amz_date.clone()),
    ];
    if let Some(tok) = &cfg.session_token {
        headers.push(("x-amz-security-token".into(), tok.clone()));
    }
    headers.sort_by(|a, b| a.0.cmp(&b.0));

    let signed_headers = headers
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");

    let canonical_headers = headers.iter().fold(String::new(), |mut acc, (k, v)| {
        acc.push_str(k);
        acc.push(':');
        acc.push_str(v.trim());
        acc.push('\n');
        acc
    });

    let canonical_request =
        format!("PUT\n{canonical_uri}\n\n{canonical_headers}\n{signed_headers}\n{payload_hash}");

    let credential_scope = format!("{date_stamp}/{}/{SERVICE}/aws4_request", cfg.region);
    let string_to_sign = format!(
        "{ALGORITHM}\n{amz_date}\n{credential_scope}\n{}",
        hex_sha256(canonical_request.as_bytes()),
    );

    let signing_key = derive_signing_key(&cfg.secret_key, &date_stamp, &cfg.region, SERVICE);
    let signature = hex_lower(hmac_sha256(&signing_key, string_to_sign.as_bytes()).as_ref());

    let authorization = format!(
        "{ALGORITHM} Credential={access}/{credential_scope}, SignedHeaders={signed_headers}, Signature={signature}",
        access = cfg.access_key,
    );

    let mut req = client.put(&url).body(content);
    for (k, v) in &headers {
        // Skip Host — reqwest sets it from the URL.
        if k == "host" {
            continue;
        }
        req = req.header(k, v);
    }
    req = req.header("authorization", authorization);

    let resp = req
        .send()
        .await
        .with_context(|| format!("PutObject HTTP request to {url} failed"))?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!(
            "PutObject s3://{bucket}/{key} failed: HTTP {status}: {body}"
        ));
    }

    log::debug!("PutObject succeeded: bucket={bucket} key={key}");
    Ok(())
}

/// Upload a versioned mirror at `<key-stem>-<timestamp><ext>`.
///
/// Failures are intentionally non-fatal: callers log at `WARN` and continue.
///
/// # Errors
///
/// Returns an error if the underlying [`put_object`] call fails.
pub async fn upload_versioned_mirror(
    client: &Client,
    cfg: &S3Config,
    bucket: &str,
    key: &str,
    content: Vec<u8>,
    timestamp: &str,
) -> anyhow::Result<()> {
    let mirror_key = versioned_key(key, timestamp);
    put_object(client, cfg, bucket, &mirror_key, content).await?;
    log::info!("versioned mirror uploaded: key={mirror_key}");
    Ok(())
}

// ---------------------------------------------------------------------------
// SigV4 primitives
// ---------------------------------------------------------------------------

/// Format `now` as the `SigV4` `x-amz-date` and `YYYYMMDD` date stamp.
fn format_dates(now: SystemTime) -> (String, String) {
    let secs = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let (y, m, d, hh, mm, ss) = epoch_to_ymdhms(secs);
    (
        format!("{y:04}{m:02}{d:02}T{hh:02}{mm:02}{ss:02}Z"),
        format!("{y:04}{m:02}{d:02}"),
    )
}

/// Convert a Unix epoch timestamp (seconds, UTC) to civil
/// `(year, month, day, hour, minute, second)` using Howard Hinnant's
/// `days_from_civil`-inverse algorithm — branch-free, no leap-table lookups.
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
fn epoch_to_ymdhms(secs: u64) -> (i32, u32, u32, u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let time_of_day = (secs % 86_400) as u32;
    let hh = time_of_day / 3_600;
    let mm = (time_of_day / 60) % 60;
    let ss = time_of_day % 60;

    // Days since 1970-01-01 → days since 0000-03-01 (shifted civil epoch).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = u32::try_from(z.rem_euclid(146_097)).unwrap_or(0); // 0..=146_096
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // 0..=399
    let y = i64::from(yoe) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // 0..=365
    let mp = (5 * doy + 2) / 153; // 0..=11
    let d = doy - (153 * mp + 2) / 5 + 1; // 1..=31
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // 1..=12
    let year = (y + i64::from(m <= 2)) as i32;
    (year, m, d, hh, mm, ss)
}

/// `SigV4` signing key chain: HMAC-SHA256 over date → region → service → `"aws4_request"`.
fn derive_signing_key(secret: &str, date_stamp: &str, region: &str, service: &str) -> Vec<u8> {
    let k_date = hmac_sha256(format!("AWS4{secret}").as_bytes(), date_stamp.as_bytes());
    let k_region = hmac_sha256(k_date.as_ref(), region.as_bytes());
    let k_service = hmac_sha256(k_region.as_ref(), service.as_bytes());
    let k_signing = hmac_sha256(k_service.as_ref(), b"aws4_request");
    k_signing.as_ref().to_vec()
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> hmac::Tag {
    let key = hmac::Key::new(hmac::HMAC_SHA256, key);
    hmac::sign(&key, data)
}

fn hex_sha256(data: &[u8]) -> String {
    hex_lower(digest::digest(&digest::SHA256, data).as_ref())
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}
const HEX: &[u8; 16] = b"0123456789abcdef";

/// AWS-flavored URI encoding (RFC 3986 unreserved characters + `/` for paths).
///
/// `SigV4` requires percent-encoded bytes use **uppercase** hex digits.
fn uri_encode(input: &str, encode_slash: bool) -> String {
    const HEX_UPPER: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(input.len());
    for &b in input.as_bytes() {
        let unreserved = b.is_ascii_alphanumeric()
            || matches!(b, b'-' | b'_' | b'.' | b'~')
            || (!encode_slash && b == b'/');
        if unreserved {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX_UPPER[(b >> 4) as usize] as char);
            out.push(HEX_UPPER[(b & 0x0f) as usize] as char);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Versioned-key helper
// ---------------------------------------------------------------------------

/// Build `<stem>-<timestamp-safe><ext>` from a key and a timestamp string.
///
/// Colons in `timestamp` are replaced with hyphens so the resulting key is
/// safe for all S3 consumers and logging systems.
pub(crate) fn versioned_key(key: &str, timestamp: &str) -> String {
    let ts_safe = timestamp.replace(':', "-");
    if let Some(dot) = key.rfind('.') {
        let (stem, ext) = key.split_at(dot);
        format!("{stem}-{ts_safe}{ext}")
    } else {
        format!("{key}-{ts_safe}")
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header_exists, method, path},
    };

    fn test_cfg() -> S3Config {
        S3Config {
            access_key: "AKIAIOSFODNN7EXAMPLE".to_owned(),
            secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".to_owned(),
            session_token: None,
            region: "us-east-1".to_owned(),
        }
    }

    #[tokio::test]
    async fn put_object_sends_signed_put_with_required_headers() {
        let server = MockServer::start().await;

        Mock::given(method("PUT"))
            .and(path("/test-bucket/geofeed.csv"))
            .and(header_exists("authorization"))
            .and(header_exists("x-amz-date"))
            .and(header_exists("x-amz-content-sha256"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let client = Client::new();
        put_object_to(
            &client,
            &test_cfg(),
            "test-bucket",
            "geofeed.csv",
            b"prefix,US,,,\n".to_vec(),
            &server.uri(),
        )
        .await
        .expect("put should succeed");
    }

    #[tokio::test]
    async fn put_object_returns_error_on_non_2xx() {
        let server = MockServer::start().await;

        Mock::given(method("PUT"))
            .and(path("/b/k"))
            .respond_with(ResponseTemplate::new(403).set_body_string("AccessDenied"))
            .mount(&server)
            .await;

        let client = Client::new();
        let err = put_object_to(&client, &test_cfg(), "b", "k", vec![], &server.uri())
            .await
            .expect_err("403 should produce an error");
        assert!(err.to_string().contains("403"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn session_token_is_included_when_set() {
        let server = MockServer::start().await;

        Mock::given(method("PUT"))
            .and(path("/b/k"))
            .and(header_exists("x-amz-security-token"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let cfg = S3Config {
            session_token: Some("session-token-value".to_owned()),
            ..test_cfg()
        };
        let client = Client::new();
        put_object_to(&client, &cfg, "b", "k", vec![], &server.uri())
            .await
            .expect("put should succeed");
    }

    #[tokio::test]
    async fn versioned_mirror_uploads_under_versioned_key() {
        let server = MockServer::start().await;

        Mock::given(method("PUT"))
            .and(path("/b/feeds/geofeed-2026-05-05T12-34-56Z.csv"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        // upload_versioned_mirror routes through put_object → default endpoint;
        // exercise versioned_key directly + put_object_to here for hermetic testing.
        let client = Client::new();
        let mirror_key = versioned_key("feeds/geofeed.csv", "2026-05-05T12:34:56Z");
        put_object_to(
            &client,
            &test_cfg(),
            "b",
            &mirror_key,
            b"data".to_vec(),
            &server.uri(),
        )
        .await
        .expect("mirror put should succeed");
    }

    // ── SigV4 known-answer tests ────────────────────────────────────────────

    /// Verified against an independent Python `hmac.new(...sha256).digest()`
    /// chain over the canonical AWS-published example inputs.
    #[test]
    fn signing_key_matches_independent_reference() {
        let key = derive_signing_key(
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            "20120215",
            "us-east-1",
            "iam",
        );
        let expected =
            hex_decode("004aa806e13dae88b9032d9261bcb04c67d023afadd221e6b0d206e1760e0b5e");
        assert_eq!(key, expected);
    }

    fn hex_decode(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn hex_sha256_empty_string() {
        assert_eq!(
            hex_sha256(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn epoch_to_ymdhms_unix_epoch() {
        assert_eq!(epoch_to_ymdhms(0), (1970, 1, 1, 0, 0, 0));
    }

    #[test]
    fn epoch_to_ymdhms_known_date() {
        // 1777034096 = 2026-04-24T12:34:56Z (verified via Python datetime).
        assert_eq!(epoch_to_ymdhms(1_777_034_096), (2026, 4, 24, 12, 34, 56));
    }

    #[test]
    fn epoch_to_ymdhms_leap_day() {
        // 2024-02-29T00:00:00Z = 1709164800
        assert_eq!(epoch_to_ymdhms(1_709_164_800), (2024, 2, 29, 0, 0, 0));
    }

    #[test]
    fn versioned_key_with_extension() {
        assert_eq!(
            versioned_key("geofeed.csv", "2026-05-05T12:34:56Z"),
            "geofeed-2026-05-05T12-34-56Z.csv"
        );
    }

    #[test]
    fn versioned_key_without_extension() {
        assert_eq!(
            versioned_key("geofeed", "2026-05-05T12:34:56Z"),
            "geofeed-2026-05-05T12-34-56Z"
        );
    }

    #[test]
    fn versioned_key_nested_path() {
        assert_eq!(
            versioned_key("feeds/geofeed.csv", "2026-05-05T00:00:00Z"),
            "feeds/geofeed-2026-05-05T00-00-00Z.csv"
        );
    }

    #[test]
    fn uri_encode_preserves_path_slash() {
        assert_eq!(uri_encode("a/b/c", false), "a/b/c");
        assert_eq!(uri_encode("a/b/c", true), "a%2Fb%2Fc");
    }

    #[test]
    fn uri_encode_escapes_spaces_and_special() {
        // SigV4 requires uppercase hex in percent-encoded bytes.
        assert_eq!(uri_encode("hello world", false), "hello%20world");
        assert_eq!(uri_encode("a+b", false), "a%2Bb");
    }
}
