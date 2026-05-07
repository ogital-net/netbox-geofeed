# netbox-geofeed

Generates a [RFC 8805] self-published IP-geolocation feed ("geofeed") from
prefixes assigned to sites in [NetBox][netbox] and uploads the resulting
CSV to S3.

[RFC 8805]: https://datatracker.ietf.org/doc/html/rfc8805
[netbox]: https://netboxlabs.com/

---

## Quickstart

```sh
# Build
cargo build --release

# Configure
cp .env.example .env
$EDITOR .env

# One-time: create the required custom fields on dcim.site in NetBox
./target/release/netbox-geofeed init-netbox

# Backfill missing geo fields on a few sites (writes to NetBox)
./target/release/netbox-geofeed geocode --site nyc1 --site lhr2

# Generate the feed and print to stdout (no S3, no NetBox writes)
./target/release/netbox-geofeed generate --dry-run --no-write

# Generate the feed and upload to S3
./target/release/netbox-geofeed generate --s3-bucket my-geofeed-bucket
```

## Subcommands

| Subcommand    | Purpose                                                     |
| ------------- | ----------------------------------------------------------- |
| `generate`    | Produce the geofeed CSV and upload to S3 (or print to stdout under `--dry-run`). Optionally geocodes sites with empty geo fields inline. |
| `geocode`     | Backfill empty `geofeed_country` / `geofeed_region` / `geofeed_city` / `latitude` / `longitude` fields on sites via ArcGIS. Never overwrites populated values. |
| `init-netbox` | Create the three required custom fields (`geofeed_country`, `geofeed_region`, `geofeed_city`) on `dcim.site` in NetBox. |

Run `netbox-geofeed <SUBCOMMAND> --help` for per-subcommand options.

## Environment variables

`.env` is loaded automatically (precedence: `.env` < real env vars < CLI flags).

| Variable             | Required for       | Notes                                                                 |
| -------------------- | ------------------ | --------------------------------------------------------------------- |
| `NETBOX_URL`         | all                | Base URL, no trailing `/api`                                          |
| `NETBOX_TOKEN`       | all                | NetBox API token                                                      |
| `ARCGIS_CLIENT_ID`   | `geocode`; optional for `generate` | ArcGIS OAuth app client ID                                          |
| `ARCGIS_CLIENT_SECRET` | `geocode`; optional for `generate` | ArcGIS OAuth app client secret. Inline geocoding in `generate` runs only when both ID and secret are set. |
| `AWS_REGION`         | `generate` (S3)    | Falls back to standard AWS SDK resolution when unset                  |
| `S3_BUCKET`          | `generate` (S3)    | Required unless `--dry-run`                                           |
| `S3_KEY`             | optional           | Default: `geofeed.csv`                                                |
| `AGGREGATE_COUNTRY`  | optional           | ISO 3166-1 alpha-2 used for every aggregate; default `US`             |
| `VERSIONED_MIRROR`   | optional           | `1`/`true` to also write a timestamped object alongside the primary key |
| `RUST_LOG`           | optional           | `env_logger` filter string; default `info` (override with `-v`/`-vv` or `--log-level`) |

AWS credentials are read from the standard environment variables only.
The shared-credentials file (`~/.aws/credentials`) and EC2 / ECS instance
metadata chains are **not** consulted — set the env vars explicitly
(typically via `.env` or a systemd `EnvironmentFile`):

| Variable                | Notes                                                       |
| ----------------------- | ----------------------------------------------------------- |
| `AWS_ACCESS_KEY_ID`     | Required for S3 upload                                      |
| `AWS_SECRET_ACCESS_KEY` | Required for S3 upload                                      |
| `AWS_SESSION_TOKEN`     | Optional; for STS / role-chained / temporary credentials    |
| `AWS_REGION`            | Required unless `--s3-region` is passed                     |

`--s3-region` overrides `AWS_REGION` when both are set.

The IAM principal needs `s3:PutObject` on the target key (and on the
mirror prefix when `--versioned-mirror` is in use):

A minimal bucket-scoped policy:

```json
{
  "Version": "2012-10-17",
  "Statement": [{
    "Effect": "Allow",
    "Action": ["s3:PutObject"],
    "Resource": "arn:aws:s3:::example-geofeed/*"
  }]
}
```

## Output format (RFC 8805)

```
# netbox-geofeed 0.1.0 (abc1234)
# Self-published geofeed as defined in datatracker.ietf.org/doc/html/rfc8805
# Last updated (rfc3339): 2026-05-05T12:34:56Z
# Number of records: 4, checksum of the actual content minus comments:
# SHA256 = a0945cedccf24286fc9aa85efb53ec8fca58dfb8eef091c504c18c899dce8a3c
1.2.3.0/24,US-NY,New York,,
5.6.7.0/24,GB-ENG,London,,
2001:db8::/32,DE-BE,Berlin,,
8.0.0.0/8,US,,,
```

- `prefix,ISO,city,postal,extra` (postal and extra are always empty).
- IPv4 first, then IPv6, both in numeric order. Output is byte-stable
  given identical input.
- The header omits the source NetBox URL (private operational data) and
  publishes a SHA-256 of the body so consumers can detect tampering or
  transport corruption.
- Aggregates contribute country-only rows. Site-assigned prefixes contribute
  full country/region/city based on the site's `geofeed_*` custom fields.
- Bogon and special-use prefixes (RFC 1918, RFC 5737, loopback, ULA, etc.)
  are filtered out and logged at WARN.

## Exit codes

| Code | Meaning                                                          |
| ---- | ---------------------------------------------------------------- |
| 0    | Success                                                          |
| 1    | Generic / unexpected error (network, AWS, etc.)                  |
| 2    | Skip threshold exceeded — feed generated but not uploaded        |
| 3    | Configuration error (missing required flag, etc.)                |

## S3 upload behaviour

The primary upload is a single signed `PutObject` to the final key. S3
`PutObject` is itself atomic at the object level — consumers always see
either the previous geofeed or the new one, never a partial write — so no
temp-key / `CopyObject` dance is performed.

Object metadata: `Content-Type: text/csv; charset=utf-8`,
`Cache-Control: max-age=300, public`.

With `--versioned-mirror`, a second object is written to
`<key-stem>-<UTC-timestamp><ext>` for audit history. Mirror failures are
logged but do not fail the run.

## NetBox write policy

- `geocode` and `generate` only fill empty fields. Operator-curated values
  are never overwritten.
- Both subcommands accept `--no-write` to suppress all NetBox mutations
  (independent of `--dry-run`, which only controls the S3 upload).

## Cron example

```cron
# Refresh the geofeed every 6 hours.
0 */6 * * *  /usr/local/bin/netbox-geofeed generate --versioned-mirror >> /var/log/netbox-geofeed.log 2>&1
```

A systemd timer is recommended over cron for production deployments so
that exit codes are surfaced to the journal:

```ini
# /etc/systemd/system/netbox-geofeed.service
[Unit]
Description=Generate and publish geofeed
After=network-online.target

[Service]
Type=oneshot
EnvironmentFile=/etc/netbox-geofeed.env
ExecStart=/usr/local/bin/netbox-geofeed generate --versioned-mirror
```

```ini
# /etc/systemd/system/netbox-geofeed.timer
[Unit]
Description=Run netbox-geofeed every 6 hours

[Timer]
OnBootSec=5min
OnUnitActiveSec=6h
Persistent=true

[Install]
WantedBy=timers.target
```

## Development

```sh
cargo build
cargo test
cargo clippy --all-targets -- -W clippy::pedantic
```

Tests do not require a live NetBox or S3 bucket; HTTP-level fixtures use
`wiremock` for both NetBox and S3 (the in-tree SigV4 client accepts an
endpoint override for testing).
