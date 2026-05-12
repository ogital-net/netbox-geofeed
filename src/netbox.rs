//! Thin wrapper around `netbox-client` providing geofeed-specific queries.
//!
//! The [`Netbox`] struct is instantiated once per run and holds a site cache
//! to avoid redundant HTTP requests when multiple prefixes share the same site.

// NetBox page results are always far smaller than 2^32 items.
#![allow(clippy::cast_possible_truncation)]
// Public API consumed by generate.rs and geocode.rs.
#![allow(dead_code)]

use std::collections::HashMap;

use anyhow::Context as _;
use futures_util::TryStreamExt as _;
use futures_util::stream::{BoxStream, try_unfold};
use netbox_client::{
    Aggregate, NetboxClient, Prefix, Site, SitePatchRequest,
    dcim::SiteFilter,
    ipam::{AggregateFilter, PrefixFilter},
};

const PAGE_SIZE: u32 = 50;

/// A per-run memoization cache for [`Site`] lookups by numeric ID.
///
/// Kept separate from [`Netbox`] so callers can hold the cache independently,
/// allowing [`Netbox::site`] to take `&self` rather than `&mut self`.
pub struct SitesCache(HashMap<i64, Site>);

impl SitesCache {
    #[must_use]
    pub fn new() -> Self {
        Self(HashMap::new())
    }

    /// Insert or replace a site in the cache.
    ///
    /// Call this after a successful [`Netbox::site_patch`] to keep the cache
    /// consistent for the remainder of the run.
    pub fn update(&mut self, site: Site) {
        self.0.insert(site.id, site);
    }

    /// Look up a site by ID without mutating the cache.
    #[must_use]
    pub fn get(&self, id: i64) -> Option<&Site> {
        self.0.get(&id)
    }
}

impl Default for SitesCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Per-run `NetBox` client.
pub struct Netbox {
    client: NetboxClient,
}

impl Netbox {
    /// Construct a new instance from a base URL and API token.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying HTTP client cannot be constructed.
    pub fn new(url: &str, token: &str) -> anyhow::Result<Self> {
        let client = NetboxClient::new(url, token).context("failed to construct NetBox client")?;
        Ok(Self { client })
    }

    /// Stream every active, `dcim.site`-scoped prefix.
    ///
    /// `status=active` is applied server-side; `scope_type=dcim.site` is
    /// verified client-side on each returned record.
    pub fn prefixes_active_site_scoped(&self) -> BoxStream<'_, anyhow::Result<Prefix>> {
        use std::collections::VecDeque;

        Box::pin(try_unfold(
            (Some(0u32), VecDeque::<Prefix>::new()),
            move |(mut next_offset, mut buf)| async move {
                loop {
                    // Yield the next already-buffered site-scoped prefix.
                    while let Some(item) = buf.pop_front() {
                        if item.scope_type.as_deref() == Some("dcim.site") {
                            return Ok(Some((item, (next_offset, buf))));
                        }
                        // Non-site-scoped — discard and check next.
                    }

                    // Buffer exhausted; fetch the next page.
                    let Some(offset) = next_offset else {
                        return Ok(None);
                    };
                    let filter = PrefixFilter {
                        status: vec!["active".to_owned()],
                        ..Default::default()
                    };
                    let page = self
                        .client
                        .prefixes_list(PAGE_SIZE, offset, &filter)
                        .await
                        .map_err(anyhow::Error::from)?;
                    next_offset = page
                        .next
                        .is_some()
                        .then_some(offset + page.results.len() as u32);
                    buf = page.results.into_iter().collect();
                }
            },
        ))
    }

    /// Stream every aggregate in `NetBox` (unfiltered).
    pub fn aggregates_all(&self) -> BoxStream<'_, anyhow::Result<Aggregate>> {
        // The stream borrows the filter for its lifetime, so it must outlive
        // this stack frame. A function-local static provides a 'static reference
        // without any per-call allocation.
        static FILTER: std::sync::LazyLock<AggregateFilter> =
            std::sync::LazyLock::new(AggregateFilter::default);
        Box::pin(self.client.aggregates(&FILTER).map_err(anyhow::Error::from))
    }

    /// Fetch a site by ID, memoizing the result in `cache`.
    ///
    /// The cache lifetime is independent of `self`, so this method takes
    /// `&self` and can be called without a mutable borrow on [`Netbox`].
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP request fails or the site does not exist.
    pub async fn site<'cache>(
        &self,
        id: i64,
        cache: &'cache mut SitesCache,
    ) -> anyhow::Result<&'cache Site> {
        if let std::collections::hash_map::Entry::Vacant(e) = cache.0.entry(id) {
            let site = self
                .client
                .site(id)
                .await
                .with_context(|| format!("failed to fetch site {id}"))?;
            e.insert(site);
        }
        Ok(cache.0.get(&id).expect("just inserted"))
    }

    /// PATCH a site with the supplied partial-update body.
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP request fails or the server rejects the body.
    pub async fn site_patch(&self, id: i64, body: &SitePatchRequest) -> anyhow::Result<Site> {
        self.client
            .site_patch(id, body)
            .await
            .with_context(|| format!("failed to PATCH site {id}"))
    }

    /// Stream all sites matching `filter` from `NetBox`.
    ///
    /// Delegates to the underlying `netbox-client` paginated stream and
    /// converts errors to [`anyhow::Error`].
    pub fn sites_stream<'a>(
        &'a self,
        filter: &'a SiteFilter,
    ) -> BoxStream<'a, anyhow::Result<Site>> {
        Box::pin(self.client.sites(filter).map_err(anyhow::Error::from))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header, method, path, query_param},
    };

    fn prefix_json(
        id: i64,
        prefix_cidr: &str,
        scope_type: Option<&str>,
        scope_id: Option<i64>,
    ) -> sonic_rs::Value {
        sonic_rs::json!({
            "id": id,
            "url": format!("https://nb.example.com/api/ipam/prefixes/{id}/"),
            "display_url": format!("https://nb.example.com/ipam/prefixes/{id}/"),
            "display": prefix_cidr,
            "family": {"value": 4, "label": "IPv4"},
            "prefix": prefix_cidr,
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

    fn aggregate_json(id: i64, prefix_cidr: &str) -> sonic_rs::Value {
        sonic_rs::json!({
            "id": id,
            "url": format!("https://nb.example.com/api/ipam/aggregates/{id}/"),
            "display_url": format!("https://nb.example.com/ipam/aggregates/{id}/"),
            "display": prefix_cidr,
            "family": {"value": 4, "label": "IPv4"},
            "prefix": prefix_cidr,
            "rir": {
                "id": 1,
                "url": "https://nb.example.com/api/ipam/rirs/1/",
                "display": "ARIN",
                "name": "ARIN",
                "slug": "arin",
                "description": "",
                "aggregate_count": 5
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

    fn site_json(id: i64, name: &str, slug: &str) -> sonic_rs::Value {
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
            "custom_fields": {},
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

    // ── prefixes ─────────────────────────────────────────────────────────────

    /// Two pages of prefixes containing a mix of site-scoped and non-site-scoped
    /// entries; asserts only `scope_type=dcim.site` records are yielded.
    #[tokio::test]
    async fn prefixes_stream_filters_to_site_scoped_across_two_pages() {
        let server = MockServer::start().await;

        // Page 1: 2 site-scoped, 1 non-site-scoped (dcim.rack).
        let page1 = sonic_rs::json!({
            "count": 4,
            "next": format!("{}/api/ipam/prefixes/?limit=50&offset=3", server.uri()),
            "previous": null,
            "results": [
                prefix_json(1, "10.0.0.0/24", Some("dcim.site"), Some(1)),
                prefix_json(2, "10.0.1.0/24", Some("dcim.rack"), Some(99)),
                prefix_json(3, "10.0.2.0/24", Some("dcim.site"), Some(2)),
            ]
        });
        // Page 2: 1 site-scoped.
        let page2 = sonic_rs::json!({
            "count": 4,
            "next": null,
            "previous": null,
            "results": [
                prefix_json(4, "10.0.3.0/24", Some("dcim.site"), Some(3)),
            ]
        });

        Mock::given(method("GET"))
            .and(path("/api/ipam/prefixes/"))
            .and(query_param("status", "active"))
            .and(query_param("offset", "0"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(sonic_rs::to_vec(&page1).unwrap(), "application/json"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/ipam/prefixes/"))
            .and(query_param("status", "active"))
            .and(query_param("offset", "3"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(sonic_rs::to_vec(&page2).unwrap(), "application/json"),
            )
            .mount(&server)
            .await;

        let nb = Netbox::new(&server.uri(), "token").unwrap();
        let prefixes: Vec<Prefix> = nb
            .prefixes_active_site_scoped()
            .try_collect()
            .await
            .unwrap();

        assert_eq!(prefixes.len(), 3);
        assert_eq!(prefixes[0].prefix, "10.0.0.0/24");
        assert_eq!(prefixes[1].prefix, "10.0.2.0/24");
        assert_eq!(prefixes[2].prefix, "10.0.3.0/24");
    }

    /// All prefixes on both pages are non-site-scoped; stream yields nothing.
    #[tokio::test]
    async fn prefixes_stream_empty_when_no_site_scoped() {
        let server = MockServer::start().await;

        let page = sonic_rs::json!({
            "count": 1,
            "next": null,
            "previous": null,
            "results": [
                prefix_json(1, "10.0.0.0/8", Some("dcim.rack"), Some(5)),
            ]
        });

        Mock::given(method("GET"))
            .and(path("/api/ipam/prefixes/"))
            .and(query_param("status", "active"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(sonic_rs::to_vec(&page).unwrap(), "application/json"),
            )
            .mount(&server)
            .await;

        let nb = Netbox::new(&server.uri(), "token").unwrap();
        let prefixes: Vec<Prefix> = nb
            .prefixes_active_site_scoped()
            .try_collect()
            .await
            .unwrap();
        assert!(prefixes.is_empty());
    }

    // ── aggregates ───────────────────────────────────────────────────────────

    /// All aggregates across two pages are yielded in order.
    #[tokio::test]
    async fn aggregates_stream_walks_two_pages() {
        let server = MockServer::start().await;

        let page1 = sonic_rs::json!({
            "count": 2,
            "next": format!("{}/api/ipam/aggregates/?limit=50&offset=1", server.uri()),
            "previous": null,
            "results": [aggregate_json(1, "10.0.0.0/8")]
        });
        let page2 = sonic_rs::json!({
            "count": 2,
            "next": null,
            "previous": null,
            "results": [aggregate_json(2, "172.16.0.0/12")]
        });

        Mock::given(method("GET"))
            .and(path("/api/ipam/aggregates/"))
            .and(query_param("offset", "0"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(sonic_rs::to_vec(&page1).unwrap(), "application/json"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/ipam/aggregates/"))
            .and(query_param("offset", "1"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(sonic_rs::to_vec(&page2).unwrap(), "application/json"),
            )
            .mount(&server)
            .await;

        let nb = Netbox::new(&server.uri(), "token").unwrap();
        let aggs: Vec<Aggregate> = nb.aggregates_all().try_collect().await.unwrap();

        assert_eq!(aggs.len(), 2);
        assert_eq!(aggs[0].prefix, "10.0.0.0/8");
        assert_eq!(aggs[1].prefix, "172.16.0.0/12");
    }

    // ── site memoization ─────────────────────────────────────────────────────

    /// Two calls to `site(id)` produce one HTTP request; the returned slug is
    /// identical on both calls.
    #[tokio::test]
    async fn site_memoizes_across_two_calls() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/dcim/sites/42/"))
            .and(header("Authorization", "Token token"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                sonic_rs::to_vec(&site_json(42, "New York", "nyc")).unwrap(),
                "application/json",
            ))
            .expect(1) // exactly one HTTP request despite two Rust calls
            .mount(&server)
            .await;

        let nb = Netbox::new(&server.uri(), "token").unwrap();
        let mut cache = SitesCache::new();
        let s1 = nb.site(42, &mut cache).await.unwrap();
        assert_eq!(s1.slug, "nyc");
        let s2 = nb.site(42, &mut cache).await.unwrap();
        assert_eq!(s2.slug, "nyc");
        // MockServer drop verifies the expect(1) assertion.
    }

    // ── site PATCH ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn site_patch_sends_patch_and_returns_updated_site() {
        let server = MockServer::start().await;

        Mock::given(method("PATCH"))
            .and(path("/api/dcim/sites/7/"))
            .and(header("Authorization", "Token token"))
            .and(header("Content-Type", "application/json"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                sonic_rs::to_vec(&site_json(7, "London", "lhr")).unwrap(),
                "application/json",
            ))
            .mount(&server)
            .await;

        let nb = Netbox::new(&server.uri(), "token").unwrap();
        let body = SitePatchRequest {
            latitude: Some(51.5),
            longitude: Some(-0.1),
            ..Default::default()
        };
        let site = nb.site_patch(7, &body).await.unwrap();
        assert_eq!(site.id, 7);
        assert_eq!(site.slug, "lhr");
    }
}
