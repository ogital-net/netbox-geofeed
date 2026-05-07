//! Argument parsing and subcommand dispatch.
//!
//! Precedence (lowest → highest): `.env` file < real environment variables < CLI flags.
//!
//! Subcommand is the first non-option positional argument, following POSIX
//! stop-at-first-non-option semantics from `getopt-iter`.

use std::ffi::OsString;

use getopt_iter::Getopt;

use crate::error::{Error, Result};

// ---------------------------------------------------------------------------
// Resolved global config
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct GlobalConfig {
    pub netbox_url: String,
    pub netbox_token: String,
}

// ---------------------------------------------------------------------------
// Subcommand argument structs
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct GenerateArgs {
    pub global: GlobalConfig,
    /// Write CSV to stdout; skip S3 upload entirely.
    pub dry_run: bool,
    pub s3_bucket: Option<String>,
    pub s3_key: String,
    pub s3_region: Option<String>,
    /// Fail (exit 2) when more than this percentage of candidate prefixes are skipped.
    pub max_skip_pct: f64,
    /// ISO 3166-1 alpha-2 country code applied to every `ipam.aggregate` record.
    pub aggregate_country: String,
    /// Also upload to a timestamped key for audit history.
    pub versioned_mirror: bool,
    /// Suppress `NetBox` write-back of geocoded fields (dry-run for mutations).
    pub no_write: bool,
    /// `ArcGIS` OAuth client credentials for inline geocoding of sites missing
    /// geo fields. Read from `ARCGIS_CLIENT_ID` / `ARCGIS_CLIENT_SECRET`;
    /// optional — geocoding is skipped unless **both** are present.
    pub arcgis_client_id: Option<String>,
    pub arcgis_client_secret: Option<String>,
    /// Minimum `ArcGIS` candidate score for inline geocoding (default 85.0).
    pub min_score: f64,
}

#[derive(Debug)]
pub struct GeocodeArgs {
    pub global: GlobalConfig,
    pub arcgis_client_id: String,
    pub arcgis_client_secret: String,
    /// Restrict to these site slugs; empty = all sites.
    pub sites: Vec<String>,
    /// Print proposed PATCH bodies without applying them.
    pub no_write: bool,
    /// Minimum `ArcGIS` candidate score to accept.
    pub min_score: f64,
}

#[derive(Debug)]
pub struct InitNetboxArgs {
    pub global: GlobalConfig,
    /// Print the fields that would be created without actually creating them.
    pub no_write: bool,
}

// ---------------------------------------------------------------------------
// Top-level dispatch
// ---------------------------------------------------------------------------

pub async fn run() -> anyhow::Result<()> {
    // getopt-iter consumes the first non-option arg into a private buffer
    // and then returns None — `remaining()` does NOT yield it. So we can't
    // ask the global parser for the subcommand. Pre-split argv ourselves
    // into (global args ending at the subcommand-exclusive split) +
    // (subcommand and its args).
    let raw_args: Vec<OsString> = std::env::args_os().collect();
    let prog = raw_args
        .first()
        .map(|a| {
            std::path::Path::new(a).file_name().map_or_else(
                || a.to_string_lossy().into_owned(),
                |s| s.to_string_lossy().into_owned(),
            )
        })
        .unwrap_or_default();

    let split = find_subcommand_index(&raw_args);

    // Prefix global slice with argv[0] so Getopt can consume it as the program name.
    let mut global_argv: Vec<OsString> = Vec::with_capacity(split + 1);
    if let Some(prog0) = raw_args.first() {
        global_argv.push(prog0.clone());
    }
    global_argv.extend(raw_args[1..split].iter().cloned());

    let mut opts = Getopt::new(global_argv, global_optstring());
    opts.set_opterr(false);

    let mut netbox_url: Option<String> = std::env::var("NETBOX_URL").ok();
    let mut netbox_token: Option<String> = std::env::var("NETBOX_TOKEN").ok();
    let env_log_level: Option<String> = std::env::var("RUST_LOG").ok();
    let mut cli_log_level: Option<String> = None;
    let mut verbose: u8 = 0;

    for opt in opts.by_ref() {
        match opt.val() {
            'U' => netbox_url = opt.into_arg().map(std::borrow::Cow::into_owned),
            'T' => netbox_token = opt.into_arg().map(std::borrow::Cow::into_owned),
            'l' => cli_log_level = opt.into_arg().map(std::borrow::Cow::into_owned),
            'v' => verbose = verbose.saturating_add(1),
            'h' => {
                print_usage(&prog);
                std::process::exit(0);
            }
            '?' | ':' => {
                let bad = opt.erropt().map(|c| format!("-{c}")).unwrap_or_default();
                return Err(config_err(format!("unknown or incomplete option {bad}")));
            }
            _ => {}
        }
    }

    // Initialise logger after log-level is resolved.
    // Precedence (highest first): explicit `--log-level`, then `-v` repetitions
    // on the CLI, then `RUST_LOG` from the environment, then default "info".
    // CLI flags always beat env vars per the global precedence rule.
    let resolved_filter = if let Some(l) = cli_log_level {
        l
    } else if verbose > 0 {
        match verbose {
            1 => "debug".to_owned(),
            _ => "trace".to_owned(),
        }
    } else {
        env_log_level.unwrap_or_else(|| "info".to_owned())
    };
    init_logger(&resolved_filter);

    // Anything from `split` onwards belongs to the subcommand.
    let sub_slice = &raw_args[split..];
    let subcommand = sub_slice
        .first()
        .map(|s| s.to_string_lossy().into_owned())
        .ok_or_else(|| config_err("no subcommand given (try --help)"))?;

    let global = GlobalConfig {
        netbox_url: netbox_url
            .ok_or_else(|| config_err("--netbox-url / NETBOX_URL is required"))?,
        netbox_token: netbox_token
            .ok_or_else(|| config_err("--netbox-token / NETBOX_TOKEN is required"))?,
    };

    // Subcommand parser expects argv[0] = subcommand name.
    let sub_argv: Vec<String> = sub_slice
        .iter()
        .map(|s| s.to_string_lossy().into_owned())
        .collect();

    match subcommand.as_str() {
        "generate" => {
            let args = parse_generate(global, sub_argv, &prog)?;
            crate::cli::dispatch::generate(args).await
        }
        "geocode" => {
            let args = parse_geocode(global, sub_argv, &prog)?;
            crate::cli::dispatch::geocode(args).await
        }
        "init-netbox" => {
            let args = parse_init_netbox(global, sub_argv, &prog)?;
            crate::cli::dispatch::init_netbox(args).await
        }
        other => Err(config_err(format!(
            "unknown subcommand '{other}' (try --help)"
        ))),
    }
}

/// Scan `argv` (including argv\[0\]) and return the index of the first
/// argument that is the subcommand — i.e. the first non-option that is not
/// itself the value of a known global option-with-argument.
///
/// If no subcommand is present, returns `argv.len()`.
fn find_subcommand_index(argv: &[OsString]) -> usize {
    // Global options that take a value.
    const VALUE_LONG: &[&str] = &[
        "--netbox-url",
        "--netbox-token",
        "--log-level",
        "--env-file",
    ];
    const VALUE_SHORT: &[char] = &['U', 'T', 'l'];
    // Global boolean options (no value).
    const BOOL_LONG: &[&str] = &["--help", "--verbose"];
    const BOOL_SHORT: &[char] = &['h', 'v'];

    let mut i = 1;
    while i < argv.len() {
        let s = argv[i].to_string_lossy();
        let s = s.as_ref();

        if s == "--" {
            // Per POSIX, `--` ends option processing; treat the next arg as the subcommand.
            return (i + 1).min(argv.len());
        }

        // Long option?
        if let Some(rest) = s.strip_prefix("--") {
            // `--name=value` — self-contained.
            let name_only = rest.split('=').next().unwrap_or(rest);
            let full_long = format!("--{name_only}");
            if rest.contains('=') {
                if VALUE_LONG.contains(&full_long.as_str())
                    || BOOL_LONG.contains(&full_long.as_str())
                {
                    i += 1;
                    continue;
                }
                // Not a known global long option → must be a subcommand flag, so the
                // subcommand itself must have been earlier; but we never found one,
                // so treat this as the start (will be diagnosed as unknown later).
                return i;
            }
            if BOOL_LONG.contains(&full_long.as_str()) {
                i += 1;
                continue;
            }
            if VALUE_LONG.contains(&full_long.as_str()) {
                // Value is the next arg.
                i += 2;
                continue;
            }
            // Unknown long option at this position — let global parser report it.
            return i;
        }

        // Short option?
        if let Some(rest) = s.strip_prefix('-') {
            if rest.is_empty() {
                // Bare "-" is not an option; treat as subcommand boundary.
                return i;
            }
            let first = rest.chars().next().unwrap();
            if BOOL_SHORT.contains(&first) {
                // Aggregated bools or unknown trailing chars: just consume the token.
                i += 1;
                continue;
            }
            if VALUE_SHORT.contains(&first) {
                if rest.len() > 1 {
                    // Attached value: -Uvalue
                    i += 1;
                } else {
                    // Separate value: -U value
                    i += 2;
                }
                continue;
            }
            // Unknown short option — let global parser report it.
            return i;
        }

        // Non-option: this is the subcommand.
        return i;
    }
    argv.len()
}

// ---------------------------------------------------------------------------
// Subcommand parsers
// ---------------------------------------------------------------------------

fn parse_generate(global: GlobalConfig, argv: Vec<String>, prog: &str) -> Result<GenerateArgs> {
    let mut opts = Getopt::new(argv, generate_optstring());
    opts.set_opterr(false);

    let mut dry_run = false;
    let mut s3_bucket: Option<String> = std::env::var("S3_BUCKET").ok();
    let mut s3_key: Option<String> = std::env::var("S3_KEY").ok();
    let mut s3_region: Option<String> = std::env::var("AWS_REGION").ok();
    let mut max_skip_pct: f64 = 5.0;
    let mut aggregate_country: Option<String> = std::env::var("AGGREGATE_COUNTRY").ok();
    let mut versioned_mirror: bool =
        std::env::var("VERSIONED_MIRROR").is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));
    let mut no_write = false;
    let arcgis_client_id: Option<String> = std::env::var("ARCGIS_CLIENT_ID").ok();
    let arcgis_client_secret: Option<String> = std::env::var("ARCGIS_CLIENT_SECRET").ok();
    let mut min_score: f64 = 85.0;

    for opt in opts {
        match opt.val() {
            'd' => dry_run = true,
            'b' => s3_bucket = opt.into_arg().map(std::borrow::Cow::into_owned),
            'k' => s3_key = opt.into_arg().map(std::borrow::Cow::into_owned),
            'r' => s3_region = opt.into_arg().map(std::borrow::Cow::into_owned),
            'p' => {
                let raw = opt.into_arg().unwrap_or_default();
                max_skip_pct = raw
                    .parse::<f64>()
                    .map_err(|_| Error::Config(format!("--max-skip-pct: invalid float '{raw}'")))?;
            }
            'c' => aggregate_country = opt.into_arg().map(std::borrow::Cow::into_owned),
            'V' => versioned_mirror = true,
            'n' => no_write = true,
            'm' => {
                let raw = opt.into_arg().unwrap_or_default();
                min_score = raw
                    .parse::<f64>()
                    .map_err(|_| Error::Config(format!("--min-score: invalid float '{raw}'")))?;
            }
            'h' => {
                print_generate_usage(prog);
                std::process::exit(0);
            }
            '?' | ':' => {
                let bad = opt.erropt().map(|c| format!("-{c}")).unwrap_or_default();
                return Err(Error::Config(format!("unknown or incomplete option {bad}")));
            }
            _ => {}
        }
    }

    if !dry_run && s3_bucket.is_none() {
        return Err(Error::Config(
            "--s3-bucket / S3_BUCKET is required unless --dry-run".into(),
        ));
    }

    Ok(GenerateArgs {
        global,
        dry_run,
        s3_bucket,
        s3_key: s3_key.unwrap_or_else(|| "geofeed.csv".into()),
        s3_region,
        max_skip_pct,
        aggregate_country: aggregate_country.unwrap_or_else(|| "US".into()),
        versioned_mirror,
        no_write,
        arcgis_client_id,
        arcgis_client_secret,
        min_score,
    })
}

fn parse_geocode(global: GlobalConfig, argv: Vec<String>, prog: &str) -> Result<GeocodeArgs> {
    let mut opts = Getopt::new(argv, geocode_optstring());
    opts.set_opterr(false);

    let mut arcgis_client_id: Option<String> = std::env::var("ARCGIS_CLIENT_ID").ok();
    let mut arcgis_client_secret: Option<String> = std::env::var("ARCGIS_CLIENT_SECRET").ok();
    let mut sites: Vec<String> = Vec::new();
    let mut no_write = false;
    let mut min_score: f64 = 85.0;

    for opt in opts {
        match opt.val() {
            'i' => arcgis_client_id = opt.into_arg().map(std::borrow::Cow::into_owned),
            'S' => arcgis_client_secret = opt.into_arg().map(std::borrow::Cow::into_owned),
            's' => {
                if let Some(slug) = opt.into_arg() {
                    sites.push(slug.into_owned());
                }
            }
            'n' => no_write = true,
            'm' => {
                let raw = opt.into_arg().unwrap_or_default();
                min_score = raw
                    .parse::<f64>()
                    .map_err(|_| Error::Config(format!("--min-score: invalid float '{raw}'")))?;
            }
            'h' => {
                print_geocode_usage(prog);
                std::process::exit(0);
            }
            '?' | ':' => {
                let bad = opt.erropt().map(|c| format!("-{c}")).unwrap_or_default();
                return Err(Error::Config(format!("unknown or incomplete option {bad}")));
            }
            _ => {}
        }
    }

    Ok(GeocodeArgs {
        global,
        arcgis_client_id: arcgis_client_id.ok_or_else(|| {
            Error::Config("--arcgis-client-id / ARCGIS_CLIENT_ID is required".into())
        })?,
        arcgis_client_secret: arcgis_client_secret.ok_or_else(|| {
            Error::Config("--arcgis-client-secret / ARCGIS_CLIENT_SECRET is required".into())
        })?,
        sites,
        no_write,
        min_score,
    })
}

fn parse_init_netbox(
    global: GlobalConfig,
    argv: Vec<String>,
    prog: &str,
) -> Result<InitNetboxArgs> {
    let mut opts = Getopt::new(argv, init_netbox_optstring());
    opts.set_opterr(false);

    let mut no_write = false;

    for opt in opts {
        match opt.val() {
            'n' => no_write = true,
            'h' => {
                print_init_netbox_usage(prog);
                std::process::exit(0);
            }
            '?' | ':' => {
                let bad = opt.erropt().map(|c| format!("-{c}")).unwrap_or_default();
                return Err(Error::Config(format!("unknown or incomplete option {bad}")));
            }
            _ => {}
        }
    }

    Ok(InitNetboxArgs { global, no_write })
}

// ---------------------------------------------------------------------------
// Optstrings
// ---------------------------------------------------------------------------

fn global_optstring() -> &'static str {
    // -U/--netbox-url, -T/--netbox-token, -l/--log-level, -v/--verbose, -h/--help
    "U:(netbox-url)T:(netbox-token)l:(log-level)v(verbose)h(help)"
}

fn generate_optstring() -> &'static str {
    // -d/--dry-run, -b/--s3-bucket, -k/--s3-key, -r/--s3-region,
    // -p/--max-skip-pct, -c/--aggregate-country, -V/--versioned-mirror,
    // -n/--no-write, -m/--min-score, -h/--help
    "d(dry-run)b:(s3-bucket)k:(s3-key)r:(s3-region)p:(max-skip-pct)c:(aggregate-country)V(versioned-mirror)n(no-write)m:(min-score)h(help)"
}

fn geocode_optstring() -> &'static str {
    // -i/--arcgis-client-id, -S/--arcgis-client-secret, -s/--site,
    // -n/--no-write, -m/--min-score, -h/--help
    "i:(arcgis-client-id)S:(arcgis-client-secret)s:(site)n(no-write)m:(min-score)h(help)"
}

fn init_netbox_optstring() -> &'static str {
    // -n/--no-write, -h/--help
    "n(no-write)h(help)"
}

// ---------------------------------------------------------------------------
// Subcommand dispatch stubs (filled in by later milestones)
// ---------------------------------------------------------------------------

mod dispatch {
    use super::{GenerateArgs, GeocodeArgs, InitNetboxArgs};

    pub async fn generate(args: GenerateArgs) -> anyhow::Result<()> {
        crate::generate::run(args).await
    }

    pub async fn geocode(args: GeocodeArgs) -> anyhow::Result<()> {
        crate::geocode::run(args).await
    }

    pub async fn init_netbox(args: InitNetboxArgs) -> anyhow::Result<()> {
        crate::init_netbox::run(args).await
    }
}

// ---------------------------------------------------------------------------
// Usage blocks
// ---------------------------------------------------------------------------

fn print_usage(prog: &str) {
    print!(
        "\
Usage: {prog} [GLOBAL OPTIONS] <SUBCOMMAND> [SUBCOMMAND OPTIONS]

Global options:
  -U, --netbox-url <URL>      NetBox base URL (env: NETBOX_URL)
  -T, --netbox-token <TOKEN>  NetBox API token (env: NETBOX_TOKEN)
  -l, --log-level <LEVEL>     Log level filter (env: RUST_LOG) [default: info]
  -v, --verbose               Increase verbosity (-v=debug, -vv=trace).
                              Overridden by -l/--log-level / RUST_LOG.
  -h, --help                  Show this help and exit

Subcommands:
  generate      Produce an RFC 8805 geofeed and upload to S3
  geocode       Backfill missing site geo fields in NetBox via ArcGIS
  init-netbox   Create required custom fields in NetBox

Run '{prog} <SUBCOMMAND> --help' for subcommand-specific options.
",
    );
}

fn print_generate_usage(prog: &str) {
    print!(
        "\
Usage: {prog} [GLOBAL OPTIONS] generate [OPTIONS]

Options:
  -d, --dry-run                    Write CSV to stdout; skip S3 upload
  -b, --s3-bucket <NAME>           S3 bucket name (env: S3_BUCKET) [required unless --dry-run]
  -k, --s3-key <KEY>               S3 object key (env: S3_KEY) [default: geofeed.csv]
  -r, --s3-region <REGION>         AWS region (env: AWS_REGION)
  -p, --max-skip-pct <FLOAT>       Max skipped-prefix % before exit 2 [default: 5.0]
  -c, --aggregate-country <CC>     Country code for aggregates (env: AGGREGATE_COUNTRY) [default: US]
  -V, --versioned-mirror           Also upload to a timestamped key (env: VERSIONED_MIRROR)
  -n, --no-write                   Suppress NetBox geo-field write-back
  -m, --min-score <FLOAT>          Minimum ArcGIS score for inline geocoding [default: 85.0]
  -h, --help                       Show this help and exit

Inline geocoding is enabled when ARCGIS_CLIENT_ID and ARCGIS_CLIENT_SECRET
are both set in the environment.
",
    );
}

fn print_geocode_usage(prog: &str) {
    print!(
        "\
Usage: {prog} [GLOBAL OPTIONS] geocode [OPTIONS]

Options:
  -i, --arcgis-client-id <ID>      ArcGIS OAuth client ID (env: ARCGIS_CLIENT_ID) [required]
  -S, --arcgis-client-secret <S>   ArcGIS OAuth client secret (env: ARCGIS_CLIENT_SECRET) [required]
  -s, --site <SLUG>            Restrict to this site slug (repeatable)
  -n, --no-write               Print proposed PATCHes without applying them
  -m, --min-score <FLOAT>      Minimum ArcGIS candidate score [default: 85.0]
  -h, --help                   Show this help and exit
",
    );
}

fn print_init_netbox_usage(prog: &str) {
    print!(
        "\
Usage: {prog} [GLOBAL OPTIONS] init-netbox [OPTIONS]

Creates the three geofeed custom fields (geofeed_country, geofeed_region,
geofeed_city) on dcim.site in NetBox. Skips any field that already exists.

Options:
  -n, --no-write   Print what would be created without making any changes
  -h, --help       Show this help and exit
",
    );
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn config_err(msg: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(Error::Config(msg.into()))
}

fn init_logger(filter: &str) {
    use std::io::Write as _;
    env_logger::Builder::new()
        .parse_filters(filter)
        .target(env_logger::Target::Stderr)
        .format(|buf, record| {
            let level_style = buf.default_level_style(record.level());
            writeln!(
                buf,
                "{level_style}{:>5}{level_style:#} {}",
                record.level(),
                record.args(),
            )
        })
        .init();
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn gc(url: &str, token: &str) -> GlobalConfig {
        GlobalConfig {
            netbox_url: url.into(),
            netbox_token: token.into(),
        }
    }

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(std::string::ToString::to_string).collect()
    }

    // --- generate ---

    #[test]
    fn generate_dry_run_no_bucket_required() {
        let args = parse_generate(
            gc("http://nb", "tok"),
            argv(&["generate", "--dry-run"]),
            "netbox-geofeed",
        )
        .unwrap();
        assert!(args.dry_run);
        assert!(args.s3_bucket.is_none());
    }

    #[test]
    fn generate_bucket_required_without_dry_run() {
        let err = parse_generate(
            gc("http://nb", "tok"),
            argv(&["generate"]),
            "netbox-geofeed",
        )
        .unwrap_err();
        assert!(err.to_string().contains("s3-bucket"));
    }

    #[test]
    fn generate_defaults() {
        let args = parse_generate(
            gc("http://nb", "tok"),
            argv(&["generate", "--dry-run"]),
            "netbox-geofeed",
        )
        .unwrap();
        assert_eq!(args.s3_key, "geofeed.csv");
        assert!((args.max_skip_pct - 5.0).abs() < f64::EPSILON);
        assert_eq!(args.aggregate_country, "US");
        assert!(!args.versioned_mirror);
        assert!(!args.no_write);
    }

    #[test]
    fn generate_long_flags() {
        let args = parse_generate(
            gc("http://nb", "tok"),
            argv(&[
                "generate",
                "--s3-bucket",
                "my-bucket",
                "--s3-key",
                "feed.csv",
                "--aggregate-country",
                "DE",
                "--versioned-mirror",
                "--no-write",
                "--max-skip-pct",
                "10.5",
            ]),
            "netbox-geofeed",
        )
        .unwrap();
        assert_eq!(args.s3_bucket.as_deref(), Some("my-bucket"));
        assert_eq!(args.s3_key, "feed.csv");
        assert_eq!(args.aggregate_country, "DE");
        assert!(args.versioned_mirror);
        assert!(args.no_write);
        assert!((args.max_skip_pct - 10.5).abs() < f64::EPSILON);
    }

    #[test]
    fn generate_invalid_max_skip_pct() {
        let err = parse_generate(
            gc("http://nb", "tok"),
            argv(&["generate", "--dry-run", "--max-skip-pct", "abc"]),
            "netbox-geofeed",
        )
        .unwrap_err();
        assert!(err.to_string().contains("max-skip-pct"));
    }

    // --- geocode ---

    #[test]
    fn geocode_credentials_required() {
        let err = parse_geocode(gc("http://nb", "tok"), argv(&["geocode"]), "netbox-geofeed");
        // If the env vars are set, parsing succeeds — skip assertion.
        if std::env::var("ARCGIS_CLIENT_ID").is_err()
            && std::env::var("ARCGIS_CLIENT_SECRET").is_err()
        {
            assert!(err.unwrap_err().to_string().contains("arcgis-client"));
        }
    }

    #[test]
    fn geocode_long_flags() {
        let args = parse_geocode(
            gc("http://nb", "tok"),
            argv(&[
                "geocode",
                "--arcgis-client-id",
                "my-id",
                "--arcgis-client-secret",
                "my-secret",
                "--site",
                "nyc1",
                "--site",
                "lhr2",
                "--no-write",
                "--min-score",
                "90.0",
            ]),
            "netbox-geofeed",
        )
        .unwrap();
        assert_eq!(args.arcgis_client_id, "my-id");
        assert_eq!(args.arcgis_client_secret, "my-secret");
        assert_eq!(args.sites, vec!["nyc1", "lhr2"]);
        assert!(args.no_write);
        assert!((args.min_score - 90.0).abs() < f64::EPSILON);
    }

    #[test]
    fn geocode_defaults() {
        let args = parse_geocode(
            gc("http://nb", "tok"),
            argv(&[
                "geocode",
                "--arcgis-client-id",
                "id",
                "--arcgis-client-secret",
                "sec",
            ]),
            "netbox-geofeed",
        )
        .unwrap();
        assert!(args.sites.is_empty());
        assert!(!args.no_write);
        assert!((args.min_score - 85.0).abs() < f64::EPSILON);
    }
}
