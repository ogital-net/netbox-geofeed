//! RFC 8805 geofeed record model, sorting, and CSV serialization.
//!
//! Two kinds of row appear in the feed:
//!
//! - [`Record::from_site_prefix`] — full country/region/city from a
//!   `NetBox` site-assigned prefix.
//! - [`Record::from_aggregate`] — country-only for an `ipam.aggregate`.
//!
//! # Sorting (§7)
//!
//! Records are sorted: IPv4 before IPv6, then numerically by network address.
//! Within the same address the secondary key is the site slug (for site
//! prefixes) or the aggregate CIDR string (for aggregates), keeping output
//! stable across runs with identical input.

use std::borrow::Cow;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

// ---------------------------------------------------------------------------
// Record
// ---------------------------------------------------------------------------

/// A single line in the RFC 8805 geofeed CSV.
#[derive(Debug)]
pub struct Record<'a> {
    /// IP prefix in CIDR notation, exactly as stored in `NetBox`.
    pub prefix: &'a str,
    /// ISO 3166-1 alpha-2 country code (e.g. `US`).
    pub country: &'a str,
    /// ISO 3166-2 subdivision code *without* the country prefix (e.g. `CA`).
    /// When present, the output ISO field is formatted as `<country>-<region>`.
    pub region: Option<&'a str>,
    /// UTF-8 city name; `None` is serialized as an empty string.
    pub city: Option<&'a str>,
    // Pre-computed sort fields; kept private to enforce construction through
    // the provided constructors.
    sort_family: u8,         // 0 = IPv4, 1 = IPv6, 2 = unparseable (fallback)
    sort_addr: u128,         // network address as an unsigned integer
    sort_secondary: Option<&'a str>, // Some(site_slug) for site prefixes; None = fall back to prefix
}

impl<'a> Record<'a> {
    /// Construct a record from a site-assigned prefix.
    ///
    /// `site_slug` is used as the secondary sort key (see §7).
    #[must_use]
    pub fn from_site_prefix(
        prefix: &'a str,
        country: &'a str,
        region: Option<&'a str>,
        city: Option<&'a str>,
        site_slug: &'a str,
    ) -> Self {
        let (sort_family, sort_addr) = addr_sort_key(prefix);
        Self {
            prefix,
            country,
            region,
            city,
            sort_family,
            sort_addr,
            sort_secondary: Some(site_slug),
        }
    }

    /// Construct a record from an aggregate.
    ///
    /// Only the country column is populated; region and city are left empty.
    /// The aggregate's own CIDR string is used as the secondary sort key.
    #[must_use]
    pub fn from_aggregate(prefix: &'a str, country: &'a str) -> Self {
        let (sort_family, sort_addr) = addr_sort_key(prefix);
        Self {
            prefix,
            country,
            region: None,
            city: None,
            sort_family,
            sort_addr,
            sort_secondary: None,
        }
    }

    /// Return the region column value: `<country>-<region>` when a region is
    /// known, or an empty string otherwise.
    ///
    /// Allocates only when both country and region are present; returns
    /// `Cow::Borrowed("")` for records without a region to avoid a heap
    /// allocation on every such row.
    fn region_field(&self) -> Cow<'_, str> {
        match self.region {
            Some(r) if !r.is_empty() => Cow::Owned(format!("{}-{}", self.country, r)),
            _ => Cow::Borrowed(""),
        }
    }
}

/// Parse the network address from a CIDR string and return a tuple
/// `(family, addr)` suitable for numeric ordering.
fn addr_sort_key(prefix_cidr: &str) -> (u8, u128) {
    let addr_str = prefix_cidr.split('/').next().unwrap_or(prefix_cidr);
    match addr_str.parse::<IpAddr>() {
        Ok(IpAddr::V4(a)) => (0, u128::from(u32::from(a))),
        Ok(IpAddr::V6(a)) => (1, u128::from(a)),
        Err(_) => (2, 0), // unparseable — sorts last
    }
}

// ---------------------------------------------------------------------------
// Sorting
// ---------------------------------------------------------------------------

/// Sort `records` in-place following the §7 determinism rules:
/// IPv4 before IPv6, numeric network-address order, secondary sort on
/// site slug / aggregate CIDR.
pub fn sort(records: &mut [Record<'_>]) {
    records.sort_by(|a, b| {
        // Aggregates have no explicit secondary key; fall back to the prefix CIDR string.
        let a_sec = a.sort_secondary.unwrap_or(a.prefix);
        let b_sec = b.sort_secondary.unwrap_or(b.prefix);
        (a.sort_family, a.sort_addr, a_sec).cmp(&(b.sort_family, b.sort_addr, b_sec))
    });
}

// ---------------------------------------------------------------------------
// CSV serialization
// ---------------------------------------------------------------------------

/// Parameters that vary per run and appear in the comment header.
pub struct FeedParams<'a> {
    /// RFC 3339 UTC timestamp string, e.g. `2024-01-01T00:00:00Z`.
    pub timestamp: &'a str,
    /// Crate version string, e.g. `0.1.0`.
    pub version: &'a str,
    /// Short git SHA or `"unknown"`.
    pub git_sha: &'a str,
}

/// Write a complete RFC 8805 geofeed — comment header followed by CSV rows —
/// to `out`.
///
/// `records` must already be sorted (call [`sort`] first).
///
/// The header deliberately does **not** disclose the source `NetBox` URL,
/// which is considered private operational data. The body is hashed with
/// SHA-256 so consumers can detect tampering or transport corruption.
///
/// # Errors
///
/// Returns any I/O or CSV serialization error encountered while writing.
pub fn write_feed<W: io::Write>(
    records: &[Record<'_>],
    mut out: W,
    params: &FeedParams<'_>,
) -> io::Result<()> {
    use aws_lc_rs::digest;

    // Serialize the CSV body into an in-memory buffer first so we can
    // compute its SHA-256 and place the digest in the comment header.
    let mut body = Vec::<u8>::new();
    {
        let mut wtr = csv::WriterBuilder::new()
            .has_headers(false)
            .terminator(csv::Terminator::Any(b'\n'))
            .from_writer(&mut body);

        for record in records {
            let region = record.region_field();
            let city = record.city.unwrap_or("");
            wtr.write_record([
                record.prefix,
                record.country,
                &*region, // deref Cow<str> → &str; no allocation when region is absent
                city,
                "",
            ])
            .map_err(io::Error::other)?;
        }
        wtr.flush()?;
    }

    let digest = digest::digest(&digest::SHA256, &body);
    let digest_hex = hex_lower(digest.as_ref());

    // Comment header — modeled on Google Corp's published geofeed.
    writeln!(
        out,
        "# netbox-geofeed {} ({})",
        params.version, params.git_sha
    )?;
    writeln!(
        out,
        "# Self-published geofeed as defined in datatracker.ietf.org/doc/html/rfc8805"
    )?;
    writeln!(out, "# Last updated (rfc3339): {}", params.timestamp)?;
    writeln!(
        out,
        "# Number of records: {}, checksum of the actual content minus comments:",
        records.len()
    )?;
    writeln!(out, "# SHA256 = {digest_hex}")?;

    out.write_all(&body)?;
    Ok(())
}

/// Lower-case hex encoding without pulling in an extra crate.
fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

// ---------------------------------------------------------------------------
// Globally-routable prefix filter
// ---------------------------------------------------------------------------

/// IPv4 bogon/special-use prefixes that must not appear in the geofeed.
/// Each entry is `(network_address_as_u32, prefix_length)`.
const IPV4_BOGONS: &[(u32, u8)] = &[
    (0x0000_0000, 8),  // 0.0.0.0/8        — "This" network (RFC 1122)
    (0x0a00_0000, 8),  // 10.0.0.0/8       — Private (RFC 1918)
    (0x6440_0000, 10), // 100.64.0.0/10    — Shared address space (RFC 6598)
    (0x7f00_0000, 8),  // 127.0.0.0/8      — Loopback (RFC 1122)
    (0xa9fe_0000, 16), // 169.254.0.0/16   — Link-local (RFC 3927)
    (0xac10_0000, 12), // 172.16.0.0/12    — Private (RFC 1918)
    (0xc000_0000, 24), // 192.0.0.0/24     — IETF protocol assignments (RFC 6890)
    (0xc000_0200, 24), // 192.0.2.0/24     — TEST-NET-1 / documentation (RFC 5737)
    (0xc058_6300, 24), // 192.88.99.0/24   — 6to4 relay anycast (RFC 7526)
    (0xc0a8_0000, 16), // 192.168.0.0/16   — Private (RFC 1918)
    (0xc612_0000, 15), // 198.18.0.0/15    — Benchmarking (RFC 2544)
    (0xc633_6400, 24), // 198.51.100.0/24  — TEST-NET-2 / documentation (RFC 5737)
    (0xcb00_7100, 24), // 203.0.113.0/24   — TEST-NET-3 / documentation (RFC 5737)
    (0xe000_0000, 4),  // 224.0.0.0/4      — Multicast (RFC 1112)
    (0xf000_0000, 4),  // 240.0.0.0/4      — Reserved / limited broadcast (RFC 1112)
];

/// IPv6 bogon/special-use prefixes that must not appear in the geofeed.
/// Each entry is `(network_address_as_u128, prefix_length)`.
const IPV6_BOGONS: &[(u128, u8)] = &[
    (0x0000_0000_0000_0000_0000_0000_0000_0000_u128, 128), // ::/128          — Unspecified (RFC 4291)
    (0x0000_0000_0000_0000_0000_0000_0000_0001_u128, 128), // ::1/128         — Loopback (RFC 4291)
    (0x0000_0000_0000_0000_0000_ffff_0000_0000_u128, 96), // ::ffff:0:0/96   — IPv4-mapped (RFC 4291)
    (0x0064_ff9b_0000_0000_0000_0000_0000_0000_u128, 96), // 64:ff9b::/96    — NAT64 (RFC 6052)
    (0x0064_ff9b_0001_0000_0000_0000_0000_0000_u128, 48), // 64:ff9b:1::/48  — NAT64 local (RFC 8215)
    (0x0100_0000_0000_0000_0000_0000_0000_0000_u128, 64), // 100::/64        — Discard (RFC 6666)
    (0x2001_0000_0000_0000_0000_0000_0000_0000_u128, 32), // 2001::/32       — Teredo (RFC 4380)
    (0x2001_0002_0000_0000_0000_0000_0000_0000_u128, 48), // 2001:2::/48     — Benchmarking (RFC 5180)
    (0x2001_0db8_0000_0000_0000_0000_0000_0000_u128, 32), // 2001:db8::/32   — Documentation (RFC 3849)
    (0x2002_0000_0000_0000_0000_0000_0000_0000_u128, 16), // 2002::/16       — 6to4 (RFC 3056)
    (0xfc00_0000_0000_0000_0000_0000_0000_0000_u128, 7), // fc00::/7        — Unique Local (RFC 4193)
    (0xfe80_0000_0000_0000_0000_0000_0000_0000_u128, 10), // fe80::/10       — Link-local (RFC 4291)
    (0xff00_0000_0000_0000_0000_0000_0000_0000_u128, 8), // ff00::/8        — Multicast (RFC 4291)
];

/// Returns `true` when `prefix_cidr` represents a prefix that could appear
/// in the global routing table — i.e., it is not a subnet of (or equal to)
/// any well-known special-use or bogon range (RFC 1918, RFC 5737, loopback,
/// link-local, multicast, ULA, etc.).
///
/// Returns `false` for prefixes that cannot be parsed.
pub fn is_globally_routable(prefix_cidr: &str) -> bool {
    let Some((addr_str, len_str)) = prefix_cidr.split_once('/') else {
        return false;
    };
    let Ok(len) = len_str.parse::<u8>() else {
        return false;
    };
    if let Ok(addr) = addr_str.parse::<Ipv4Addr>() {
        is_globally_routable_v4(addr, len)
    } else if let Ok(addr) = addr_str.parse::<Ipv6Addr>() {
        is_globally_routable_v6(addr, len)
    } else {
        false
    }
}

fn ipv4_mask(len: u8) -> u32 {
    if len == 0 {
        0
    } else {
        !0u32 << (32 - u32::from(len))
    }
}

fn ipv6_mask(len: u8) -> u128 {
    if len == 0 {
        0
    } else {
        !0u128 << (128 - u32::from(len))
    }
}

/// Returns `true` if `addr` falls within the network `net/len`.
fn ipv4_in_net(addr: u32, net: u32, len: u8) -> bool {
    let mask = ipv4_mask(len);
    addr & mask == net & mask
}

/// Returns `true` if `addr` falls within the network `net/len`.
fn ipv6_in_net(addr: u128, net: u128, len: u8) -> bool {
    let mask = ipv6_mask(len);
    addr & mask == net & mask
}

fn is_globally_routable_v4(addr: Ipv4Addr, len: u8) -> bool {
    if len > 32 {
        return false;
    }
    let a = u32::from(addr);
    IPV4_BOGONS
        .iter()
        .all(|&(net, net_len)| !(len >= net_len && ipv4_in_net(a, net, net_len)))
}

fn is_globally_routable_v6(addr: Ipv6Addr, len: u8) -> bool {
    if len > 128 {
        return false;
    }
    let a = u128::from(addr);
    IPV6_BOGONS
        .iter()
        .all(|&(net, net_len)| !(len >= net_len && ipv6_in_net(a, net, net_len)))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ── sort ─────────────────────────────────────────────────────────────────

    #[test]
    fn sort_ipv4_before_ipv6() {
        let mut records = vec![
            Record::from_site_prefix("2001:db8::/32", "DE", None, None, "ber1"),
            Record::from_site_prefix("10.0.0.0/8", "US", None, None, "nyc1"),
        ];
        sort(&mut records);
        assert_eq!(records[0].prefix, "10.0.0.0/8");
        assert_eq!(records[1].prefix, "2001:db8::/32");
    }

    #[test]
    fn sort_numeric_within_family() {
        let mut records = vec![
            Record::from_site_prefix("192.168.0.0/24", "GB", None, None, "lon1"),
            Record::from_aggregate("10.0.0.0/8", "US"),
            Record::from_site_prefix("10.1.0.0/16", "US", Some("CA"), None, "sfo1"),
        ];
        sort(&mut records);
        assert_eq!(records[0].prefix, "10.0.0.0/8");
        assert_eq!(records[1].prefix, "10.1.0.0/16");
        assert_eq!(records[2].prefix, "192.168.0.0/24");
    }

    #[test]
    fn sort_secondary_key_breaks_ties() {
        // Two records with the same network address (hypothetically same prefix
        // from aggregate vs site) should be ordered by secondary key.
        let mut records = vec![
            Record::from_site_prefix("10.0.0.0/8", "US", None, None, "zzz"),
            Record::from_aggregate("10.0.0.0/8", "US"),
        ];
        sort(&mut records);
        // aggregate secondary = None → falls back to "10.0.0.0/8"; site secondary = Some("zzz")
        // "10.0.0.0/8" < "zzz" lexicographically
        assert_eq!(records[0].sort_secondary.unwrap_or(records[0].prefix), "10.0.0.0/8");
        assert_eq!(records[1].sort_secondary.unwrap_or(records[1].prefix), "zzz");
    }

    // ── region_field ─────────────────────────────────────────────────────────

    #[test]
    fn region_field_country_only() {
        let r = Record::from_aggregate("10.0.0.0/8", "US");
        assert_eq!(r.prefix, "10.0.0.0/8");
        assert_eq!(r.country, "US");
        assert_eq!(r.region, None);
        assert_eq!(r.city, None);
        assert_eq!(r.region_field(), "");
    }

    #[test]
    fn region_field_country_and_region() {
        let r = Record::from_site_prefix("10.0.0.0/8", "US", Some("CA"), None, "sfo1");
        assert_eq!(r.prefix, "10.0.0.0/8");
        assert_eq!(r.country, "US");
        assert_eq!(r.region, Some("CA"));
        assert_eq!(r.city, None);
        assert_eq!(r.region_field(), "US-CA");
    }

    #[test]
    fn region_field_empty_region_treated_as_absent() {
        let r = Record::from_site_prefix("10.0.0.0/8", "US", Some(""), None, "sfo1");
        assert_eq!(r.prefix, "10.0.0.0/8");
        assert_eq!(r.country, "US");
        assert_eq!(r.region, Some(""));
        assert_eq!(r.city, None);
        assert_eq!(r.region_field(), "");
    }

    // ── RFC 8805 compliance ───────────────────────────────────────────────────

    /// Serialize `records` through `write_feed` and return the non-comment
    /// CSV rows as `Vec<Vec<String>>`, splitting each line on commas.
    ///
    /// The helper sorts the records first, matching the production code path.
    fn feed_rows(mut records: Vec<Record>) -> Vec<Vec<String>> {
        sort(&mut records);
        let mut buf = Vec::new();
        write_feed(
            &records,
            &mut buf,
            &FeedParams {
                timestamp: "2024-01-01T00:00:00Z",
                version: "0.1.0",
                git_sha: "test",
            },
        )
        .expect("write_feed must not fail");
        String::from_utf8(buf)
            .expect("output must be valid UTF-8")
            .lines()
            .filter(|l| !l.starts_with('#') && !l.is_empty())
            .map(|l| l.split(',').map(str::to_owned).collect())
            .collect()
    }

    /// RFC 8805 §2.1: every data row must carry exactly five comma-separated
    /// fields: `ip_prefix,alpha2code,region,city,postal_code`.
    #[test]
    fn each_row_has_exactly_five_fields() {
        let records = vec![
            Record::from_aggregate("1.2.3.0/24", "US"),
            Record::from_site_prefix("5.6.7.0/24", "GB", Some("ENG"), Some("London"), "lon1"),
            Record::from_site_prefix("8.0.0.0/8", "DE", None, None, "ber1"),
        ];
        for row in feed_rows(records) {
            assert_eq!(row.len(), 5, "expected 5 fields, got: {row:?}");
        }
    }

    /// RFC 8805 §2.1.1.2: alpha2code must be a 2-letter ISO 3166-1 alpha-2
    /// uppercase code or an empty string. It must never contain the country +
    /// region joined together.
    #[test]
    fn alpha2code_is_two_uppercase_letters() {
        let records = vec![
            Record::from_aggregate("1.2.3.0/24", "US"),
            Record::from_site_prefix("5.6.7.0/24", "GB", Some("ENG"), Some("London"), "lon1"),
            Record::from_site_prefix("8.0.0.0/8", "DE", Some("BE"), Some("Berlin"), "ber1"),
        ];
        for row in feed_rows(records) {
            let code = &row[1];
            assert!(
                code.len() == 2 && code.chars().all(|c| c.is_ascii_uppercase()),
                "alpha2code {code:?} must be exactly 2 uppercase ASCII letters"
            );
        }
    }

    /// RFC 8805 §2.1.1.3: region must be in ISO 3166-2 `CC-SUB` format or
    /// empty. It must never hold only a subdivision code without the country
    /// prefix.
    #[test]
    fn region_is_iso3166_2_or_empty() {
        let records = vec![
            Record::from_aggregate("1.2.3.0/24", "US"),
            Record::from_site_prefix("5.6.7.0/24", "GB", Some("ENG"), Some("London"), "lon1"),
            Record::from_site_prefix("8.0.0.0/8", "DE", None, None, "ber1"),
        ];
        for row in feed_rows(records) {
            let region = &row[2];
            if region.is_empty() {
                continue;
            }
            let Some((cc, sub)) = region.split_once('-') else {
                panic!("region {region:?} is not in CC-SUB format");
            };
            assert!(
                cc.len() == 2 && cc.chars().all(|c| c.is_ascii_uppercase()),
                "region country prefix {cc:?} must be 2 uppercase letters"
            );
            assert!(
                !sub.is_empty(),
                "subdivision in {region:?} must not be empty"
            );
        }
    }

    /// RFC 8805 §2.1.1.3: when the region field is non-empty, its country
    /// prefix must match the alpha2code field exactly.
    #[test]
    fn region_country_prefix_matches_alpha2code() {
        let records = vec![
            Record::from_site_prefix("1.2.3.0/24", "US", Some("NY"), Some("New York"), "nyc1"),
            Record::from_site_prefix("5.6.7.0/24", "GB", Some("ENG"), Some("London"), "lon1"),
            Record::from_site_prefix("8.0.0.0/8", "DE", Some("BE"), Some("Berlin"), "ber1"),
        ];
        for row in feed_rows(records) {
            let country = &row[1];
            let region = &row[2];
            if region.is_empty() {
                continue;
            }
            let cc = region.split('-').next().unwrap();
            assert_eq!(
                cc, country,
                "region CC prefix {cc:?} must match alpha2code {country:?}"
            );
        }
    }

    /// RFC 8805 §2.1.1.5: postal code is deprecated; this tool never emits
    /// one. The column must be present but always empty.
    #[test]
    fn postal_code_is_always_empty() {
        let records = vec![
            Record::from_aggregate("1.2.3.0/24", "US"),
            Record::from_site_prefix("5.6.7.0/24", "GB", Some("ENG"), Some("London"), "lon1"),
        ];
        for row in feed_rows(records) {
            assert_eq!(row[4], "", "postal code (field 5) must always be empty");
        }
    }

    /// RFC 8805 §2.1.1.4: city SHOULD exclude the comma character.
    #[test]
    fn city_contains_no_commas() {
        let records = vec![
            Record::from_site_prefix("1.2.3.0/24", "US", Some("NY"), Some("New York"), "nyc1"),
            Record::from_site_prefix("5.6.7.0/24", "GB", Some("ENG"), Some("London"), "lon1"),
        ];
        for row in feed_rows(records) {
            assert!(
                !row[3].contains(','),
                "city {:?} must not contain a comma",
                row[3]
            );
        }
    }

    /// Aggregates contribute only the alpha2code (country) column; region,
    /// city, and postal code must all be empty.
    #[test]
    fn aggregate_row_emits_country_only() {
        let records = vec![Record::from_aggregate("1.2.3.0/24", "US")];
        let rows = feed_rows(records);
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row[0], "1.2.3.0/24");
        assert_eq!(row[1], "US");
        assert_eq!(row[2], "", "aggregate region must be empty");
        assert_eq!(row[3], "", "aggregate city must be empty");
        assert_eq!(row[4], "", "aggregate postal code must be empty");
    }

    /// Site-prefix rows must emit all five geo fields in the correct columns.
    #[test]
    fn site_prefix_row_emits_all_geo_fields() {
        let records = vec![Record::from_site_prefix(
            "1.2.3.0/24",
            "US",
            Some("NY"),
            Some("New York"),
            "nyc1",
        )];
        let rows = feed_rows(records);
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row[0], "1.2.3.0/24");
        assert_eq!(row[1], "US");
        assert_eq!(row[2], "US-NY");
        assert_eq!(row[3], "New York");
        assert_eq!(row[4], "");
    }

    /// A site-prefix with no region must still emit an empty region column and
    /// a populated alpha2code — the country must not migrate into the region
    /// slot.
    #[test]
    fn site_prefix_without_region_leaves_region_column_empty() {
        let records = vec![Record::from_site_prefix(
            "8.0.0.0/8",
            "DE",
            None,
            Some("Berlin"),
            "ber1",
        )];
        let rows = feed_rows(records);
        let row = &rows[0];
        assert_eq!(row[1], "DE");
        assert_eq!(
            row[2], "",
            "region column must be empty when no region is known"
        );
        assert_eq!(row[3], "Berlin");
    }

    // ── write_feed / golden-file ──────────────────────────────────────────────

    #[test]
    fn golden_file() {
        let mut records = vec![
            // Deliberately out of order to exercise sort().
            Record::from_site_prefix("192.168.0.0/24", "GB", Some("ENG"), Some("London"), "lon1"),
            Record::from_aggregate("10.0.0.0/8", "US"),
            Record::from_site_prefix("2001:db8::/32", "DE", Some("BE"), Some("Berlin"), "ber1"),
            Record::from_site_prefix(
                "10.1.0.0/16",
                "US",
                Some("CA"),
                Some("San Francisco"),
                "sfo1",
            ),
        ];
        sort(&mut records);

        let mut buf = Vec::new();
        write_feed(
            &records,
            &mut buf,
            &FeedParams {
                timestamp: "2024-01-01T00:00:00Z",
                version: "0.1.0",
                git_sha: "abc1234",
            },
        )
        .expect("write_feed should not fail");

        let actual = String::from_utf8(buf).expect("output must be valid UTF-8");
        let expected = include_str!("../tests/fixtures/geofeed_golden.csv");
        assert_eq!(actual, expected);
    }

    // ── is_globally_routable ─────────────────────────────────────────────────

    // -- IPv4 bogons that must be rejected --

    #[test]
    fn ipv4_rfc1918_10_rejected() {
        assert!(!is_globally_routable("10.0.0.0/8"));
    }

    #[test]
    fn ipv4_rfc1918_172_rejected() {
        assert!(!is_globally_routable("172.16.0.0/12"));
    }

    #[test]
    fn ipv4_rfc1918_192_rejected() {
        assert!(!is_globally_routable("192.168.0.0/16"));
    }

    #[test]
    fn ipv4_rfc1918_subnet_rejected() {
        // More-specific subnets of RFC 1918 space are also bogons.
        assert!(!is_globally_routable("10.1.2.0/24"));
        assert!(!is_globally_routable("172.31.0.0/16"));
        assert!(!is_globally_routable("192.168.1.0/24"));
    }

    #[test]
    fn ipv4_loopback_rejected() {
        assert!(!is_globally_routable("127.0.0.0/8"));
        assert!(!is_globally_routable("127.0.0.1/32"));
    }

    #[test]
    fn ipv4_link_local_rejected() {
        assert!(!is_globally_routable("169.254.0.0/16"));
        assert!(!is_globally_routable("169.254.1.0/24"));
    }

    #[test]
    fn ipv4_shared_address_space_rejected() {
        // 100.64.0.0/10 — RFC 6598 (carrier-grade NAT)
        assert!(!is_globally_routable("100.64.0.0/10"));
        assert!(!is_globally_routable("100.100.0.0/16"));
    }

    #[test]
    fn ipv4_documentation_rejected() {
        assert!(!is_globally_routable("192.0.2.0/24")); // TEST-NET-1
        assert!(!is_globally_routable("198.51.100.0/24")); // TEST-NET-2
        assert!(!is_globally_routable("203.0.113.0/24")); // TEST-NET-3
    }

    #[test]
    fn ipv4_benchmarking_rejected() {
        assert!(!is_globally_routable("198.18.0.0/15"));
        assert!(!is_globally_routable("198.19.0.0/16"));
    }

    #[test]
    fn ipv4_multicast_rejected() {
        assert!(!is_globally_routable("224.0.0.0/4"));
        assert!(!is_globally_routable("239.255.255.0/24"));
    }

    #[test]
    fn ipv4_reserved_rejected() {
        assert!(!is_globally_routable("240.0.0.0/4"));
        assert!(!is_globally_routable("255.255.255.255/32"));
    }

    #[test]
    fn ipv4_6to4_relay_rejected() {
        assert!(!is_globally_routable("192.88.99.0/24"));
    }

    // -- IPv4 globally routable prefixes that must be accepted --

    #[test]
    fn ipv4_globally_routable_accepted() {
        assert!(is_globally_routable("8.8.8.0/24"));
        assert!(is_globally_routable("1.1.1.0/24"));
        assert!(is_globally_routable("9.0.0.0/8"));
        assert!(is_globally_routable("104.16.0.0/13"));
        assert!(is_globally_routable("185.1.0.0/22"));
    }

    // -- IPv6 bogons that must be rejected --

    #[test]
    fn ipv6_unspecified_loopback_rejected() {
        assert!(!is_globally_routable("::/128"));
        assert!(!is_globally_routable("::1/128"));
    }

    #[test]
    fn ipv6_ipv4_mapped_rejected() {
        assert!(!is_globally_routable("::ffff:0:0/96"));
        assert!(!is_globally_routable("::ffff:192.0.2.1/128"));
    }

    #[test]
    fn ipv6_nat64_rejected() {
        assert!(!is_globally_routable("64:ff9b::/96"));
        assert!(!is_globally_routable("64:ff9b:1::/48"));
    }

    #[test]
    fn ipv6_discard_rejected() {
        assert!(!is_globally_routable("100::/64"));
    }

    #[test]
    fn ipv6_teredo_rejected() {
        assert!(!is_globally_routable("2001::/32"));
    }

    #[test]
    fn ipv6_benchmarking_rejected() {
        assert!(!is_globally_routable("2001:2::/48"));
    }

    #[test]
    fn ipv6_documentation_rejected() {
        assert!(!is_globally_routable("2001:db8::/32"));
        assert!(!is_globally_routable("2001:db8:1::/48"));
    }

    #[test]
    fn ipv6_6to4_rejected() {
        assert!(!is_globally_routable("2002::/16"));
        assert!(!is_globally_routable("2002:c000:200::/48"));
    }

    #[test]
    fn ipv6_ula_rejected() {
        assert!(!is_globally_routable("fc00::/7"));
        assert!(!is_globally_routable("fd00::/8"));
        assert!(!is_globally_routable("fd12:3456::/32"));
    }

    #[test]
    fn ipv6_link_local_rejected() {
        assert!(!is_globally_routable("fe80::/10"));
        assert!(!is_globally_routable("fe80::1/128"));
    }

    #[test]
    fn ipv6_multicast_rejected() {
        assert!(!is_globally_routable("ff00::/8"));
        assert!(!is_globally_routable("ff02::1/128"));
    }

    // -- IPv6 globally routable prefixes that must be accepted --

    #[test]
    fn ipv6_globally_routable_accepted() {
        assert!(is_globally_routable("2001:4860::/32")); // Google
        assert!(is_globally_routable("2606:4700::/32")); // Cloudflare
        assert!(is_globally_routable("2a00:1450::/32")); // Google EU
        assert!(is_globally_routable("2400:cb00::/32")); // Cloudflare AP
    }

    // -- parse errors and invalid inputs --

    #[test]
    fn invalid_prefix_rejected() {
        assert!(!is_globally_routable("not-an-ip/24"));
        assert!(!is_globally_routable("8.8.8.8")); // no prefix length
        assert!(!is_globally_routable("8.8.8.8/33")); // prefix len > 32
        assert!(!is_globally_routable("2001:db8::/129")); // prefix len > 128
        assert!(!is_globally_routable(""));
    }
}
