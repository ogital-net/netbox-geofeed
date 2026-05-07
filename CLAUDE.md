# netbox-geofeed

An internal CLI tool, written in Rust, that generates an [RFC 8805] self-published
IP geolocation feed ("geofeed") from prefixes assigned to sites in
[NetBox](https://netboxlabs.com/), and publishes the resulting CSV to an S3
bucket.

[RFC 8805]: https://datatracker.ietf.org/doc/html/rfc8805

---

## 1. Goals

1. Produce a single, valid RFC 8805 geofeed CSV covering every active
   site-assigned prefix in NetBox.
2. Resolve each site's geolocation deterministically via NetBox custom fields,
   normalizing/geocoding addresses through the
   [`arcgis-geocoder`](https://github.com/ogital-net/arcgis-geocoder) crate
   when needed.
3. Publish the feed to an S3 bucket so downstream consumers (RIRs, peers,
   geo-IP providers) can fetch a stable URL.
4. Be safe to run unattended on a cron/systemd timer: deterministic output,
   non-zero exit on hard failures, structured logs, no surprises.

## 2. Non-Goals

- Not a signed-geofeed (RFC 9092) producer in v1. Detached-signature support
  may be added later.
- Not a general-purpose geocoder. Geocoding is a side effect used solely to
  normalize site addresses into country/region/city/coordinate values that
  are then persisted back to NetBox.
- Not a daemon. One-shot CLI invocation per run.
- Not a postal-code publisher. Per current geofeed best practice, the
  postal-code column in the RFC 8805 output is always left empty, and no
  postal-code custom field is read or written.

### 2.1 NetBox write behavior

The tool **does** write to NetBox, but only conservatively:

- For each site evaluated, if any of `geofeed_country`, `geofeed_region`,
  `geofeed_city`, or the site's built-in `latitude` / `longitude` fields are
  empty, the tool geocodes the site's `physical_address` via
  `arcgis-geocoder` and PATCHes the missing fields back onto the site.
- If a field is **already populated** in NetBox, that value is trusted
  verbatim — the tool never overwrites existing data, even if geocoding
  would yield a different answer. Operator-curated values win.
- Writes are gated by `--no-write` (dry-run for NetBox mutations,
  independent of `--dry-run` for the S3 upload) so cron runs can be made
  fully read-only when desired.

## 3. Output Format (RFC 8805)

Each non-comment line is:

```
<prefix>,<ISO-3166-2 country/subdivision>,<city>,<postal-code>,<extra>
```

Conventions for this tool:

- `prefix` — CIDR exactly as represented in NetBox (`prefix` field on
  `ipam.prefix`).
- ISO field — `<ISO 3166-1 alpha-2>` when only the country is known, or
  `<alpha-2>-<subdivision>` (ISO 3166-2) when a region is available.
- `city` — UTF-8 city name; empty string when unknown.
- `postal-code` — **always empty.** Per current geofeed best practice we
  do not publish postal codes, and no NetBox field backs this column.
- `extra` — left empty in v1.

The file begins with comment lines (prefixed `#`) recording:

- Tool name + version + git SHA
- A pointer to RFC 8805
- Last-updated timestamp (UTC, RFC 3339)
- Total record count and a SHA-256 of the comment-stripped body

The source NetBox URL is **never** published in the feed — it is
considered private operational data. The body checksum lets downstream
consumers detect tampering or transport corruption.

## 4. NetBox Data Model Assumptions

### 4.1 Site fields

Custom fields (must be added in NetBox):

| Custom field name  | Type   | Required | Notes                                       |
| ------------------ | ------ | -------- | ------------------------------------------- |
| `geofeed_country`  | text   | yes      | ISO 3166-1 alpha-2 (e.g. `US`, `DE`)        |
| `geofeed_region`   | text   | no       | ISO 3166-2 subdivision code (e.g. `CA`)     |
| `geofeed_city`     | text   | no       | UTF-8 city name                             |

Built-in site fields also consulted/populated:

| NetBox field        | Notes                                                          |
| ------------------- | -------------------------------------------------------------- |
| `physical_address`  | Source string fed to the geocoder when fields are missing.     |
| `latitude`          | Decimal degrees, written from geocoder result if empty.        |
| `longitude`         | Decimal degrees, written from geocoder result if empty.        |

Write policy (see §2.1): empty fields are filled in from geocoder output;
populated fields are never modified. Both the `generate` and `geocode`
subcommands apply this policy — `generate` does it inline as a side effect
of producing the feed, `geocode` does only the geocode + write step
without producing CSV.

### 4.2 Prefix selection

Two NetBox object types contribute records to the feed:

**Site-assigned prefixes (`ipam.prefix`).** Include iff **all** of:

- `prefix.status == "active"`
- `prefix.scope_type == "dcim.site"` and `prefix.scope_id` resolves to a site
  (NetBox 4.x replaced the `site` FK with the generic `scope_*` pair; the
  client must filter accordingly).
- The site has a non-empty `geofeed_country` custom field.
- The address family is IPv4 or IPv6 (both included; no separate flag).

Full country/region/city is emitted from the site's geofeed fields.

**Aggregates (`ipam.aggregate`).** Include every aggregate, regardless of
RIR or status. Only the country column is emitted; region, city, postal
code, and extra are all left empty. Country resolution:

- v1 has no aggregate-level custom fields — every aggregate is emitted
  with the country given by `--aggregate-country` (default `US`).
- A future version may read a `geofeed_country` custom field on the
  aggregate itself and fall back to the CLI default; the column layout will
  not change.

Site-assigned prefixes that fall *inside* an aggregate are still emitted
separately — RFC 8805 consumers handle the longest-prefix-match.

Skipped prefixes are logged at `WARN` with the reason. The CLI exits
non-zero only if the skip rate exceeds a configurable threshold (see §7).

## 5. External Dependencies

| Crate                                                                    | Purpose                                                  |
| ------------------------------------------------------------------------ | -------------------------------------------------------- |
| [`netbox-client`](https://github.com/ogital-net/netbox-client)           | Async NetBox REST client; auto-paginated streams         |
| [`arcgis-geocoder`](https://github.com/ogital-net/arcgis-geocoder)       | ArcGIS World Geocoding for `geocode` subcommand          |
| [`reqwest`](https://crates.io/crates/reqwest) (rustls)                   | HTTP client for the in-tree S3 `PutObject` signer        |
| [`aws-lc-rs`](https://crates.io/crates/aws-lc-rs)                        | SHA-256 / HMAC primitives for SigV4 signing              |
| [`getopt-iter`](https://crates.io/crates/getopt-iter)                    | POSIX-style argument parsing (short + long opts)         |
| `dotenvy`                                                                | Load `.env` at startup                                   |
| `tokio` (`rt`, `macros`, `current_thread`)                               | Async runtime                                            |
| `log` + `env_logger`                                                     | Structured logging to stderr                             |
| `anyhow` (binary) / `thiserror` (any internal lib modules)               | Error handling                                           |
| `futures-util`                                                           | Stream combinators for paginated lists                   |
| `csv`                                                                    | RFC 4180-compliant writer for the geofeed body           |
| [`sonic-rs`](https://crates.io/crates/sonic-rs)                          | Fast JSON for NetBox / ArcGIS responses                  |
| [`json-ts`](https://github.com/ogital-net/json-ts)                       | JSON typestate helpers used by the API clients           |

S3 access is implemented in-tree (`src/s3.rs`) rather than through
`aws-sdk-s3` to keep the binary small; SigV4 signing uses `aws-lc-rs`
primitives directly. AWS credentials are read from the standard
`AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` / `AWS_SESSION_TOKEN`
environment variables only — the shared-credentials file and IMDS chains
are not consulted.

The crate is a binary only (`src/main.rs` + supporting modules). No library
target. `build.rs` bakes the current git SHA into the binary for the
geofeed comment header.

## 6. CLI Surface

Top-level binary: `netbox-geofeed`.

```
netbox-geofeed [GLOBAL FLAGS] <SUBCOMMAND>
```

### 6.1 Global flags

| Flag                  | Env var               | Default | Notes                                                  |
| --------------------- | --------------------- | ------- | ------------------------------------------------------ |
| `--netbox-url <URL>`  | `NETBOX_URL`          | —       | Required. Base URL, no trailing `/api`.                |
| `--netbox-token <T>`  | `NETBOX_TOKEN`        | —       | Required. NetBox API token.                            |
| `--log-level <LEVEL>` | `RUST_LOG`            | `info`  | `env_logger` filter string (e.g. `info,hyper=warn`).   |
| `-v`, `--verbose`     | —                     | —       | Repeatable: `-v` ⇒ debug, `-vv` ⇒ trace. Overridden by `--log-level` / `RUST_LOG`. |

`.env` is loaded automatically from the working directory at startup via
`dotenvy::dotenv()`. Precedence: `.env` < real environment variables < CLI
flags.

### 6.2 Subcommands

**`generate`** — produce the geofeed.

| Flag                           | Env var               | Default       | Notes                                                           |
| ------------------------------ | --------------------- | ------------- | --------------------------------------------------------------- |
| `--dry-run`                    | —                     | false         | Write the feed to **stdout** instead of S3. No AWS calls made.  |
| `--s3-bucket <NAME>`           | `S3_BUCKET`           | —             | Required unless `--dry-run`.                                    |
| `--s3-key <KEY>`               | `S3_KEY`              | `geofeed.csv` | Object key.                                                     |
| `--s3-region <REGION>`         | `AWS_REGION`          | SDK default   | Passed to `aws-config`.                                         |
| `--max-skip-pct <FLOAT>`       | —                     | `5.0`         | Fail (exit 2) if more than this % of candidate prefixes were skipped. |
| `--aggregate-country <CC>`     | `AGGREGATE_COUNTRY`   | `US`          | ISO 3166-1 alpha-2 used for every `ipam.aggregate` record.      |
| `--versioned-mirror`           | `VERSIONED_MIRROR`    | false         | Also upload to `<key-stem>-<UTC-timestamp>.csv` for audit history. Ignored under `--dry-run`. |

**`init-netbox`** — create the three required custom fields on `dcim.site`.

| Flag         | Default | Notes                                                     |
| ------------ | ------- | --------------------------------------------------------- |
| `--no-write` | false   | Print proposed fields without creating them.              |

For each of `geofeed_country`, `geofeed_region`, `geofeed_city`: checks whether
the field already exists (by name + `object_type=dcim.site`); if not, creates it
(type `text`). Logs INFO for each field created or already present. Exits 0 if
all fields are present or successfully created; exits 1 on any API error.

**`geocode`** — backfill missing site geo fields in NetBox.

For each site missing one or more of `geofeed_country`, `geofeed_region`,
`geofeed_city`, `latitude`, `longitude`:

1. Look up the site's `physical_address`.
2. Call `arcgis-geocoder`'s `find_address_candidates`.
3. PATCH the site with values for the empty fields only. Existing values
   are preserved (see §2.1).
4. Log the before/after for each site at INFO.

| Flag                            | Env var                | Default | Notes                                          |
| ------------------------------- | ---------------------- | ------- | ---------------------------------------------- |
| `--arcgis-client-id <ID>`       | `ARCGIS_CLIENT_ID`     | —       | Required. ArcGIS OAuth app client ID.          |
| `--arcgis-client-secret <S>`    | `ARCGIS_CLIENT_SECRET` | —       | Required. ArcGIS OAuth app client secret.      |
| `--site <SLUG>`                 | —                      | —       | Repeatable; restrict to specific site slugs.   |
| `--no-write`                    | —                      | false   | Print proposed PATCH bodies to stderr instead of writing. |
| `--min-score <FLOAT>`           | —                      | `85.0`  | Skip candidates with ArcGIS `score` below this threshold. |

Exit codes:

| Code | Meaning                                                                  |
| ---- | ------------------------------------------------------------------------ |
| 0    | Success                                                                  |
| 1    | Generic / unexpected error (network, AWS, panic-equivalent)              |
| 2    | Skip threshold exceeded — feed was generated but not uploaded            |
| 3    | Configuration error (missing required flag, invalid URL, etc.)           |

## 7. Operational Behavior

- **Determinism.** Output is sorted: IPv4 prefixes first (numeric), then
  IPv6 (numeric). Aggregates and site prefixes are interleaved by numeric
  order; the secondary sort key is the source object's slug/name (site
  slug for site prefixes, aggregate `prefix` for aggregates) so output is
  stable across runs. Same input ⇒ byte-identical output.
- **Atomicity.** S3 `PutObject` is itself atomic at the object level —
  clients never observe a partially-written object — so the primary upload
  is a single signed `PutObject` to the final key (no temp-key /
  `CopyObject` dance). The optional versioned mirror (`--versioned-mirror`)
  is uploaded as a separate `PutObject` after the primary; failures to
  write the mirror are logged at WARN but do not fail the run.
- **Content-Type.** Uploaded with `text/csv; charset=utf-8` and
  `Cache-Control: max-age=300, public`.
- **Concurrency.** NetBox prefix listing uses the streaming
  (`BoxStream`) helper from `netbox-client`. Site lookups are memoized in a
  per-run `HashMap<i64, Site>` to avoid re-fetching.
- **Timeouts.** 30s default per HTTP request to NetBox / ArcGIS / S3,
  configurable later if needed.

## 8. Logging & Observability

- All logs via the `log` facade, rendered to stderr by `env_logger`
  (compact format, auto-color when stderr is a TTY, humantime timestamps).
- Stdout is reserved for CSV output in `--dry-run` mode.
- One INFO line at start ("generating geofeed", with NetBox URL and target),
  one INFO line at end with `{records, skipped, duration_ms, bytes}`.
- Each skipped prefix logs WARN with `prefix`, `site`, `reason`.

## 9. Caching (out of scope for v1, noted for design)

ArcGIS geocoding has per-call cost. The `geocode` subcommand should later
gain an on-disk cache keyed by the normalized `physical_address` so reruns
are free. Suggested location: `${XDG_CACHE_HOME:-~/.cache}/netbox-geofeed/geocode.json`.
Not implemented in v1.

## 10. Repository Layout

```
.
├── Cargo.toml
├── build.rs              # captures git SHA into env! for the comment header
├── CLAUDE.md             # this file
├── README.md             # user-facing usage docs
├── .env.example          # documents required env vars; never commit a real .env
├── src/
│   ├── main.rs           # entrypoint, dotenvy load, dispatch
│   ├── cli.rs            # getopt-iter parsing, subcommand dispatch, global config resolution
│   ├── netbox.rs         # thin wrapper around netbox-client (filters, streaming)
│   ├── geofeed.rs        # record model, sorting, CSV serialization, bogon filter
│   ├── generate.rs       # `generate` subcommand orchestration
│   ├── geocode.rs        # `geocode` subcommand orchestration
│   ├── init_netbox.rs    # `init-netbox` subcommand orchestration
│   ├── s3.rs             # in-tree SigV4 PutObject + versioned mirror
│   └── error.rs          # crate Error enum (thiserror) + Result alias
└── tests/
    └── fixtures/         # golden CSV fixtures consumed by unit tests
```

## 11. Testing Strategy

- Unit tests colocated in each module under `#[cfg(test)] mod tests`.
- HTTP-level tests against NetBox use `wiremock` (mirrors the
  `netbox-client` testing convention). No live NetBox required for `cargo
  test`.
- S3 upload tests also use `wiremock` against the in-tree SigV4 client
  (via the endpoint-override hook on `put_object_to`); no real bucket and
  no `aws-sdk-s3` test client required.
- A golden-file test feeds a fixed JSON fixture of NetBox prefixes through
  the generator and asserts the exact CSV output, byte-for-byte, including
  comment header (with timestamp/version stubbed).
- `cargo clippy --all-targets -- -W clippy::pedantic` must pass; suppress
  with `#[allow(...)]` + justification only when readability suffers.

## 12. Build & Run

```sh
cargo build --release
cp .env.example .env       # then edit
./target/release/netbox-geofeed generate --dry-run
./target/release/netbox-geofeed generate --s3-bucket my-geofeed-bucket
./target/release/netbox-geofeed geocode --site nyc1 --site lhr2
```

Required env (typical `.env`):

```
NETBOX_URL=https://netbox.example.com
NETBOX_TOKEN=...
ARCGIS_CLIENT_ID=...        # only needed for `geocode` (or inline geocoding from `generate`)
ARCGIS_CLIENT_SECRET=...    # paired with ARCGIS_CLIENT_ID
AWS_REGION=us-east-1
AWS_ACCESS_KEY_ID=...       # required for S3 upload
AWS_SECRET_ACCESS_KEY=...   # required for S3 upload
AWS_SESSION_TOKEN=...       # optional, for STS / role-chained sessions
S3_BUCKET=example-geofeed
S3_KEY=geofeed.csv
```

## 13. Open Questions

1. Per-prefix or per-aggregate custom-field overrides for entries that
   span multiple geographies (e.g. anycast, multi-region aggregates).
   Deferred; revisit once the v1 aggregate-country default is in
   production and we see how often it's wrong.
2. RFC 9092 detached-signature publishing — when, and with what key
   custody? TBD; will design alongside whichever RIR first requires it.


