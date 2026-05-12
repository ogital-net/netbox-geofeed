//! `generate` subcommand orchestration.
//!
//! Streams active site-scoped prefixes and aggregates from `NetBox`,
//! builds sorted RFC 8805 records, and writes the geofeed to stdout
//! (`--dry-run`) or S3 (normal run — implemented in M6).

use std::io::{self};
use std::time::Instant;

use anyhow::Context as _;
use arcgis_geocoder::{GeocoderClient, OAuthCredentials};
use futures_util::TryStreamExt as _;
use netbox_client::Site;
use sonic_rs::JsonValueTrait as _;

use crate::cli::GenerateArgs;
use crate::geofeed::{self, FeedParams, Record};
use crate::netbox::Netbox;

/// Entry point for the `generate` subcommand.
///
/// # Errors
///
/// Returns an error on `NetBox` API failures, I/O errors, or if the
/// skip threshold is exceeded (which `main` maps to exit code 2).
pub async fn run(args: GenerateArgs) -> anyhow::Result<()> {
    let ts = json_ts::JsonTimestamp::now();
    let mut ts_buf = json_ts::Buffer::new();
    let timestamp = ts_buf.format(ts).to_owned();
    run_impl(&args, io::stdout(), &timestamp).await
}

/// Inner implementation with an injectable writer and timestamp for testability.
#[allow(clippy::too_many_lines)] // orchestration function — splitting would obscure the flow
pub(crate) async fn run_impl<W: io::Write>(
    args: &GenerateArgs,
    mut out: W,
    timestamp: &str,
) -> anyhow::Result<()> {
    let start = Instant::now();

    // Build geocoder when both ArcGIS OAuth credentials are present.
    let geocoder: Option<GeocoderClient> =
        match (&args.arcgis_client_id, &args.arcgis_client_secret) {
            (Some(id), Some(secret)) => Some(
                GeocoderClient::with_oauth_credentials(OAuthCredentials::new(
                    id.clone(),
                    secret.clone(),
                ))
                .context("failed to construct ArcGIS geocoder client")?,
            ),
            _ => None,
        };

    let mut netbox = Netbox::new(&args.global.netbox_url, &args.global.netbox_token)
        .context("failed to initialise NetBox client")?;

    let target: String = if args.dry_run {
        "stdout (dry-run)".to_owned()
    } else {
        format!(
            "s3://{}/{}",
            args.s3_bucket.as_deref().unwrap_or("?"),
            args.s3_key
        )
    };

    log::info!(
        "generating geofeed: netbox_url={} target={target} geocoding_enabled={}",
        args.global.netbox_url,
        geocoder.is_some(),
    );

    // Collect all candidates before mutable site lookups to avoid holding
    // an immutable stream borrow alongside the &mut self site() calls.
    let raw_prefixes = netbox
        .prefixes_active_site_scoped()
        .try_collect::<Vec<_>>()
        .await
        .context("failed to stream active site-scoped prefixes")?;

    let raw_aggregates = netbox
        .aggregates_all()
        .try_collect::<Vec<_>>()
        .await
        .context("failed to stream aggregates")?;

    let raw_total = raw_prefixes.len() + raw_aggregates.len();
    let mut skipped: usize = 0;
    let mut skipped_non_routable: usize = 0;
    let mut records: Vec<Record> = Vec::with_capacity(raw_total);

    // --- Site-assigned prefix records ---

    for prefix in &raw_prefixes {
        let cidr = &prefix.prefix;

        if !geofeed::is_globally_routable(cidr) {
            log::warn!("skipping prefix {cidr}: not globally routable");
            skipped_non_routable += 1;
            continue;
        }

        let Some(site_id) = prefix.scope_id else {
            // Should not happen: the stream already filtered to scope_type=dcim.site,
            // but be defensive.
            log::warn!("skipping prefix {cidr}: missing scope_id on dcim.site-scoped prefix");
            skipped += 1;
            continue;
        };

        // Geocode inline (side effect): fill any empty geo fields on this site.
        // Only runs when an ArcGIS token is configured and --no-write is not set.
        if let Some(gc) = &geocoder
            && !args.no_write
        {
            let site_for_geo = netbox
                .site(site_id)
                .await
                .with_context(|| format!("failed to fetch site {site_id} for geocoding"))?
                .clone(); // clone to release the &mut borrow before passing &netbox below
            match crate::geocode::fill_missing(&site_for_geo, gc, &netbox, args.min_score, false)
                .await
            {
                Ok(Some(updated)) => netbox.update_cached_site(updated),
                Ok(None) => {}
                Err(e) => log::warn!(
                    "inline geocoding failed for site_id={site_id} prefix={cidr}: {e}; continuing with existing site data",
                ),
            }
        }

        let site = netbox
            .site(site_id)
            .await
            .with_context(|| format!("failed to fetch site {site_id} for prefix {cidr}"))?;

        let Some(country) = custom_field_str(site, "geofeed_country") else {
            log::warn!(
                "skipping prefix {cidr} (site={}): geofeed_country custom field is empty or missing",
                site.slug,
            );
            skipped += 1;
            continue;
        };

        let region = custom_field_str(site, "geofeed_region");
        let city = custom_field_str(site, "geofeed_city");

        records.push(Record::from_site_prefix(
            cidr, country, region, city, &site.slug,
        ));
    }

    // --- Aggregate records ---

    for agg in &raw_aggregates {
        let cidr = &agg.prefix;

        if !geofeed::is_globally_routable(cidr) {
            log::warn!("skipping aggregate {cidr}: not globally routable");
            skipped_non_routable += 1;
            continue;
        }

        records.push(Record::from_aggregate(cidr, &args.aggregate_country));
    }

    // --- Sort, serialize, emit ---

    geofeed::sort(&mut records);

    let params = FeedParams {
        timestamp,
        version: env!("CARGO_PKG_VERSION"),
        git_sha: env!("GIT_SHA"),
    };

    // Write to an in-memory buffer first so we can report byte count in the
    // summary log and write atomically to `out`.
    let mut buf = Vec::new();
    geofeed::write_feed(&records, &mut buf, &params).context("failed to serialise geofeed")?;
    let bytes = buf.len();

    if args.dry_run {
        out.write_all(&buf).context("failed to write output")?;
    }

    let elapsed_ms = start.elapsed().as_millis();
    log::info!(
        "geofeed generation complete: records={} skipped={skipped} skipped_non_routable={skipped_non_routable} duration_ms={elapsed_ms} bytes={bytes}",
        records.len(),
    );

    // Skip-threshold check happens *after* writing the dry-run feed but
    // *before* the S3 upload: exit 2 means "generated but not uploaded"
    // (§6.2). In dry-run mode the CSV is already on stdout; in upload
    // mode S3 is skipped entirely on threshold breach.
    // Non-globally-routable prefixes are excluded from both numerator and
    // denominator: they're a property of the input data we never intend to
    // publish, so they shouldn't push the run over the skip threshold.
    let total_candidates = raw_total - skipped_non_routable;
    if total_candidates > 0 {
        #[allow(clippy::cast_precision_loss)] // rough percentage; precision loss is fine
        let skip_pct = (skipped as f64 / total_candidates as f64) * 100.0;
        if skip_pct > args.max_skip_pct {
            return Err(anyhow::Error::new(
                crate::error::Error::SkipThresholdExceeded {
                    skipped,
                    total: total_candidates,
                    pct: skip_pct,
                    limit: args.max_skip_pct,
                },
            ));
        }
    }

    if !args.dry_run {
        let bucket = args
            .s3_bucket
            .as_deref()
            .expect("s3_bucket is Some when dry_run is false; validated by CLI parser");
        let cfg = crate::s3::S3Config::from_env(args.s3_region.as_deref())
            .context("failed to resolve S3 credentials/region")?;
        let client = reqwest::Client::new();

        if args.versioned_mirror {
            let mirror_buf = buf.clone();
            crate::s3::put_object(&client, &cfg, bucket, &args.s3_key, buf)
                .await
                .context("S3 upload failed")?;
            log::info!("geofeed uploaded to s3://{bucket}/{}", args.s3_key);
            if let Err(e) = crate::s3::upload_versioned_mirror(
                &client,
                &cfg,
                bucket,
                &args.s3_key,
                mirror_buf,
                timestamp,
            )
            .await
            {
                log::warn!("versioned mirror upload failed (non-fatal): {e}");
            }
        } else {
            crate::s3::put_object(&client, &cfg, bucket, &args.s3_key, buf)
                .await
                .context("S3 upload failed")?;
            log::info!("geofeed uploaded to s3://{bucket}/{}", args.s3_key);
        }
    }

    Ok(())
}

/// Extract a non-empty `&str` value from a site's custom fields.
fn custom_field_str<'a>(site: &'a Site, field: &str) -> Option<&'a str> {
    site.custom_fields
        .as_ref()?
        .get(field)?
        .as_str()
        .filter(|s| !s.is_empty())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::GlobalConfig;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path, query_param},
    };

    fn make_args(url: &str) -> GenerateArgs {
        GenerateArgs {
            global: GlobalConfig {
                netbox_url: url.to_owned(),
                netbox_token: "test-token".to_owned(),
            },
            dry_run: true,
            s3_bucket: None,
            s3_key: "geofeed.csv".to_owned(),
            s3_region: None,
            max_skip_pct: 50.0, // generous; 1 bogon in 4 candidates = 25%
            aggregate_country: "US".to_owned(),
            versioned_mirror: false,
            no_write: true,
            arcgis_client_id: None,
            arcgis_client_secret: None,
            min_score: 85.0,
        }
    }

    // --- JSON fixture helpers ---

    fn prefix_json(
        id: i64,
        cidr: &str,
        scope_type: Option<&str>,
        scope_id: Option<i64>,
    ) -> sonic_rs::Value {
        sonic_rs::json!({
            "id": id,
            "url": format!("https://nb.example.com/api/ipam/prefixes/{id}/"),
            "display_url": format!("https://nb.example.com/ipam/prefixes/{id}/"),
            "display": cidr,
            "family": {"value": 4, "label": "IPv4"},
            "prefix": cidr,
            "vrf": null,
            "scope_type": scope_type,
            "scope_id": scope_id,
            "scope": null,
            "tenant": null,
            "vlan": null,
            "status": {"value": "active", "label": "Active"},
            "role": null,
            "is_pool": false,
            "mark_utilized": false,
            "description": "",
            "owner": null,
            "comments": "",
            "tags": [],
            "custom_fields": {},
            "created": "2024-01-01T00:00:00Z",
            "last_updated": "2024-01-01T00:00:00Z",
            "children": 0,
            "_depth": 0
        })
    }

    fn site_json(
        id: i64,
        name: &str,
        slug: &str,
        country: &str,
        region: &str,
        city: &str,
    ) -> sonic_rs::Value {
        sonic_rs::json!({
            "id": id,
            "url": format!("https://nb.example.com/api/dcim/sites/{id}/"),
            "display_url": format!("https://nb.example.com/dcim/sites/{id}/"),
            "display": name,
            "name": name,
            "slug": slug,
            "status": {"value": "active", "label": "Active"},
            "region": null,
            "group": null,
            "tenant": null,
            "facility": "",
            "time_zone": null,
            "description": "",
            "physical_address": "",
            "shipping_address": "",
            "latitude": null,
            "longitude": null,
            "owner": null,
            "comments": "",
            "asns": [],
            "tags": [],
            "custom_fields": {
                "geofeed_country": country,
                "geofeed_region": region,
                "geofeed_city": city
            },
            "created": "2024-01-01T00:00:00Z",
            "last_updated": "2024-01-01T00:00:00Z",
            "circuit_count": 0,
            "device_count": 0,
            "prefix_count": 0,
            "rack_count": 0,
            "virtualmachine_count": 0,
            "vlan_count": 0
        })
    }

    fn aggregate_json(id: i64, cidr: &str) -> sonic_rs::Value {
        sonic_rs::json!({
            "id": id,
            "url": format!("https://nb.example.com/api/ipam/aggregates/{id}/"),
            "display_url": format!("https://nb.example.com/ipam/aggregates/{id}/"),
            "display": cidr,
            "family": {"value": 4, "label": "IPv4"},
            "prefix": cidr,
            "rir": {
                "id": 1,
                "url": "https://nb.example.com/api/ipam/rirs/1/",
                "display_url": "https://nb.example.com/ipam/rirs/1/",
                "display": "ARIN",
                "name": "ARIN",
                "slug": "arin",
                "description": "",
                "aggregate_count": 1
            },
            "tenant": null,
            "date_added": null,
            "description": "",
            "owner": null,
            "comments": "",
            "tags": [],
            "custom_fields": {},
            "created": "2024-01-01T00:00:00Z",
            "last_updated": "2024-01-01T00:00:00Z"
        })
    }

    /// Register all the wiremock endpoints for the golden-output test.
    ///
    /// Fixture data:
    /// - Prefix 1.2.3.0/24 → site 1 (nyc1, US-NY, New York)
    /// - Prefix 10.0.0.0/24 → site 1 (bogon → skipped)
    /// - Prefix 5.6.7.0/24 → site 2 (lon1, GB-ENG, London)
    /// - Aggregate 8.0.0.0/8 (globally routable → country = "US")
    ///
    /// Expected sorted output:
    ///   1.2.3.0/24  US-NY  New York
    ///   5.6.7.0/24  GB-ENG London
    ///   8.0.0.0/8   US
    async fn mount_golden_mocks(server: &MockServer) {
        let prefixes_page = sonic_rs::json!({
            "count": 3,
            "next": null,
            "previous": null,
            "results": [
                prefix_json(1, "1.2.3.0/24",   Some("dcim.site"), Some(1)),
                prefix_json(2, "10.0.0.0/24",  Some("dcim.site"), Some(1)), // RFC 1918 subnet
                prefix_json(3, "5.6.7.0/24",   Some("dcim.site"), Some(2)),
            ]
        });
        let aggregates_page = sonic_rs::json!({
            "count": 1,
            "next": null,
            "previous": null,
            "results": [aggregate_json(1, "8.0.0.0/8")]
        });

        Mock::given(method("GET"))
            .and(path("/api/ipam/prefixes/"))
            .and(query_param("status", "active"))
            .and(query_param("offset", "0"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                sonic_rs::to_vec(&prefixes_page).unwrap(),
                "application/json",
            ))
            .mount(server)
            .await;

        Mock::given(method("GET"))
            .and(path("/api/ipam/aggregates/"))
            .and(query_param("offset", "0"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                sonic_rs::to_vec(&aggregates_page).unwrap(),
                "application/json",
            ))
            .mount(server)
            .await;

        Mock::given(method("GET"))
            .and(path("/api/dcim/sites/1/"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(
                    sonic_rs::to_vec(&site_json(1, "New York", "nyc1", "US", "NY", "New York"))
                        .unwrap(),
                    "application/json",
                ),
            )
            .mount(server)
            .await;

        Mock::given(method("GET"))
            .and(path("/api/dcim/sites/2/"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                sonic_rs::to_vec(&site_json(2, "London", "lon1", "GB", "ENG", "London")).unwrap(),
                "application/json",
            ))
            .mount(server)
            .await;
    }

    // ── golden output ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn generate_dry_run_golden_output() {
        let server = MockServer::start().await;
        mount_golden_mocks(&server).await;

        let mut out = Vec::<u8>::new();
        run_impl(&make_args(&server.uri()), &mut out, "2024-01-01T00:00:00Z")
            .await
            .expect("run_impl should succeed");

        let actual = String::from_utf8(out).expect("output must be UTF-8");

        // Build the expected string dynamically: version and SHA come from the
        // build environment; the timestamp and content-checksum are fixed.
        let expected = format!(
            "\
# netbox-geofeed {ver} ({sha})
# Self-published geofeed as defined in datatracker.ietf.org/doc/html/rfc8805
# Last updated (rfc3339): 2024-01-01T00:00:00Z
# Number of records: 3, checksum of the actual content minus comments:
# SHA256 = f57fe3112c1c39a8384644771288b45ae8190d3a9c242589e2b52f3f74a397c3
1.2.3.0/24,US,US-NY,New York,
5.6.7.0/24,GB,GB-ENG,London,
8.0.0.0/8,US,,,
",
            ver = env!("CARGO_PKG_VERSION"),
            sha = env!("GIT_SHA"),
        );

        assert_eq!(actual, expected);
    }

    // ── skip-threshold enforcement ────────────────────────────────────────────

    /// A routable prefix whose site lacks `geofeed_country` is a "real" skip
    /// that counts toward `--max-skip-pct`. Bogon prefixes are excluded from
    /// both the numerator and denominator (they're a property of the input
    /// data we never intend to publish), so mixing one in must not affect
    /// the calculation. The feed is still written to `out` (empty CSV)
    /// before the error is returned.
    #[tokio::test]
    async fn generate_skip_threshold_exceeded() {
        let server = MockServer::start().await;

        // Two prefixes:
        //   - 1.2.3.0/24 → site 1, no geofeed_country → counts as a skip
        //   - 192.168.1.0/24 → bogon, excluded from the ratio entirely
        // Effective skip rate = 1/1 = 100 %.
        let prefixes_page = sonic_rs::json!({
            "count": 2,
            "next": null,
            "previous": null,
            "results": [
                prefix_json(1, "1.2.3.0/24", Some("dcim.site"), Some(1)),
                prefix_json(2, "192.168.1.0/24", Some("dcim.site"), Some(1)),
            ]
        });
        let aggregates_page = sonic_rs::json!({
            "count": 0, "next": null, "previous": null, "results": []
        });
        // Site 1 has empty custom_fields → routable prefix is skipped.
        let site = site_json(1, "Empty Site", "empty", "", "", "");

        Mock::given(method("GET"))
            .and(path("/api/ipam/prefixes/"))
            .and(query_param("status", "active"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                sonic_rs::to_vec(&prefixes_page).unwrap(),
                "application/json",
            ))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/api/ipam/aggregates/"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                sonic_rs::to_vec(&aggregates_page).unwrap(),
                "application/json",
            ))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/api/dcim/sites/1/"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(sonic_rs::to_vec(&site).unwrap(), "application/json"),
            )
            .mount(&server)
            .await;

        let mut tight_args = make_args(&server.uri());
        tight_args.max_skip_pct = 0.0;

        let mut out = Vec::<u8>::new();
        let result = run_impl(&tight_args, &mut out, "2024-01-01T00:00:00Z").await;

        // The feed (empty, 0 records) was still written before the error.
        let csv_text = String::from_utf8(out).unwrap();
        assert!(csv_text.contains("# Number of records: 0"));

        // The error should be a SkipThresholdExceeded.
        let err = result.expect_err("should have failed with skip threshold error");
        assert!(
            err.to_string().contains("skip threshold"),
            "unexpected error: {err}"
        );
    }

    // ── missing geofeed_country skips prefix ──────────────────────────────────

    #[tokio::test]
    async fn prefix_without_geofeed_country_is_skipped() {
        let server = MockServer::start().await;

        let prefixes_page = sonic_rs::json!({
            "count": 1,
            "next": null,
            "previous": null,
            "results": [
                prefix_json(1, "1.2.3.0/24", Some("dcim.site"), Some(1)),
            ]
        });
        let aggregates_page = sonic_rs::json!({
            "count": 0, "next": null, "previous": null, "results": []
        });
        // Site has empty custom_fields — no geofeed_country.
        let site = sonic_rs::json!({
            "id": 1,
            "url": "https://nb.example.com/api/dcim/sites/1/",
            "display_url": "https://nb.example.com/dcim/sites/1/",
            "display": "Empty Site",
            "name": "Empty Site",
            "slug": "empty",
            "status": {"value": "active", "label": "Active"},
            "region": null, "group": null, "tenant": null,
            "facility": "", "time_zone": null, "description": "",
            "physical_address": "", "shipping_address": "",
            "latitude": null, "longitude": null, "owner": null,
            "comments": "", "asns": [], "tags": [],
            "custom_fields": {},
            "created": "2024-01-01T00:00:00Z",
            "last_updated": "2024-01-01T00:00:00Z",
            "circuit_count": 0, "device_count": 0, "prefix_count": 0,
            "rack_count": 0, "virtualmachine_count": 0, "vlan_count": 0
        });

        Mock::given(method("GET"))
            .and(path("/api/ipam/prefixes/"))
            .and(query_param("status", "active"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                sonic_rs::to_vec(&prefixes_page).unwrap(),
                "application/json",
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/ipam/aggregates/"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                sonic_rs::to_vec(&aggregates_page).unwrap(),
                "application/json",
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/dcim/sites/1/"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(sonic_rs::to_vec(&site).unwrap(), "application/json"),
            )
            .mount(&server)
            .await;

        let mut args = make_args(&server.uri());
        args.max_skip_pct = 100.0; // don't trigger threshold

        let mut out = Vec::<u8>::new();
        run_impl(&args, &mut out, "2024-01-01T00:00:00Z")
            .await
            .expect("should succeed despite skip");

        let csv_text = String::from_utf8(out).unwrap();
        assert!(
            csv_text.contains("# Number of records: 0"),
            "expected 0 records, got:\n{csv_text}"
        );
    }
}
