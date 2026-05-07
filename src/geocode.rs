//! `geocode` subcommand — backfills missing geo fields in `NetBox` via `ArcGIS`.
//!
//! The [`fill_missing`] function is also called inline by the `generate`
//! subcommand as a side effect of producing the feed (see `generate.rs`).
//!
//! Write policy (§2.1): only empty fields are filled; populated fields are
//! never overwritten.

use anyhow::Context as _;
use arcgis_geocoder::{FindAddressCandidatesParams, GeocoderClient, OAuthCredentials};
use futures_util::TryStreamExt as _;
use netbox_client::{Site, SitePatchRequest, dcim::SiteFilter};
use sonic_rs::{JsonValueTrait as _, Value};

use crate::cli::GeocodeArgs;
use crate::netbox::Netbox;

// ---------------------------------------------------------------------------
// GeoResult — extracted geocoding output
// ---------------------------------------------------------------------------

/// Geocoding output extracted from a single `ArcGIS` candidate.
#[derive(Debug, Clone)]
pub struct GeoResult {
    /// ISO 3166-1 alpha-2 country code (e.g. `US`, `DE`).
    pub country: String,
    /// ISO 3166-2 subdivision code *without* the country prefix (e.g. `CA`).
    pub region: Option<String>,
    /// UTF-8 city name.
    pub city: Option<String>,
    /// WGS 84 latitude.
    pub latitude: f64,
    /// WGS 84 longitude.
    pub longitude: f64,
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Entry point for the `geocode` subcommand.
///
/// # Errors
///
/// Returns an error on `NetBox` or `ArcGIS` API failures.
pub async fn run(args: GeocodeArgs) -> anyhow::Result<()> {
    let creds = OAuthCredentials::new(
        args.arcgis_client_id.clone(),
        args.arcgis_client_secret.clone(),
    );
    let geocoder = GeocoderClient::with_oauth_credentials(creds)
        .context("failed to construct ArcGIS geocoder client")?;

    let netbox = Netbox::new(&args.global.netbox_url, &args.global.netbox_token)
        .context("failed to initialise NetBox client")?;

    let filter = SiteFilter {
        slug: args.sites.clone(),
        ..Default::default()
    };

    let sites: Vec<Site> = netbox
        .sites_stream(&filter)
        .try_collect()
        .await
        .context("failed to stream sites from NetBox")?;

    log::info!("starting geocode run (total={})", sites.len());

    let mut filled = 0usize;
    let mut skipped = 0usize;

    for site in &sites {
        // Log before-state for any site that has missing geo fields.
        let before_country = cf_str(site, "geofeed_country").unwrap_or("");
        let before_region = cf_str(site, "geofeed_region").unwrap_or("");
        let before_city = cf_str(site, "geofeed_city").unwrap_or("");

        match fill_missing(site, &geocoder, &netbox, args.min_score, args.no_write).await {
            Ok(Some(updated)) => {
                let after_country = updated
                    .custom_fields
                    .as_ref()
                    .and_then(|cf| cf.get("geofeed_country"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                log::info!(
                    "geocoded site={} before=(country={before_country}, region={before_region}, city={before_city}, lat={:?}, lon={:?}) after=(country={after_country}, lat={:?}, lon={:?})",
                    site.slug,
                    site.latitude,
                    site.longitude,
                    updated.latitude,
                    updated.longitude,
                );
                filled += 1;
            }
            Ok(None) => {
                skipped += 1;
            }
            Err(e) => {
                log::warn!("geocoding failed for site={}: {e}; skipping", site.slug);
                skipped += 1;
            }
        }
    }

    log::info!("geocode run complete (filled={filled}, skipped={skipped})");
    Ok(())
}

/// Geocode `site.physical_address` via `ArcGIS` and PATCH any empty geo fields
/// back to `NetBox`.
///
/// Returns `Some(updated_site)` when at least one field was written (or would
/// have been written under `--no-write`), `None` when all fields were already
/// populated or geocoding produced no usable result.
///
/// # Write policy
///
/// Only fields that are currently empty on the site are written. Populated
/// fields are never overwritten (§2.1).
///
/// # Errors
///
/// Returns an error on `ArcGIS` or `NetBox` API failures.
pub async fn fill_missing(
    site: &Site,
    geocoder: &GeocoderClient,
    netbox: &Netbox,
    min_score: f64,
    no_write: bool,
) -> anyhow::Result<Option<Site>> {
    let needs_country = cf_empty(site, "geofeed_country");
    let needs_region = cf_empty(site, "geofeed_region");
    let needs_city = cf_empty(site, "geofeed_city");
    let needs_lat = site.latitude.is_none();
    let needs_lon = site.longitude.is_none();

    if !needs_country && !needs_region && !needs_city && !needs_lat && !needs_lon {
        // Nothing to fill.
        return Ok(None);
    }

    if site.physical_address.trim().is_empty() {
        log::warn!(
            "cannot geocode site={}: physical_address is empty",
            site.slug,
        );
        return Ok(None);
    }

    let Some(geo) = geocode_site(site, geocoder, min_score).await? else {
        return Ok(None);
    };

    let patch = build_patch(
        &geo,
        needs_country,
        needs_region,
        needs_city,
        needs_lat,
        needs_lon,
    );

    if no_write {
        let body = sonic_rs::to_string_pretty(&patch)
            .unwrap_or_else(|_| "<serialization error>".to_owned());
        println!("would PATCH site {} (id={}):\n{body}", site.slug, site.id);
        // Return the existing site unchanged (no write happened).
        return Ok(None);
    }

    let updated = netbox
        .site_patch(site.id, &patch)
        .await
        .with_context(|| format!("failed to PATCH site {} (id={})", site.slug, site.id))?;

    Ok(Some(updated))
}

// ---------------------------------------------------------------------------
// Geocoding internals
// ---------------------------------------------------------------------------

/// Call `ArcGIS` `findAddressCandidates` for `site.physical_address`.
///
/// Returns the first candidate whose score meets `min_score`, or `None` if
/// there are no qualifying candidates.
///
/// # Errors
///
/// Returns an error if the `ArcGIS` HTTP request fails.
pub async fn geocode_site(
    site: &Site,
    geocoder: &GeocoderClient,
    min_score: f64,
) -> anyhow::Result<Option<GeoResult>> {
    let params = FindAddressCandidatesParams {
        single_line: Some(site.physical_address.clone()),
        out_fields: Some("CountryCode,Region,RegionAbbr,City".to_owned()),
        max_locations: Some(1),
        for_storage: Some(false),
        ..Default::default()
    };

    let resp = geocoder
        .find_address_candidates(&params)
        .await
        .with_context(|| format!("ArcGIS geocode failed for site {}", site.slug))?;

    let Some(candidate) = resp.candidates.into_iter().find(|c| c.score >= min_score) else {
        log::warn!(
            "no ArcGIS candidates met min_score={min_score} for site={} address={:?}",
            site.slug,
            site.physical_address,
        );
        return Ok(None);
    };

    log::debug!(
        "ArcGIS candidate selected: site={} address={:?} score={}",
        site.slug,
        candidate.address,
        candidate.score,
    );

    let attrs = &candidate.attributes;

    let raw_country = str_attr(attrs, "CountryCode");
    let Some(country) = raw_country.and_then(normalize_country_code) else {
        log::warn!(
            "ArcGIS returned unrecognised country code={:?} for site={}; skipping geocode",
            raw_country.unwrap_or(""),
            site.slug,
        );
        return Ok(None);
    };

    // Prefer the abbreviation for the subdivision (e.g. "CA" for California).
    // Fall back to the full region name only if no abbreviation is available.
    let region = nonempty_str_attr(attrs, "RegionAbbr")
        .or_else(|| nonempty_str_attr(attrs, "Region"))
        .map(str::to_owned);

    let city = nonempty_str_attr(attrs, "City").map(str::to_owned);

    Ok(Some(GeoResult {
        country,
        region,
        city,
        latitude: candidate.location.y,
        longitude: candidate.location.x,
    }))
}

/// Construct a [`SitePatchRequest`] that fills only the missing fields.
#[allow(clippy::fn_params_excessive_bools)] // five discrete fill-or-not flags; a struct would be heavier without benefit
fn build_patch(
    geo: &GeoResult,
    needs_country: bool,
    needs_region: bool,
    needs_city: bool,
    needs_lat: bool,
    needs_lon: bool,
) -> SitePatchRequest {
    let mut custom_fields = sonic_rs::Object::new();
    if needs_country {
        custom_fields.insert("geofeed_country", Value::from(&geo.country));
    }
    if needs_region && let Some(r) = &geo.region {
        custom_fields.insert("geofeed_region", Value::from(r));
    }
    if needs_city && let Some(c) = &geo.city {
        custom_fields.insert("geofeed_city", Value::from(c));
    }

    SitePatchRequest {
        custom_fields: if custom_fields.is_empty() {
            None
        } else {
            Some(Value::from(custom_fields))
        },
        latitude: needs_lat.then_some(round6(geo.latitude)),
        longitude: needs_lon.then_some(round6(geo.longitude)),
        ..Default::default()
    }
}

/// Round a coordinate to 6 decimal places — NetBox's maximum precision for
/// the built-in `latitude` / `longitude` fields.
fn round6(v: f64) -> f64 {
    (v * 1_000_000.0).round() / 1_000_000.0
}

// ---------------------------------------------------------------------------
// Country code normalisation
// ---------------------------------------------------------------------------

/// Normalize a raw country code string to ISO 3166-1 alpha-2.
///
/// - If already 2 characters (e.g. `"US"`): returned as-is (uppercased).
/// - If 3 characters (e.g. `"USA"`): looked up in the ISO 3166-1 table.
/// - Otherwise: returns `None`.
fn normalize_country_code(raw: &str) -> Option<String> {
    let upper = raw.trim().to_uppercase();
    match upper.len() {
        2 => Some(upper),
        3 => alpha3_to_alpha2(&upper).map(str::to_owned),
        _ => None,
    }
}

/// ISO 3166-1 alpha-3 → alpha-2 lookup.
///
/// Covers all 249 UN-recognized territories as of the 2024 ISO 3166-1 table.
#[allow(clippy::too_many_lines)] // 249-entry ISO 3166-1 lookup table; no shorter form exists
fn alpha3_to_alpha2(alpha3: &str) -> Option<&'static str> {
    // Sorted by alpha-3 for readability; O(n) scan is fine for 249 entries.
    match alpha3 {
        "ABW" => Some("AW"), // Aruba
        "AFG" => Some("AF"), // Afghanistan
        "AGO" => Some("AO"), // Angola
        "AIA" => Some("AI"), // Anguilla
        "ALA" => Some("AX"), // Åland Islands
        "ALB" => Some("AL"), // Albania
        "AND" => Some("AD"), // Andorra
        "ARE" => Some("AE"), // United Arab Emirates
        "ARG" => Some("AR"), // Argentina
        "ARM" => Some("AM"), // Armenia
        "ASM" => Some("AS"), // American Samoa
        "ATA" => Some("AQ"), // Antarctica
        "ATF" => Some("TF"), // French Southern Territories
        "ATG" => Some("AG"), // Antigua and Barbuda
        "AUS" => Some("AU"), // Australia
        "AUT" => Some("AT"), // Austria
        "AZE" => Some("AZ"), // Azerbaijan
        "BDI" => Some("BI"), // Burundi
        "BEL" => Some("BE"), // Belgium
        "BEN" => Some("BJ"), // Benin
        "BES" => Some("BQ"), // Bonaire, Sint Eustatius and Saba
        "BFA" => Some("BF"), // Burkina Faso
        "BGD" => Some("BD"), // Bangladesh
        "BGR" => Some("BG"), // Bulgaria
        "BHR" => Some("BH"), // Bahrain
        "BHS" => Some("BS"), // Bahamas
        "BIH" => Some("BA"), // Bosnia and Herzegovina
        "BLM" => Some("BL"), // Saint Barthélemy
        "BLR" => Some("BY"), // Belarus
        "BLZ" => Some("BZ"), // Belize
        "BMU" => Some("BM"), // Bermuda
        "BOL" => Some("BO"), // Bolivia
        "BRA" => Some("BR"), // Brazil
        "BRB" => Some("BB"), // Barbados
        "BRN" => Some("BN"), // Brunei
        "BTN" => Some("BT"), // Bhutan
        "BVT" => Some("BV"), // Bouvet Island
        "BWA" => Some("BW"), // Botswana
        "CAF" => Some("CF"), // Central African Republic
        "CAN" => Some("CA"), // Canada
        "CCK" => Some("CC"), // Cocos (Keeling) Islands
        "CHE" => Some("CH"), // Switzerland
        "CHL" => Some("CL"), // Chile
        "CHN" => Some("CN"), // China
        "CIV" => Some("CI"), // Côte d'Ivoire
        "CMR" => Some("CM"), // Cameroon
        "COD" => Some("CD"), // Congo, DR
        "COG" => Some("CG"), // Congo
        "COK" => Some("CK"), // Cook Islands
        "COL" => Some("CO"), // Colombia
        "COM" => Some("KM"), // Comoros
        "CPV" => Some("CV"), // Cape Verde
        "CRI" => Some("CR"), // Costa Rica
        "CUB" => Some("CU"), // Cuba
        "CUW" => Some("CW"), // Curaçao
        "CXR" => Some("CX"), // Christmas Island
        "CYM" => Some("KY"), // Cayman Islands
        "CYP" => Some("CY"), // Cyprus
        "CZE" => Some("CZ"), // Czechia
        "DEU" => Some("DE"), // Germany
        "DJI" => Some("DJ"), // Djibouti
        "DMA" => Some("DM"), // Dominica
        "DNK" => Some("DK"), // Denmark
        "DOM" => Some("DO"), // Dominican Republic
        "DZA" => Some("DZ"), // Algeria
        "ECU" => Some("EC"), // Ecuador
        "EGY" => Some("EG"), // Egypt
        "ERI" => Some("ER"), // Eritrea
        "ESH" => Some("EH"), // Western Sahara
        "ESP" => Some("ES"), // Spain
        "EST" => Some("EE"), // Estonia
        "ETH" => Some("ET"), // Ethiopia
        "FIN" => Some("FI"), // Finland
        "FJI" => Some("FJ"), // Fiji
        "FLK" => Some("FK"), // Falkland Islands
        "FRA" => Some("FR"), // France
        "FRO" => Some("FO"), // Faroe Islands
        "FSM" => Some("FM"), // Micronesia
        "GAB" => Some("GA"), // Gabon
        "GBR" => Some("GB"), // United Kingdom
        "GEO" => Some("GE"), // Georgia
        "GGY" => Some("GG"), // Guernsey
        "GHA" => Some("GH"), // Ghana
        "GIB" => Some("GI"), // Gibraltar
        "GIN" => Some("GN"), // Guinea
        "GLP" => Some("GP"), // Guadeloupe
        "GMB" => Some("GM"), // Gambia
        "GNB" => Some("GW"), // Guinea-Bissau
        "GNQ" => Some("GQ"), // Equatorial Guinea
        "GRC" => Some("GR"), // Greece
        "GRD" => Some("GD"), // Grenada
        "GRL" => Some("GL"), // Greenland
        "GTM" => Some("GT"), // Guatemala
        "GUF" => Some("GF"), // French Guiana
        "GUM" => Some("GU"), // Guam
        "GUY" => Some("GY"), // Guyana
        "HKG" => Some("HK"), // Hong Kong
        "HMD" => Some("HM"), // Heard Island and McDonald Islands
        "HND" => Some("HN"), // Honduras
        "HRV" => Some("HR"), // Croatia
        "HTI" => Some("HT"), // Haiti
        "HUN" => Some("HU"), // Hungary
        "IDN" => Some("ID"), // Indonesia
        "IMN" => Some("IM"), // Isle of Man
        "IND" => Some("IN"), // India
        "IOT" => Some("IO"), // British Indian Ocean Territory
        "IRL" => Some("IE"), // Ireland
        "IRN" => Some("IR"), // Iran
        "IRQ" => Some("IQ"), // Iraq
        "ISL" => Some("IS"), // Iceland
        "ISR" => Some("IL"), // Israel
        "ITA" => Some("IT"), // Italy
        "JAM" => Some("JM"), // Jamaica
        "JEY" => Some("JE"), // Jersey
        "JOR" => Some("JO"), // Jordan
        "JPN" => Some("JP"), // Japan
        "KAZ" => Some("KZ"), // Kazakhstan
        "KEN" => Some("KE"), // Kenya
        "KGZ" => Some("KG"), // Kyrgyzstan
        "KHM" => Some("KH"), // Cambodia
        "KIR" => Some("KI"), // Kiribati
        "KNA" => Some("KN"), // Saint Kitts and Nevis
        "KOR" => Some("KR"), // South Korea
        "KWT" => Some("KW"), // Kuwait
        "LAO" => Some("LA"), // Laos
        "LBN" => Some("LB"), // Lebanon
        "LBR" => Some("LR"), // Liberia
        "LBY" => Some("LY"), // Libya
        "LCA" => Some("LC"), // Saint Lucia
        "LIE" => Some("LI"), // Liechtenstein
        "LKA" => Some("LK"), // Sri Lanka
        "LSO" => Some("LS"), // Lesotho
        "LTU" => Some("LT"), // Lithuania
        "LUX" => Some("LU"), // Luxembourg
        "LVA" => Some("LV"), // Latvia
        "MAC" => Some("MO"), // Macao
        "MAF" => Some("MF"), // Saint Martin (French)
        "MAR" => Some("MA"), // Morocco
        "MCO" => Some("MC"), // Monaco
        "MDA" => Some("MD"), // Moldova
        "MDG" => Some("MG"), // Madagascar
        "MDV" => Some("MV"), // Maldives
        "MEX" => Some("MX"), // Mexico
        "MHL" => Some("MH"), // Marshall Islands
        "MKD" => Some("MK"), // North Macedonia
        "MLI" => Some("ML"), // Mali
        "MLT" => Some("MT"), // Malta
        "MMR" => Some("MM"), // Myanmar
        "MNE" => Some("ME"), // Montenegro
        "MNG" => Some("MN"), // Mongolia
        "MNP" => Some("MP"), // Northern Mariana Islands
        "MOZ" => Some("MZ"), // Mozambique
        "MRT" => Some("MR"), // Mauritania
        "MSR" => Some("MS"), // Montserrat
        "MTQ" => Some("MQ"), // Martinique
        "MUS" => Some("MU"), // Mauritius
        "MWI" => Some("MW"), // Malawi
        "MYS" => Some("MY"), // Malaysia
        "MYT" => Some("YT"), // Mayotte
        "NAM" => Some("NA"), // Namibia
        "NCL" => Some("NC"), // New Caledonia
        "NER" => Some("NE"), // Niger
        "NFK" => Some("NF"), // Norfolk Island
        "NGA" => Some("NG"), // Nigeria
        "NIC" => Some("NI"), // Nicaragua
        "NIU" => Some("NU"), // Niue
        "NLD" => Some("NL"), // Netherlands
        "NOR" => Some("NO"), // Norway
        "NPL" => Some("NP"), // Nepal
        "NRU" => Some("NR"), // Nauru
        "NZL" => Some("NZ"), // New Zealand
        "OMN" => Some("OM"), // Oman
        "PAK" => Some("PK"), // Pakistan
        "PAN" => Some("PA"), // Panama
        "PCN" => Some("PN"), // Pitcairn
        "PER" => Some("PE"), // Peru
        "PHL" => Some("PH"), // Philippines
        "PLW" => Some("PW"), // Palau
        "PNG" => Some("PG"), // Papua New Guinea
        "POL" => Some("PL"), // Poland
        "PRI" => Some("PR"), // Puerto Rico
        "PRK" => Some("KP"), // North Korea
        "PRT" => Some("PT"), // Portugal
        "PRY" => Some("PY"), // Paraguay
        "PSE" => Some("PS"), // Palestine
        "PYF" => Some("PF"), // French Polynesia
        "QAT" => Some("QA"), // Qatar
        "REU" => Some("RE"), // Réunion
        "ROU" => Some("RO"), // Romania
        "RUS" => Some("RU"), // Russia
        "RWA" => Some("RW"), // Rwanda
        "SAU" => Some("SA"), // Saudi Arabia
        "SDN" => Some("SD"), // Sudan
        "SEN" => Some("SN"), // Senegal
        "SGP" => Some("SG"), // Singapore
        "SGS" => Some("GS"), // South Georgia
        "SHN" => Some("SH"), // Saint Helena
        "SJM" => Some("SJ"), // Svalbard and Jan Mayen
        "SLB" => Some("SB"), // Solomon Islands
        "SLE" => Some("SL"), // Sierra Leone
        "SLV" => Some("SV"), // El Salvador
        "SMR" => Some("SM"), // San Marino
        "SOM" => Some("SO"), // Somalia
        "SPM" => Some("PM"), // Saint Pierre and Miquelon
        "SRB" => Some("RS"), // Serbia
        "SSD" => Some("SS"), // South Sudan
        "STP" => Some("ST"), // São Tomé and Príncipe
        "SUR" => Some("SR"), // Suriname
        "SVK" => Some("SK"), // Slovakia
        "SVN" => Some("SI"), // Slovenia
        "SWE" => Some("SE"), // Sweden
        "SWZ" => Some("SZ"), // Eswatini
        "SXM" => Some("SX"), // Sint Maarten (Dutch)
        "SYC" => Some("SC"), // Seychelles
        "SYR" => Some("SY"), // Syria
        "TCA" => Some("TC"), // Turks and Caicos Islands
        "TCD" => Some("TD"), // Chad
        "TGO" => Some("TG"), // Togo
        "THA" => Some("TH"), // Thailand
        "TJK" => Some("TJ"), // Tajikistan
        "TKL" => Some("TK"), // Tokelau
        "TKM" => Some("TM"), // Turkmenistan
        "TLS" => Some("TL"), // Timor-Leste
        "TON" => Some("TO"), // Tonga
        "TTO" => Some("TT"), // Trinidad and Tobago
        "TUN" => Some("TN"), // Tunisia
        "TUR" => Some("TR"), // Türkiye
        "TUV" => Some("TV"), // Tuvalu
        "TWN" => Some("TW"), // Taiwan
        "TZA" => Some("TZ"), // Tanzania
        "UGA" => Some("UG"), // Uganda
        "UKR" => Some("UA"), // Ukraine
        "UMI" => Some("UM"), // U.S. Minor Outlying Islands
        "URY" => Some("UY"), // Uruguay
        "USA" => Some("US"), // United States
        "UZB" => Some("UZ"), // Uzbekistan
        "VAT" => Some("VA"), // Vatican City
        "VCT" => Some("VC"), // Saint Vincent and the Grenadines
        "VEN" => Some("VE"), // Venezuela
        "VGB" => Some("VG"), // British Virgin Islands
        "VIR" => Some("VI"), // U.S. Virgin Islands
        "VNM" => Some("VN"), // Vietnam
        "VUT" => Some("VU"), // Vanuatu
        "WLF" => Some("WF"), // Wallis and Futuna
        "WSM" => Some("WS"), // Samoa
        "YEM" => Some("YE"), // Yemen
        "ZAF" => Some("ZA"), // South Africa
        "ZMB" => Some("ZM"), // Zambia
        "ZWE" => Some("ZW"), // Zimbabwe
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Attribute helpers
// ---------------------------------------------------------------------------

/// Extract a string value from the `ArcGIS` candidate attributes map.
fn str_attr<'a>(
    attrs: &'a std::collections::HashMap<String, arcgis_geocoder::JsonValue>,
    key: &str,
) -> Option<&'a str> {
    attrs.get(key)?.as_str()
}

/// Extract a non-empty string value from the `ArcGIS` candidate attributes map.
fn nonempty_str_attr<'a>(
    attrs: &'a std::collections::HashMap<String, arcgis_geocoder::JsonValue>,
    key: &str,
) -> Option<&'a str> {
    str_attr(attrs, key).filter(|s| !s.is_empty())
}

// ---------------------------------------------------------------------------
// Site field helpers
// ---------------------------------------------------------------------------

/// Returns `true` if the named custom field is absent or empty on `site`.
fn cf_empty(site: &Site, field: &str) -> bool {
    site.custom_fields
        .as_ref()
        .and_then(|cf| cf.get(field))
        .and_then(|v| v.as_str())
        .is_none_or(str::is_empty)
}

/// Return the raw string value of a custom field, or `None` if absent/empty.
fn cf_str<'a>(site: &'a Site, field: &str) -> Option<&'a str> {
    site.custom_fields
        .as_ref()
        .and_then(|cf| cf.get(field))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header, method, path, query_param},
    };

    #[allow(clippy::too_many_arguments)] // test helper mirrors the JSON fixture shape
    fn site_json(
        id: i64,
        slug: &str,
        address: &str,
        country: &str,
        region: &str,
        city: &str,
        lat: Option<f64>,
        lon: Option<f64>,
    ) -> sonic_rs::Value {
        sonic_rs::json!({
            "id": id,
            "url": format!("https://nb.example.com/api/dcim/sites/{id}/"),
            "display_url": format!("https://nb.example.com/dcim/sites/{id}/"),
            "display": slug,
            "name": slug,
            "slug": slug,
            "status": {"value": "active", "label": "Active"},
            "region": null,
            "group": null,
            "tenant": null,
            "facility": "",
            "time_zone": null,
            "description": "",
            "physical_address": address,
            "shipping_address": "",
            "latitude": lat,
            "longitude": lon,
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

    fn arcgis_candidates_json(
        address: &str,
        score: f64,
        country_code: &str,
        region_abbr: &str,
        city: &str,
        x: f64,
        y: f64,
    ) -> sonic_rs::Value {
        sonic_rs::json!({
            "candidates": [{
                "address": address,
                "score": score,
                "location": {"x": x, "y": y},
                "attributes": {
                    "CountryCode": country_code,
                    "Region": "California",
                    "RegionAbbr": region_abbr,
                    "City": city
                }
            }]
        })
    }

    /// Round-trip a `sonic_rs::Value` through JSON into a `Site`.
    /// `sonic_rs::from_value` doesn't deserialize structs derived via serde
    /// reliably; serializing to a string first sidesteps that.
    fn site_from_value(v: &sonic_rs::Value) -> sonic_rs::Result<Site> {
        let s = sonic_rs::to_string(v).expect("serialize sonic_rs::Value");
        sonic_rs::from_str(&s)
    }

    /// Build a `GeocoderClient` pointed at `server_uri/arcgis/rest/services/World/GeocodeServer`.
    fn geocoder_for(server_uri: &str) -> GeocoderClient {
        GeocoderClient::builder()
            .base_url(format!(
                "{server_uri}/arcgis/rest/services/World/GeocodeServer"
            ))
            .build("test-token")
            .unwrap()
    }

    fn netbox_for(server_uri: &str) -> Netbox {
        Netbox::new(server_uri, "test-token").unwrap()
    }

    // ── fill_missing — all fields empty ─────────────────────────────────────

    /// When all geo fields are empty, `fill_missing` geocodes the address,
    /// sends a PATCH, and returns the updated site.
    #[tokio::test]
    async fn fill_missing_geocodes_and_patches_when_all_fields_empty() {
        let server = MockServer::start().await;

        // Serve the ArcGIS response.
        Mock::given(method("GET"))
            .and(path(
                "/arcgis/rest/services/World/GeocodeServer/findAddressCandidates",
            ))
            .and(query_param("singleLine", "1 Infinite Loop, Cupertino, CA"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(
                    sonic_rs::to_vec(&arcgis_candidates_json(
                        "1 Infinite Loop, Cupertino, CA 95014",
                        98.0,
                        "USA",
                        "CA",
                        "Cupertino",
                        -122.0308,
                        37.3318,
                    ))
                    .unwrap(),
                    "application/json",
                ),
            )
            .expect(1)
            .mount(&server)
            .await;

        // Serve the updated site after PATCH.
        let updated = site_json(
            1,
            "hq",
            "1 Infinite Loop, Cupertino, CA",
            "US",
            "CA",
            "Cupertino",
            Some(37.3318),
            Some(-122.0308),
        );
        Mock::given(method("PATCH"))
            .and(path("/api/dcim/sites/1/"))
            .and(header("Authorization", "Token test-token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(sonic_rs::to_vec(&updated).unwrap(), "application/json"),
            )
            .expect(1)
            .mount(&server)
            .await;

        let site: Site = site_from_value(&site_json(
            1,
            "hq",
            "1 Infinite Loop, Cupertino, CA",
            "",
            "",
            "",
            None,
            None,
        ))
        .unwrap();

        let geocoder = geocoder_for(&server.uri());
        let netbox = netbox_for(&server.uri());

        let result = fill_missing(&site, &geocoder, &netbox, 85.0, false)
            .await
            .unwrap();

        assert!(result.is_some(), "should return updated site");
        let updated_site = result.unwrap();
        assert_eq!(updated_site.latitude, Some(37.3318));
    }

    // ── fill_missing — all fields already populated ──────────────────────────

    /// When all geo fields are already populated, `fill_missing` must not call
    /// the geocoder or send a PATCH.
    #[tokio::test]
    async fn fill_missing_noop_when_all_fields_populated() {
        let server = MockServer::start().await;

        // No mocks registered — any unexpected request will fail the test.

        let site: Site = site_from_value(&site_json(
            2,
            "nyc1",
            "100 Main St, New York, NY",
            "US",
            "NY",
            "New York",
            Some(40.7128),
            Some(-74.006),
        ))
        .unwrap();

        let geocoder = geocoder_for(&server.uri());
        let netbox = netbox_for(&server.uri());

        let result = fill_missing(&site, &geocoder, &netbox, 85.0, false)
            .await
            .unwrap();

        assert!(result.is_none(), "should return None when nothing to fill");
    }

    // ── fill_missing — low score discarded ──────────────────────────────────

    /// A candidate below `min_score` must be ignored; no PATCH is sent.
    #[tokio::test]
    async fn fill_missing_skips_low_score_candidate() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path(
                "/arcgis/rest/services/World/GeocodeServer/findAddressCandidates",
            ))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(
                    sonic_rs::to_vec(&arcgis_candidates_json(
                        "Ambiguous Address",
                        60.0, // below default 85.0
                        "USA",
                        "CA",
                        "Somewhere",
                        0.0,
                        0.0,
                    ))
                    .unwrap(),
                    "application/json",
                ),
            )
            .expect(1)
            .mount(&server)
            .await;

        let site: Site = site_from_value(&site_json(
            3,
            "site3",
            "Ambiguous Address",
            "",
            "",
            "",
            None,
            None,
        ))
        .unwrap();

        let geocoder = geocoder_for(&server.uri());
        let netbox = netbox_for(&server.uri());

        let result = fill_missing(&site, &geocoder, &netbox, 85.0, false)
            .await
            .unwrap();

        assert!(result.is_none(), "low-score candidate should be discarded");
    }

    // ── fill_missing — no_write mode ────────────────────────────────────────

    /// Under `--no-write`, geocoder is called but no PATCH is sent; returns None.
    #[tokio::test]
    async fn fill_missing_no_write_prints_without_patching() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path(
                "/arcgis/rest/services/World/GeocodeServer/findAddressCandidates",
            ))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(
                    sonic_rs::to_vec(&arcgis_candidates_json(
                        "1 Infinite Loop, Cupertino, CA 95014",
                        95.0,
                        "USA",
                        "CA",
                        "Cupertino",
                        -122.0308,
                        37.3318,
                    ))
                    .unwrap(),
                    "application/json",
                ),
            )
            .expect(1)
            .mount(&server)
            .await;

        // No PATCH mock — any PATCH would fail the test.

        let site: Site = site_from_value(&site_json(
            4,
            "hq",
            "1 Infinite Loop, Cupertino, CA",
            "",
            "",
            "",
            None,
            None,
        ))
        .unwrap();

        let geocoder = geocoder_for(&server.uri());
        let netbox = netbox_for(&server.uri());

        let result = fill_missing(&site, &geocoder, &netbox, 85.0, true /* no_write */)
            .await
            .unwrap();

        // no_write returns None (no site was mutated in NetBox).
        assert!(result.is_none());
    }

    // ── normalise_country_code ───────────────────────────────────────────────

    #[test]
    fn normalise_alpha2_passthrough() {
        assert_eq!(normalize_country_code("US"), Some("US".to_owned()));
        assert_eq!(normalize_country_code("gb"), Some("GB".to_owned()));
    }

    #[test]
    fn normalise_alpha3_converts() {
        assert_eq!(normalize_country_code("USA"), Some("US".to_owned()));
        assert_eq!(normalize_country_code("GBR"), Some("GB".to_owned()));
        assert_eq!(normalize_country_code("DEU"), Some("DE".to_owned()));
    }

    #[test]
    fn normalise_unknown_returns_none() {
        assert_eq!(normalize_country_code("XYZ"), None);
        assert_eq!(normalize_country_code("USAB"), None);
    }

    // ── cf_empty ─────────────────────────────────────────────────────────────

    #[test]
    fn cf_empty_detects_missing_and_blank() {
        let site: Site =
            site_from_value(&site_json(1, "s", "addr", "", "", "", None, None)).unwrap();
        assert!(cf_empty(&site, "geofeed_country"));
        assert!(cf_empty(&site, "geofeed_region"));

        let site2: Site = site_from_value(&site_json(
            2,
            "s",
            "addr",
            "US",
            "CA",
            "LA",
            Some(1.0),
            Some(2.0),
        ))
        .unwrap();
        assert!(!cf_empty(&site2, "geofeed_country"));
    }
}
