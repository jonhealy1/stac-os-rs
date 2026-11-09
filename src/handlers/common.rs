// src/handlers/common.rs
use crate::links::LinkEngine;
use crate::store::{NodeKind, Store, CATALOGS_INDEX, COLLECTIONS_INDEX, ROOT_CATALOG_ID};
use axum::{
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::json;
use stac::{Catalog, Collection, Item, Link};
use std::collections::{HashMap, HashSet};

/// `Json` extractor that maps every rejection to `400 Bad Request` — STAC
/// returns 400, not axum's default 422, for malformed request bodies.
pub struct StacJson<T>(pub T);

impl<S, T> axum::extract::FromRequest<S> for StacJson<T>
where
    S: Send + Sync,
    T: serde::de::DeserializeOwned,
{
    type Rejection = ApiError;

    async fn from_request(req: axum::extract::Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(value)) => Ok(Self(value)),
            Err(rejection) => Err(ApiError::BadRequest(rejection.body_text())),
        }
    }
}

pub struct AppState {
    pub base_url: String,
    pub store: Store,
    pub links: LinkEngine,
    pub enable_transactions: bool,
    /// When true, poly-hierarchy alternates (extra `parent` links on
    /// catalogs, `related`/`duplicate` on collections) are suppressed.
    pub hide_alternate_parents: bool,
    /// `ENABLE_COLLECTIONS_SEARCH` (default true upstream) — when false,
    /// GET `/collections` ignores the search params (`sortby`, `fields`,
    /// `q`, `filter`, `query`, `datetime`, `bbox`) and is a plain listing.
    pub enable_collections_search: bool,
    /// `ENABLE_COLLECTIONS_SEARCH_ROUTE` (default false upstream) —
    /// mounts the GET/POST `/collections-search` route.
    pub enable_collections_search_route: bool,
    /// `ENABLE_CATALOGS_ROUTE` — mounts the `/catalogs/**` surface
    /// (registry, scoped collections/items/search, catalog writes).
    /// Opt-out here (default on); upstream's flag is opt-in (default
    /// off). When false the catalogs links/conformance URIs drop off
    /// the landing page and `related`/`duplicate` scoped links are
    /// suppressed on global collection reads (they would 404).
    pub enable_catalogs_route: bool,
    /// `STAC_DEFAULT_COLLECTION_LIMIT` — `/collections` page size when
    /// `?limit=` is absent.
    pub default_collection_limit: u64,
    /// `STAC_GLOBAL_COLLECTION_MAX_LIMIT` — hard cap on collection page
    /// size (`None` = uncapped).
    pub max_collection_limit: Option<u64>,
    /// `STAC_DEFAULT_ITEM_LIMIT` / `STAC_GLOBAL_ITEM_MAX_LIMIT` — same
    /// knobs for item listings.
    pub default_item_limit: u64,
    pub max_item_limit: Option<u64>,
    /// `EXCLUDED_FROM_ITEMS` — field paths stripped from every item
    /// response (e.g. `private_data,properties.private_data`).
    pub excluded_item_fields: Vec<String>,
    /// `EXCLUDED_FROM_QUERYABLES` — field prefixes omitted from the
    /// `/queryables` schema.
    pub excluded_queryables: Vec<String>,
    /// `ENABLE_STAC_VALIDATOR` — when non-empty, write bodies
    /// (POST/PUT, and the merged result of PATCH) are validated against
    /// STAC JSON schemas before they reach the store. Empty = validation
    /// off (upstream default). Pooled because `Validator::validate`
    /// takes `&mut self` — a single instance would serialize every
    /// validated write through one lock; `validate_stac_doc` grabs any
    /// free member and only waits when all are busy.
    pub validators: Vec<tokio::sync::Mutex<stac_validate::Validator>>,
    /// Round-robin cursor for `validate_stac_doc`'s all-busy fallback —
    /// queued waiters spread across the pool instead of piling onto
    /// `validators[0]`.
    pub validator_cursor: std::sync::atomic::AtomicUsize,
    /// `REQUIRE_IF_MATCH` — writes that accept `If-Match` require the
    /// header when true (428 when absent).
    pub require_if_match: bool,
    /// `RAISE_ON_BULK_ERROR` — batch writes (FeatureCollection POST,
    /// `/bulk_items`) abort on member failures when true; when false
    /// (upstream default) failed members are skipped and reported in
    /// the response.
    pub raise_on_bulk_error: bool,
    /// `ENABLE_TOPOLOGY_VALIDATION` — lightweight geometry checks on
    /// item writes: WGS84 bounds on all coordinates, polygon ring
    /// minimum/vertex ceiling, and antimeridian jumps (upstream's
    /// `validate_item_topology_lightweight`).
    pub enable_topology_validation: bool,
    /// `MAX_TOPOLOGY_VERTICES` — polygon ring vertex ceiling
    /// (upstream default 5000, DoS guard).
    pub max_topology_vertices: usize,
    /// `MAX_BATCH_ERROR_SIZE` — batch-validation error ceiling: more
    /// failures than this aborts the batch unconditionally
    /// ("Validation error threshold exceeded"). None = no cutoff.
    pub max_batch_error_size: Option<usize>,
}

impl AppState {
    /// Either collections-search flag on — gates `bbox_shape` injection
    /// at write time (upstream does the same check).
    pub fn collections_search_enabled(&self) -> bool {
        self.enable_collections_search || self.enable_collections_search_route
    }
}

/// `ENABLE_STAC_VALIDATOR` — validate a write body against the STAC JSON
/// schemas. No-op when validation is disabled.
pub(crate) async fn validate_stac_doc(
    state: &AppState,
    doc: &impl serde::Serialize,
) -> Result<(), ApiError> {
    if state.validators.is_empty() {
        return Ok(());
    }
    let value = serde_json::to_value(doc)
        .map_err(|e| ApiError::BadRequest(format!("STAC validation failed: {e}")))?;
    guard_stac_version(&value)?;
    // `stac-validate` fetches `stac_extensions` schemas over the network
    // with no timeout — bound the whole call so one slow URI can't
    // stall the write. Grab any free pool member; only when all are
    // busy do we queue, round-robin across the pool.
    match tokio::time::timeout(VALIDATION_TIMEOUT, async {
        let mut free = state.validators.iter().find_map(|v| v.try_lock().ok());
        match free.take() {
            Some(mut guard) => guard.validate_value(value).await,
            None => {
                let i = state
                    .validator_cursor
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    % state.validators.len();
                state.validators[i].lock().await.validate_value(value).await
            }
        }
    })
    .await
    {
        Err(_) => Err(ApiError::BadGateway(format!(
            "STAC validation timed out after {}s",
            VALIDATION_TIMEOUT.as_secs()
        ))),
        Ok(Err(e)) => Err(validation_error(e)),
        Ok(Ok(_)) => Ok(()),
    }
}

/// `ENABLE_STAC_VALIDATOR` pool size — one `Validator` per mutex;
/// concurrent validated writes parallelize up to this many.
pub const VALIDATOR_POOL_SIZE: usize = 4;

/// Bound on a single `validate` call — covers the validator lock plus
/// any extension-schema fetches performed underneath it.
const VALIDATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// `stac-validate` builds its schema URI from `stac_version` and
/// `.unwrap()`s `Uri::parse` — an unknown version panics on URI-invalid
/// chars, or triggers a live fetch to schemas.stacspec.org. Mirror the
/// validator's type dispatch and reject unknown versions up front.
fn guard_stac_version(value: &serde_json::Value) -> Result<(), ApiError> {
    match value {
        serde_json::Value::Object(map) => match map.get("type").and_then(|v| v.as_str()) {
            Some("Feature" | "Catalog" | "Collection") => {
                if let Some(version) = map.get("stac_version").and_then(|v| v.as_str()) {
                    if matches!(
                        version.parse::<stac::Version>(),
                        Ok(stac::Version::Unknown(_))
                    ) {
                        return Err(ApiError::BadRequest(format!(
                            "unsupported stac_version '{version}'"
                        )));
                    }
                }
                Ok(())
            }
            Some("FeatureCollection") => {
                guard_stac_version(map.get("features").unwrap_or(&serde_json::Value::Null))
            }
            _ => guard_stac_version(map.get("collections").unwrap_or(&serde_json::Value::Null)),
        },
        serde_json::Value::Array(items) => items.iter().try_for_each(guard_stac_version),
        _ => Ok(()),
    }
}

/// Schema fetch/compile failures are upstream-service problems (502);
/// everything else is a bad document (400).
fn validation_error(e: stac_validate::Error) -> ApiError {
    match e {
        stac_validate::Error::Reqwest(_) | stac_validate::Error::JsonschemaValidation(_) => {
            ApiError::BadGateway(format!("STAC schema fetch failed: {e}"))
        }
        other => ApiError::BadRequest(format!("STAC validation failed: {other}")),
    }
}

/// Default page size for catalog/collection/children list endpoints.
pub(crate) const DEFAULT_LIST_LIMIT: u64 = 10;

/// STAC requires `application/geo+json` on Item and ItemCollection payloads.
pub(crate) fn geo_json<T: serde::Serialize>(body: T) -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "application/geo+json")], Json(body))
}

#[derive(Deserialize, Default, utoipa::IntoParams)]
pub struct ChildrenQuery {
    pub r#type: Option<String>, // Filter by "Catalog" or "Collection"
    pub limit: Option<u64>,
    /// Opaque pagination cursor (offset-encoded).
    pub token: Option<String>,
    /// CQL2 filter — parsed and validated like the other listings.
    pub filter: Option<String>,
    #[serde(rename = "filter-lang")]
    pub filter_lang: Option<String>,
}

/// Query params for catalog listings — only what `/catalogs` and
/// `/catalogs/{id}/catalogs` actually consume. Items-only params live on
/// `ListQuery`.
#[derive(Deserialize, Default, utoipa::IntoParams)]
pub struct CatalogsQuery {
    pub limit: Option<u64>,
    /// Opaque pagination cursor (offset-encoded).
    pub token: Option<String>,
    /// CQL2 filter — parsed and validated like the other listings.
    pub filter: Option<String>,
    #[serde(rename = "filter-lang")]
    pub filter_lang: Option<String>,
}

/// Query params for the scoped collections listing —
/// `/catalogs/{id}/collections` shares the collections-search surface.
#[derive(Deserialize, Default, utoipa::IntoParams)]
pub struct ScopedCollectionsQuery {
    pub limit: Option<u64>,
    /// Opaque pagination cursor (offset-encoded).
    pub token: Option<String>,
    /// Sort extension: `?sortby=+field,-field`.
    pub sortby: Option<String>,
    /// Fields extension: `?fields=+include,-exclude`.
    pub fields: Option<String>,
    /// Free-text search: `?q=term`.
    pub q: Option<String>,
    /// CQL2 filter — parsed and validated like the other listings.
    pub filter: Option<String>,
    #[serde(rename = "filter-lang")]
    pub filter_lang: Option<String>,
}

#[derive(Deserialize, Default, utoipa::IntoParams)]
pub struct ListQuery {
    pub limit: Option<u64>,
    pub token: Option<String>,
    /// Sort extension (features binding): `?sortby=+field,-field`.
    pub sortby: Option<String>,
    /// Fields extension (features binding): `?fields=+include,-exclude`.
    pub fields: Option<String>,
    /// Free-text search (collections-search binding): `?q=term`.
    pub q: Option<String>,
    // Items-listing filters — consumed by `items_page` only; silently
    // ignored on catalog/collection listings. `intersects` is
    // deliberately absent (unaccepted on the items binding).
    pub ids: Option<String>,
    pub query: Option<String>,
    pub filter: Option<String>,
    #[serde(rename = "filter-lang")]
    pub filter_lang: Option<String>,
    pub bbox: Option<String>,
    pub datetime: Option<String>,
}

/// 400 for a CQL2 translation error. Upstream surfaces `to_es`
/// ValueErrors verbatim under `Error with cql2 filter:` — messages
/// already carrying that prefix pass through; everything else gets the
/// generic `Invalid filter parameter:` framing.
pub(crate) fn bad_filter(e: String) -> ApiError {
    if e.starts_with("Error with cql2 filter:") {
        ApiError::BadRequest(e)
    } else {
        ApiError::BadRequest(format!("Invalid filter parameter: {e}."))
    }
}

/// Upstream listing-route filter handling: `filter`/`filter-lang` is
/// parsed, validated, and translated to an OpenSearch clause. With
/// `cql2-text` or no `filter-lang` a JSON string is tried first, then
/// CQL2 text — the same "try both" order upstream uses on listing
/// routes. Returns None when no filter was given.
pub(crate) fn listing_filter_clause(
    filter: Option<&str>,
    filter_lang: Option<&str>,
    spec: crate::store::FieldSpec,
) -> Result<Option<serde_json::Value>, ApiError> {
    let Some(raw) = filter else { return Ok(None) };
    if let Some(lang) = filter_lang {
        if !matches!(lang, "cql2-json" | "cql2-text") {
            return Err(ApiError::BadRequest(format!(
                "Only 'cql2-json' and 'cql2-text' filter languages are supported for collections. Got '{lang}'."
            )));
        }
    }
    let invalid = |msg: &str| ApiError::BadRequest(format!("Invalid filter parameter: {msg}."));
    // Keep the text path as Cql2Text — `cql2_clause_with` uses it to
    // permit a single-element `IN ('x')` list, which the parser reports
    // as a scalar.
    let cql2: crate::api_types::Filter = if filter_lang == Some("cql2-json") {
        crate::api_types::Filter::Cql2Json(json_map(raw, invalid)?)
    } else {
        match serde_json::from_str::<serde_json::Value>(raw) {
            Ok(v) if v.is_object() => {
                crate::api_types::Filter::Cql2Json(v.as_object().cloned().unwrap())
            }
            Ok(_) => return Err(invalid("expected a CQL2 expression with an operator")),
            Err(_) => crate::api_types::Filter::Cql2Text(crate::store::rewrite_between(raw)),
        }
    };
    crate::store::cql2_clause_with(&cql2, spec)
        .map(Some)
        .map_err(bad_filter)
}

pub(crate) fn json_map(
    raw: &str,
    invalid: impl Fn(&str) -> ApiError,
) -> Result<serde_json::Map<String, serde_json::Value>, ApiError> {
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(v) if v.is_object() => Ok(v.as_object().cloned().unwrap()),
        Ok(_) => Err(invalid("expected a CQL2 expression with an operator")),
        Err(_) => Err(invalid("expected valid CQL2 JSON")),
    }
}

/// Resolve `?limit=&token=` into `(limit, offset)` — absent limit falls
/// back to the endpoint's configured default, then the configured max
/// caps it (`None` = uncapped). `limit=0` is a 400; an unparseable token
/// restarts from 0 (matching upstream leniency).
pub(crate) fn list_window(
    limit: Option<u64>,
    token: Option<&str>,
    default_limit: u64,
    max_limit: Option<u64>,
) -> Result<(u64, u64), ApiError> {
    let limit = limit
        .unwrap_or(default_limit)
        .min(max_limit.unwrap_or(u64::MAX));
    if limit == 0 {
        return Err(ApiError::BadRequest("limit must be >= 1".to_string()));
    }
    let offset = token.and_then(|t| t.parse::<u64>().ok()).unwrap_or(0);
    Ok((limit, offset))
}

/// 4- or 6-element bbox -> 2D (3D drops the altitude bounds, same as
/// upstream's bbox_shape extraction).
pub(crate) fn normalize_bbox(nums: Vec<f64>) -> Result<[f64; 4], ApiError> {
    match nums.len() {
        4 => Ok([nums[0], nums[1], nums[2], nums[3]]),
        6 => Ok([nums[0], nums[1], nums[3], nums[4]]),
        _ => Err(ApiError::BadRequest(
            "Invalid parameters provided: bbox".to_string(),
        )),
    }
}

/// Strip `EXCLUDED_FROM_ITEMS` paths from an item — applied before the
/// user's `fields` filter so server-side privacy always wins.
pub(crate) fn strip_excluded_item_fields(
    state: &AppState,
    obj: &mut serde_json::Map<String, serde_json::Value>,
) {
    if !state.excluded_item_fields.is_empty() {
        crate::store::filter_fields(obj, &[], &state.excluded_item_fields);
    }
}

/// `next` link for a paginated list response.
pub(crate) fn list_next_link(
    state: &AppState,
    path: &str,
    limit: u64,
    offset: u64,
    returned: usize,
    matched: usize,
    extra: Option<&str>,
) -> Option<serde_json::Value> {
    if offset as usize + returned >= matched {
        return None;
    }
    let mut href = format!(
        "{}{path}?limit={}&token={}",
        state.base_url,
        limit,
        offset + limit
    );
    if let Some(e) = extra {
        href.push_str(e);
    }
    Some(json!({ "rel": "next", "href": href }))
}

/// `&k=v&...` suffix for `list_next_link`'s `extra` param — percent-
/// encodes values so filters like `id LIKE 'a-%'` survive the link.
pub(crate) fn extra_params(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| {
            let enc: String = v
                .bytes()
                .map(|b| match b {
                    b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                        (b as char).to_string()
                    }
                    _ => format!("%{b:02X}"),
                })
                .collect();
            format!("&{k}={enc}")
        })
        .collect()
}

/// `next`-link `extra` carrying `filter`/`filter-lang` params forward.
pub(crate) fn filter_link_extra(filter: Option<&str>, filter_lang: Option<&str>) -> Option<String> {
    let mut pairs = Vec::new();
    if let Some(f) = filter {
        pairs.push(("filter", f));
    }
    if let Some(l) = filter_lang {
        pairs.push(("filter-lang", l));
    }
    (!pairs.is_empty()).then(|| extra_params(&pairs))
}

/// Raw query string minus `limit`/`token`, returned as a `&k=v…`
/// suffix — echoed verbatim into `next` hrefs so unrelated params
/// survive pagination (upstream `_next_query` behavior).
pub(crate) fn pagination_extra(raw: Option<&str>) -> Option<String> {
    let kept: Vec<&str> = raw?
        .split('&')
        .filter(|p| {
            let k = p.split('=').next().unwrap_or("");
            !k.is_empty() && !k.eq_ignore_ascii_case("limit") && !k.eq_ignore_ascii_case("token")
        })
        .collect();
    (!kept.is_empty()).then(|| format!("&{}", kept.join("&")))
}

/// Rels stripped on write — the API regenerates these on every read, so
/// stored copies would only be stale duplicates (upstream
/// `INFERRED_LINK_RELS`, applied via `resolve_links`).
pub(crate) const INFERRED_LINK_RELS: &[&str] =
    &["self", "item", "parent", "collection", "root", "items"];

/// Rels the API owns on reads (upstream `GENERATED_COLLECTION_RELS`):
/// a stored link with one of these rels is a copy of a generated link
/// and is dropped when the read generated that rel itself.
pub(crate) const GENERATED_RESOURCE_RELS: &[&str] = &[
    "queryables",
    "aggregate",
    "aggregations",
    "canonical",
    "duplicate",
    "http://www.opengis.net/def/rel/ogc/1.0/sortables",
];

/// `related` is dual-use: generated for poly-hierarchy parents, but also
/// a generic user rel for external resources (DOIs). A stored copy only
/// counts as generated when its href was generated or points inside
/// this API.
pub(crate) fn resolve_href(href: &str, base_url: &str) -> String {
    if href.starts_with("http://") || href.starts_with("https://") {
        href.to_string()
    } else {
        format!(
            "{}/{}",
            base_url.trim_end_matches('/'),
            href.trim_start_matches('/')
        )
    }
}

/// Write path (`resolve_links`): drop inferred rels and absolutize
/// relative hrefs against the advertised base URL. User links survive.
pub(crate) fn filter_stored_links(links: &mut Vec<Link>, base_url: &str) {
    filter_stored_links_rels(links, INFERRED_LINK_RELS, base_url);
}

/// Catalog writes additionally strip `child`/`children` (upstream's
/// `create_catalog`/`update_catalog` dynamic-link filter).
pub(crate) const INFERRED_CATALOG_LINK_RELS: &[&str] = &[
    "self",
    "item",
    "parent",
    "collection",
    "root",
    "items",
    "child",
    "children",
];

pub(crate) fn filter_stored_catalog_links(links: &mut Vec<Link>, base_url: &str) {
    filter_stored_links_rels(links, INFERRED_CATALOG_LINK_RELS, base_url);
}

pub(crate) fn filter_stored_links_rels(links: &mut Vec<Link>, rels: &[&str], base_url: &str) {
    links.retain(|l| !rels.contains(&l.rel.as_str()));
    for l in links.iter_mut() {
        l.href = resolve_href(&l.href, base_url);
    }
}

/// Stored user links for a read: inferred rels removed, hrefs resolved.
pub(crate) fn stored_user_links(doc: &serde_json::Value, base_url: &str) -> Vec<serde_json::Value> {
    doc["links"]
        .as_array()
        .map(|links| {
            links
                .iter()
                .filter(|l| !INFERRED_LINK_RELS.contains(&l["rel"].as_str().unwrap_or_default()))
                .map(|l| {
                    let mut l = l.clone();
                    if let Some(href) = l["href"].as_str() {
                        l["href"] = json!(resolve_href(href, base_url));
                    }
                    l
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Read path (`db_to_stac`): set `doc["links"]` to `generated` plus the
/// stored user links that aren't stale copies of generated ones —
/// rels the API owns (when this read generated them), dual-use
/// `related` pointing at generated/in-API hrefs, and exact duplicates.
pub(crate) fn merge_doc_links(doc: &mut serde_json::Value, generated: Vec<Link>, base_url: &str) {
    let mut out: Vec<serde_json::Value> = generated
        .into_iter()
        .map(|l| serde_json::to_value(l).unwrap_or_default())
        .collect();
    let gen_rels: std::collections::HashSet<String> = out
        .iter()
        .filter_map(|l| l["rel"].as_str().map(String::from))
        .collect();
    let gen_hrefs: std::collections::HashSet<String> = out
        .iter()
        .filter_map(|l| l["href"].as_str().map(String::from))
        .collect();
    for link in stored_user_links(doc, base_url) {
        let rel = link["rel"].as_str().unwrap_or_default();
        let href = link["href"].as_str().unwrap_or_default();
        if GENERATED_RESOURCE_RELS.contains(&rel) && gen_rels.contains(rel) {
            continue;
        }
        if rel == "related"
            && gen_rels.contains("related")
            && (gen_hrefs.contains(href) || href.starts_with(base_url))
        {
            continue;
        }
        if out
            .iter()
            .any(|g| g["rel"] == link["rel"] && g["href"] == link["href"])
        {
            continue;
        }
        out.push(link);
    }
    doc["links"] = json!(out);
}

/// STAC API - Children v1.0.0 invariants for the `/children` response:
/// the response `links` must include `root`, `parent`, and `self`, and
/// every child must carry a `self` link to its canonical location.
/// Invariant guard — violations are our bug, hence 500 (upstream's
/// `Children` pydantic model raises the same way on response build).
pub(crate) fn validate_children_response(
    children: &[serde_json::Value],
    links: &serde_json::Value,
) -> Result<(), ApiError> {
    let rels: HashSet<&str> = links
        .as_array()
        .map(|ls| ls.iter().filter_map(|l| l["rel"].as_str()).collect())
        .unwrap_or_default();
    for req in ["root", "parent", "self"] {
        if !rels.contains(req) {
            return Err(ApiError::Internal(format!(
                "children response links missing required rel='{req}'"
            )));
        }
    }
    if let Some(id) = children
        .iter()
        .find(|c| {
            !c["links"]
                .as_array()
                .map(|ls| ls.iter().any(|l| l["rel"].as_str() == Some("self")))
                .unwrap_or(false)
        })
        .and_then(|c| c["id"].as_str())
    {
        return Err(ApiError::Internal(format!(
            "child '{id}' missing required rel='self' link"
        )));
    }
    Ok(())
}

pub(crate) fn parse_kind(raw: Option<&str>) -> Option<NodeKind> {
    match raw {
        Some("Catalog") => Some(NodeKind::Catalog),
        Some("Collection") => Some(NodeKind::Collection),
        _ => None,
    }
}

/// 404 unless `catalog_id` is the implicit root or a catalog document —
/// a bare hierarchy node (e.g. a collection id) is not a catalog.
pub(crate) async fn require_catalog(state: &AppState, catalog_id: &str) -> Result<(), ApiError> {
    if catalog_id == ROOT_CATALOG_ID {
        return Ok(());
    }
    if state
        .store
        .get_document(CATALOGS_INDEX, catalog_id)
        .await?
        .is_some()
    {
        Ok(())
    } else {
        Err(ApiError::NotFound(catalog_id.to_string()))
    }
}

/// 404 unless `collection_id` lives somewhere inside `catalog_id`'s DAG.
pub(crate) async fn require_scoped_collection(
    state: &AppState,
    catalog_id: &str,
    collection_id: &str,
) -> Result<(), ApiError> {
    let allowed = state.store.get_descendant_collections(catalog_id).await?;
    if !allowed.contains(collection_id) {
        return Err(ApiError::NotFound(collection_id.to_string()));
    }
    Ok(())
}

pub(crate) fn link_json(href: String, rel: &str) -> Link {
    let mut link = Link::new(href, rel);
    link.r#type = Some("application/json".to_string());
    link
}

/// Dynamic links for an Item: `self`/`parent`/`collection`/`root`.
/// `self` and `parent` are scoped paths when a catalog scope is given,
/// canonical `/collections` paths otherwise.
pub(crate) fn item_links(
    state: &AppState,
    item_id: &str,
    collection_id: &str,
    scoped_catalog_id: Option<&str>,
) -> Vec<Link> {
    let base = &state.base_url;
    let (col_href, self_href) = match scoped_catalog_id {
        Some(cat) => (
            format!("{base}/catalogs/{cat}/collections/{collection_id}"),
            format!("{base}/catalogs/{cat}/collections/{collection_id}/items/{item_id}"),
        ),
        None => (
            format!("{base}/collections/{collection_id}"),
            format!("{base}/collections/{collection_id}/items/{item_id}"),
        ),
    };
    vec![
        link_json(self_href, "self"),
        link_json(col_href, "parent"),
        link_json(format!("{base}/collections/{collection_id}"), "collection"),
        link_json(format!("{base}/"), "root"),
    ]
}

/// STAC links for a catalog: one `parent` link per non-root parent
/// (upstream convention for poly-hierarchy); a root-level catalog gets a
/// single `parent` pointing at the landing page. Links are derived from
/// the DAG at read time — never stored.
/// Dynamic links for a Catalog doc. Upstream semantics: exactly ONE
/// `rel=parent` (first sorted real parent; landing page when none), the
/// rest as `rel=related` — unless hide_alternate_parents suppresses them.
/// A `rel=child` link is emitted per direct child.
pub(crate) async fn catalog_links(
    state: &AppState,
    catalog_id: &str,
    parents: &[String],
) -> Result<Vec<Link>, ApiError> {
    let base = &state.base_url;
    let mut links = vec![
        link_json(format!("{base}/catalogs/{catalog_id}"), "self"),
        link_json(base.clone(), "root"),
    ];
    let mut real: Vec<&str> = parents
        .iter()
        .map(String::as_str)
        .filter(|p| *p != ROOT_CATALOG_ID)
        .collect();
    real.sort_unstable();
    real.dedup();
    if real.is_empty() {
        links.push(link_json(base.clone(), "parent").title("Root Catalog".to_string()));
    } else {
        links.push(
            link_json(format!("{base}/catalogs/{}", real[0]), "parent").title(real[0].to_string()),
        );
        if !state.hide_alternate_parents {
            for p in &real[1..] {
                links.push(
                    link_json(format!("{base}/catalogs/{p}"), "related").title(p.to_string()),
                );
            }
        }
    }
    links.push(link_json(
        format!("{base}/catalogs/{catalog_id}/collections"),
        "data",
    ));
    links.push(link_json(
        format!("{base}/catalogs/{catalog_id}/children"),
        "children",
    ));
    links.extend(child_links(state, catalog_id).await?);
    let mut search = link_json(format!("{base}/catalogs/{catalog_id}/search"), "search");
    search.r#type = Some("application/geo+json".to_string());
    links.push(search);
    Ok(links)
}

/// `rel=child` links for a catalog's direct children, each scoped under
/// this catalog: `/catalogs/{id}/catalogs/{child}` and
/// `/catalogs/{id}/collections/{child}`. Nodes without a backing
/// document are skipped (no dead links); `title` is the doc title,
/// falling back to the child id.
pub(crate) async fn child_links(state: &AppState, catalog_id: &str) -> Result<Vec<Link>, ApiError> {
    let base = &state.base_url;
    let nodes = state.store.get_child_nodes(catalog_id, None).await?;
    let cat_ids: Vec<String> = nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Catalog)
        .map(|n| n.id.clone())
        .collect();
    let col_ids: Vec<String> = nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Collection)
        .map(|n| n.id.clone())
        .collect();
    // id -> title (absent entry = no backing doc → skip the child)
    let mut titles: HashMap<String, Option<String>> = HashMap::new();
    for docs in [
        state.store.get_documents(CATALOGS_INDEX, &cat_ids).await?,
        state
            .store
            .get_documents(COLLECTIONS_INDEX, &col_ids)
            .await?,
    ] {
        for d in docs {
            if let Some(id) = d["id"].as_str() {
                titles.insert(id.to_string(), d["title"].as_str().map(str::to_owned));
            }
        }
    }
    let mut seen = HashSet::new();
    let mut links = Vec::new();
    for node in nodes {
        let Some(title) = titles.get(&node.id) else {
            continue;
        };
        let href = match node.kind {
            NodeKind::Catalog => {
                format!("{base}/catalogs/{catalog_id}/catalogs/{}", node.id)
            }
            NodeKind::Collection => {
                format!("{base}/catalogs/{catalog_id}/collections/{}", node.id)
            }
        };
        if seen.insert(href.clone()) {
            links.push(
                link_json(href, "child").title(title.clone().unwrap_or_else(|| node.id.clone())),
            );
        }
    }
    Ok(links)
}

pub(crate) fn collection_links(
    state: &AppState,
    collection_id: &str,
    scoped_catalog_id: &str,
    parents: &[String],
) -> Vec<Link> {
    let mut links = state.links.format_scoped_collection_links(
        collection_id,
        scoped_catalog_id,
        parents,
        state.hide_alternate_parents,
    );
    // Filter extension: queryables link points at the canonical
    // resource even on scoped reads (upstream `link_queryables`).
    let mut q = link_json(
        format!("{}/collections/{collection_id}/queryables", state.base_url),
        "queryables",
    );
    q.r#type = Some("application/schema+json".to_string());
    links.push(q);
    links
}

/// Links for a collection read at the global `/collections/{id}` route:
/// `self` is the canonical URL, `parent` per non-root parent catalog
/// (landing page when root-level), `related`/`duplicate` expose the
/// scoped paths unless `hide_alternate_parents`.
pub(crate) fn global_collection_links(
    state: &AppState,
    collection_id: &str,
    parents: &[String],
) -> Vec<Link> {
    let base = &state.base_url;
    let mut links = vec![
        link_json(format!("{base}/collections/{collection_id}"), "self"),
        link_json(base.clone(), "root"),
        link_json(format!("{base}/collections/{collection_id}/items"), "items"),
    ];
    let mut sortables = link_json(
        format!("{base}/collections/{collection_id}/sortables"),
        "http://www.opengis.net/def/rel/ogc/1.0/sortables",
    );
    sortables.r#type = Some("application/schema+json".to_string());
    links.push(sortables);
    let mut q = link_json(
        format!("{base}/collections/{collection_id}/queryables"),
        "queryables",
    );
    q.r#type = Some("application/schema+json".to_string());
    links.push(q);
    // Upstream spec for global collection reads: exactly one `parent`,
    // pointing at the landing page; catalog memberships are exposed as
    // `related` (catalog) + `duplicate` (scoped URI) links instead —
    // only when the catalogs route is mounted, since those hrefs live
    // on the /catalogs surface.
    links.push(link_json(base.clone(), "parent").title("Root Catalog".to_string()));
    links.push(link_json(
        format!("{base}/collections/{collection_id}"),
        "canonical",
    ));
    let mut real: Vec<&str> = parents
        .iter()
        .map(String::as_str)
        .filter(|p| *p != ROOT_CATALOG_ID)
        .collect();
    real.sort_unstable();
    real.dedup();
    if state.enable_catalogs_route && !state.hide_alternate_parents {
        for p in &real {
            links.push(link_json(format!("{base}/catalogs/{p}"), "related").title(p.to_string()));
            links.push(
                link_json(
                    format!("{base}/catalogs/{p}/collections/{collection_id}"),
                    "duplicate",
                )
                .title(format!("Collection in catalog: {p}")),
            );
        }
    }
    links
}

/// Serialize a collection with global (canonical) links.
/// A document in the collections index counts as a collection only when
/// `type == "Collection"` — upstream's shared index stores catalogs there
/// too, so wrong-type docs read as missing on the collections surface.
pub(crate) fn is_collection_doc(doc: &serde_json::Value) -> bool {
    doc["type"].as_str() == Some("Collection")
}

pub(crate) async fn global_collection_doc(
    state: &AppState,
    collection_id: &str,
) -> Result<serde_json::Value, ApiError> {
    let mut doc = state
        .store
        .get_document(COLLECTIONS_INDEX, collection_id)
        .await?
        .filter(is_collection_doc)
        .ok_or_else(|| ApiError::NotFound(collection_id.to_string()))?;
    strip_collection_internals(&mut doc);
    let parents = state.store.get_parents(collection_id).await?;
    merge_doc_links(
        &mut doc,
        global_collection_links(state, collection_id, &parents),
        &state.base_url,
    );
    Ok(doc)
}

/// Serialize a catalog with its DAG-derived links injected — write
/// responses mirror what a subsequent GET would return.
/// Poly-hierarchy links only — `parent`/`related` per upstream's
/// `generate_poly_hierarchy_links` for `resource_type=Catalog`. This is
/// the link set write responses carry (GET responses add the full
/// structural set via `catalog_links`).
pub(crate) fn catalog_hierarchy_links(state: &AppState, parents: &[String]) -> Vec<Link> {
    let base = &state.base_url;
    let mut real: Vec<&str> = parents
        .iter()
        .map(String::as_str)
        .filter(|p| *p != ROOT_CATALOG_ID)
        .collect();
    real.sort_unstable();
    real.dedup();
    let mut links = Vec::new();
    if real.is_empty() {
        links.push(link_json(base.clone(), "parent").title("Root Catalog".to_string()));
    } else {
        links.push(
            link_json(format!("{base}/catalogs/{}", real[0]), "parent").title(real[0].to_string()),
        );
        if !state.hide_alternate_parents {
            for p in &real[1..] {
                links.push(
                    link_json(format!("{base}/catalogs/{p}"), "related").title(p.to_string()),
                );
            }
        }
    }
    links
}

/// Serialize a catalog for create/update responses — stored user links
/// plus poly-hierarchy `parent`/`related` links (upstream's write-path
/// shape: no `self`/`root`/`children`/`search` links).
pub(crate) async fn catalog_write_doc(
    state: &AppState,
    catalog: &Catalog,
) -> Result<serde_json::Value, ApiError> {
    let parents = state.store.get_parents(&catalog.id).await?;
    let mut doc = serde_json::to_value(catalog).map_err(|e| ApiError::Internal(e.to_string()))?;
    merge_doc_links(
        &mut doc,
        catalog_hierarchy_links(state, &parents),
        &state.base_url,
    );
    Ok(doc)
}

/// Serialize an item with canonical links injected — write responses
/// mirror what a subsequent GET would return.
/// Physical OpenSearch `_id` for an item document. Items are namespaced
/// per collection — `"{item_id}|{collection_id}"` (upstream `mk_item_id`)
/// — so the same item id may exist in different collections.
pub(crate) fn item_key(item_id: &str, collection_id: &str) -> String {
    format!("{item_id}|{collection_id}")
}

/// Derived search fields stored on a collection doc: `bbox_shape`, a
/// GeoJSON polygon of the first spatial extent bbox (upstream stores
/// the same derived field; it is stripped back out on reads).
pub(crate) fn collection_search_fields(doc: &mut serde_json::Map<String, serde_json::Value>) {
    if doc.contains_key("bbox_shape") {
        return;
    }
    let Some(first) = doc
        .get("extent")
        .and_then(|e| e.get("spatial"))
        .and_then(|s| s.get("bbox"))
        .and_then(|b| b.as_array())
        .and_then(|arr| arr.first().cloned())
    else {
        return;
    };
    let nums: Vec<f64> = match &first {
        serde_json::Value::Array(a) if a.first().is_some_and(|v| v.is_array()) => a[0]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_f64())
            .collect(),
        serde_json::Value::Array(a) => a.iter().filter_map(|v| v.as_f64()).collect(),
        _ => return,
    };
    let Ok([minx, miny, maxx, maxy]) = normalize_bbox(nums) else {
        return;
    };
    doc.insert(
        "bbox_shape".to_string(),
        json!({
            "type": "Polygon",
            "coordinates": [[[minx, miny], [maxx, miny], [maxx, maxy], [minx, maxy], [minx, miny]]]
        }),
    );
}

/// Drop derived storage fields (`bbox_shape`) — upstream pops them in
/// `db_to_stac`, they are search plumbing, not API surface.
pub(crate) fn strip_collection_internals(doc: &mut serde_json::Value) {
    if let Some(obj) = doc.as_object_mut() {
        obj.remove("bbox_shape");
    }
}

/// Collection doc as stored — serialized plus derived search fields
/// when collections-search is enabled (upstream skips `bbox_shape`
/// otherwise).
pub(crate) fn stored_collection_doc(
    state: &AppState,
    collection: &Collection,
) -> serde_json::Value {
    let mut doc = serde_json::to_value(collection).unwrap_or_default();
    // User links persist; inferred rels are stripped (upstream
    // `resolve_links`) since reads regenerate them.
    doc["links"] = json!(stored_user_links(&doc, &state.base_url));
    // `parent_ids` is DAG-managed — the hierarchy index is authoritative;
    // a client-supplied value is ignored, matching upstream.
    if let Some(obj) = doc.as_object_mut() {
        obj.remove("parent_ids");
    }
    if state.collections_search_enabled() {
        if let Some(obj) = doc.as_object_mut() {
            collection_search_fields(obj);
        }
    }
    doc
}

/// Strong ETag for a stored document version:
/// `"<primary_term>-<seq_no>-<indexhash>"`. When the response body is
/// compressed the response layer appends `-<coding>` inside the quotes;
/// `If-Match` comparison normalizes that suffix away (a tag shared by
/// gzip and plain bodies would be weak, RFC 9110 §8.8.1).
pub(crate) fn etag_of(state: &AppState, index: &str, v: &crate::store::DocVersion) -> String {
    format!(
        "\"{}-{}-{}\"",
        v.primary_term,
        v.seq_no,
        state.store.index_hash(index)
    )
}

/// The version portion of an ETag — drop a trailing `-<coding>` suffix.
pub(crate) fn etag_version_part(tag: &str) -> &str {
    let mut dashes = tag.match_indices('-');
    let _ = dashes.next();
    let _ = dashes.next();
    match dashes.next() {
        Some((i, _)) => &tag[..i],
        None => tag,
    }
}

/// RFC 9110 §13.1.1 strong comparison for `If-Match`: `*` or any list
/// element that is a well-formed strong tag equal to the current version
/// (coding suffix normalized). Weak (`W/"…"`) and malformed elements
/// never match; whitespace around elements is space/tab only.
pub(crate) fn if_match_holds(header_value: &str, current_etag: &str) -> bool {
    let current = current_etag.trim_matches('"');
    header_value.split(',').any(|raw| {
        let tag = raw.trim_matches(|c| c == ' ' || c == '\t');
        if tag == "*" {
            return true;
        }
        let Some(inner) = tag.strip_prefix('"').and_then(|t| t.strip_suffix('"')) else {
            return false;
        };
        etag_version_part(inner) == current
    })
}

/// 412 unless `If-Match` is absent or holds against `current_etag`.
/// With `REQUIRE_IF_MATCH=true` the header is mandatory (428 when
/// absent), matching upstream's conditional-write toggle.
pub(crate) fn require_if_match(
    state: &AppState,
    headers: &axum::http::HeaderMap,
    current_etag: &str,
) -> Result<(), ApiError> {
    match headers.get(header::IF_MATCH) {
        Some(v) if !if_match_holds(v.to_str().unwrap_or_default(), current_etag) => {
            Err(ApiError::PreconditionFailed)
        }
        None if state.require_if_match => Err(ApiError::PreconditionRequired(
            "If-Match header is required".to_string(),
        )),
        _ => Ok(()),
    }
}

/// Attach `etag` to a status+body response.
pub(crate) fn etag_response(
    status: StatusCode,
    etag: &str,
    body: impl IntoResponse,
) -> impl IntoResponse {
    (
        status,
        [(
            header::ETAG,
            axum::http::HeaderValue::from_str(etag)
                .unwrap_or_else(|_| axum::http::HeaderValue::from_static("\"invalid\"")),
        )],
        body,
    )
}

pub(crate) fn item_doc(
    state: &AppState,
    item: &Item,
    scoped_catalog_id: Option<&str>,
) -> serde_json::Value {
    let mut doc = serde_json::to_value(item).unwrap_or_default();
    if let Some(col) = item.collection.as_deref() {
        merge_doc_links(
            &mut doc,
            item_links(state, &item.id, col, scoped_catalog_id),
            &state.base_url,
        );
    }
    doc
}

/// Serialize a collection with links for the given scope context.
pub(crate) async fn collection_doc(
    state: &AppState,
    collection: &Collection,
    scoped_catalog_id: &str,
) -> Result<serde_json::Value, ApiError> {
    let parents = state.store.get_parents(&collection.id).await?;
    let mut doc =
        serde_json::to_value(collection).map_err(|e| ApiError::Internal(e.to_string()))?;
    merge_doc_links(
        &mut doc,
        collection_links(state, &collection.id, scoped_catalog_id, &parents),
        &state.base_url,
    );
    Ok(doc)
}

/// Opaque pagination token: url-safe base64 of a JSON `[offset]` —
/// upstream tokens are a base64'd JSON list too, so clients can't tell
/// the difference.
pub(crate) fn encode_item_token(offset: u64) -> String {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    URL_SAFE_NO_PAD.encode(format!("[{offset}]"))
}

pub(crate) fn decode_item_token(token: &str) -> Result<u64, ApiError> {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    let invalid = || ApiError::BadRequest("Invalid pagination token.".to_string());
    if token.is_empty() {
        return Err(invalid());
    }
    if let Ok(raw) = URL_SAFE_NO_PAD.decode(token) {
        return match serde_json::from_slice::<serde_json::Value>(&raw) {
            Ok(serde_json::Value::Array(parts)) => {
                parts.first().and_then(|v| v.as_u64()).ok_or_else(invalid)
            }
            _ => Err(invalid()),
        };
    }
    // Numeric fallback — older `next` links carried the raw offset.
    token.parse::<u64>().map_err(|_| invalid())
}

/// Apply a request body as a patch. `application/json-patch+json` + an
/// array body is RFC 6902 JSON-Patch; an object body under
/// `application/merge-patch+json` or `application/json` is RFC 7386
/// JSON Merge Patch (same split as upstream).
pub(crate) fn apply_patch(
    doc: &mut serde_json::Value,
    content_type: &str,
    patch: &serde_json::Value,
) -> Result<(), ApiError> {
    let content_type = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    match patch {
        serde_json::Value::Array(ops) if content_type == "application/json-patch+json" => {
            // Upstream patch semantics: a `copy`/`move` op that exactly
            // duplicates an earlier op in the same document is skipped,
            // so repeated copy chains read the already-patched scratch
            // value. Write ops (add/replace/remove/test) always run —
            // repeated `add` is how upstream's "reset" idiom works.
            let mut seen = std::collections::HashSet::new();
            let ops: Vec<serde_json::Value> = ops
                .iter()
                .filter(|op| {
                    !matches!(
                        op.get("op").and_then(serde_json::Value::as_str),
                        Some("copy" | "move")
                    ) || seen.insert(op.to_string())
                })
                .cloned()
                .collect();
            let ops: Vec<json_patch::PatchOperation> =
                serde_json::from_value(serde_json::Value::Array(ops))
                    .map_err(|e| ApiError::BadRequest(format!("invalid JSON-Patch: {e}")))?;
            json_patch::patch(doc, &ops)
                .map_err(|e| ApiError::BadRequest(format!("JSON-Patch failed: {e}")))
        }
        serde_json::Value::Object(_)
            if matches!(
                content_type.as_str(),
                "application/merge-patch+json" | "application/json"
            ) =>
        {
            merge_patch(doc, patch);
            Ok(())
        }
        _ => Err(ApiError::BadRequest(format!(
            "Content-Type '{content_type}' with this body is not a supported patch"
        ))),
    }
}

/// Upstream `patch_addresses_field`: whether any JSON-Patch op reads or
/// writes a top-level field — `path`/`from` first segment, or a root
/// (`""`) replacement whose value object contains the field.
pub(crate) fn patch_addresses_field(patch: &serde_json::Value, field: &str) -> bool {
    let Some(ops) = patch.as_array() else {
        return false;
    };
    fn first_seg(p: &str) -> &str {
        p.trim_matches('/').split('/').next().unwrap_or("")
    }
    for op in ops {
        let get = |k: &str| op.get(k).and_then(serde_json::Value::as_str).unwrap_or("");
        let path = get("path");
        if path.is_empty() {
            if op
                .get("value")
                .and_then(|v| v.as_object())
                .is_some_and(|o| o.contains_key(field))
            {
                return true;
            }
            continue;
        }
        if first_seg(path) == field
            || first_seg(get("from")) == field
            || first_seg(get("from_")) == field
        {
            return true;
        }
    }
    false
}

/// RFC 7386 JSON Merge Patch: `null` deletes the member, objects merge
/// recursively, any other value replaces wholesale.
pub(crate) fn merge_patch(target: &mut serde_json::Value, patch: &serde_json::Value) {
    match (target, patch) {
        (serde_json::Value::Object(target), serde_json::Value::Object(patch)) => {
            for (key, value) in patch {
                if value.is_null() {
                    target.remove(key);
                } else {
                    merge_patch(
                        target.entry(key.clone()).or_insert(serde_json::Value::Null),
                        value,
                    );
                }
            }
        }
        // Target member absent or scalar: materialize an object and merge,
        // so null members in the patch are dropped, not stored verbatim.
        (target, serde_json::Value::Object(patch)) => {
            let mut merged = serde_json::Map::new();
            for (key, value) in patch {
                if !value.is_null() {
                    let mut slot = serde_json::Value::Null;
                    merge_patch(&mut slot, value);
                    merged.insert(key.clone(), slot);
                }
            }
            *target = serde_json::Value::Object(merged);
        }
        (target, patch) => *target = patch.clone(),
    }
}

// --- Error Handling ---

pub enum ApiError {
    BadRequest(String),
    /// Structured `{"detail": <object>}` body — upstream raises
    /// `HTTPException(detail={...})` for strict bulk rejections, so
    /// `detail` is a dict (`message`/`errors`/`summary`), not a string.
    JsonDetail(StatusCode, serde_json::Value),
    /// Requested resources sit outside the scoped catalog (403).
    Forbidden(String),
    NotFound(String),
    Conflict(String),
    /// `If-Match` precondition failed.
    PreconditionFailed,
    /// `If-Match` header required but absent (428).
    PreconditionRequired(String),
    /// Optimistic-concurrency retries exhausted — retry the request.
    WriteConflict(String),
    /// An upstream-service dependency failed — e.g. a STAC extension
    /// schema could not be fetched or compiled (502).
    BadGateway(String),
    Internal(String),
}

impl From<opensearch::Error> for ApiError {
    fn from(e: opensearch::Error) -> Self {
        ApiError::Internal(e.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, detail) = match self {
            ApiError::BadRequest(msg) => (StatusCode::BAD_REQUEST, json!(msg)),
            ApiError::JsonDetail(status, detail) => (status, detail),
            ApiError::Forbidden(msg) => (StatusCode::FORBIDDEN, json!(msg)),
            ApiError::NotFound(id) => (
                StatusCode::NOT_FOUND,
                json!(format!("Resource '{id}' not found")),
            ),
            ApiError::Conflict(id) => (
                StatusCode::CONFLICT,
                json!(format!("Resource '{id}' already exists")),
            ),
            ApiError::PreconditionFailed => (
                StatusCode::PRECONDITION_FAILED,
                json!("If-Match precondition failed"),
            ),
            ApiError::PreconditionRequired(msg) => (StatusCode::PRECONDITION_REQUIRED, json!(msg)),
            ApiError::WriteConflict(id) => (
                StatusCode::CONFLICT,
                json!(format!(
                    "Resource '{id}' was modified concurrently — retry the request"
                )),
            ),
            ApiError::BadGateway(msg) => (StatusCode::BAD_GATEWAY, json!(msg)),
            ApiError::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, json!(msg)),
        };
        (status, Json(json!({ "detail": detail }))).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn children_links(rels: &[&str]) -> serde_json::Value {
        json!(rels
            .iter()
            .map(|r| json!({"rel": r, "href": "http://test/x"}))
            .collect::<Vec<_>>())
    }

    fn child_with_self(id: &str) -> serde_json::Value {
        json!({"id": id, "links": [{"rel": "self", "href": format!("http://test/catalogs/{id}")}]})
    }

    #[test]
    fn children_response_valid() {
        let children = vec![child_with_self("a")];
        let links = children_links(&["root", "parent", "self"]);
        assert!(validate_children_response(&children, &links).is_ok());
    }

    #[test]
    fn children_response_empty_children_ok() {
        // An empty children list still needs the response-level rels.
        let links = children_links(&["root", "parent", "self"]);
        assert!(validate_children_response(&[], &links).is_ok());
    }

    #[test]
    fn children_response_missing_response_rel_fails() {
        for missing in ["root", "parent", "self"] {
            let rels: Vec<&str> = ["root", "parent", "self"]
                .into_iter()
                .filter(|r| *r != missing)
                .collect();
            let err = validate_children_response(&[], &children_links(&rels)).unwrap_err();
            assert!(matches!(err, ApiError::Internal(msg) if msg.contains(missing)));
        }
    }

    #[test]
    fn children_response_child_missing_self_fails() {
        let child = json!({"id": "orphan", "links": [{"rel": "root", "href": "http://test"}]});
        let links = children_links(&["root", "parent", "self"]);
        let err = validate_children_response(&[child], &links).unwrap_err();
        assert!(matches!(err, ApiError::Internal(msg) if msg.contains("orphan")));
    }

    #[test]
    fn children_response_child_no_links_fails() {
        let child = json!({"id": "bare"});
        let links = children_links(&["root", "parent", "self"]);
        assert!(validate_children_response(&[child], &links).is_err());
    }

    #[test]
    fn stac_version_guard_allows_known_versions() {
        for version in ["1.0.0", "1.1.0", "1.1.0-beta.1"] {
            let doc = json!({"type": "Feature", "stac_version": version});
            assert!(guard_stac_version(&doc).is_ok(), "{version}");
        }
    }

    #[test]
    fn stac_version_guard_rejects_unknown_versions() {
        // URI-invalid chars would panic `Uri::parse().unwrap()` upstream;
        // plausible-looking versions would trigger a live schema fetch.
        for version in ["a b", "2.0.0", "1.0", "{}"] {
            let doc = json!({"type": "Feature", "stac_version": version});
            assert!(guard_stac_version(&doc).is_err(), "{version}");
        }
    }

    #[test]
    fn stac_version_guard_recurses_members() {
        let fc = json!({
            "type": "FeatureCollection",
            "features": [
                {"type": "Feature", "stac_version": "1.0.0"},
                {"type": "Feature", "stac_version": "bogus version"},
            ]
        });
        assert!(guard_stac_version(&fc).is_err());
        let listing = json!({"collections": [{"type": "Collection", "stac_version": "9.9"}]});
        assert!(guard_stac_version(&listing).is_err());
    }

    #[test]
    fn stac_version_guard_ignores_non_stac_objects() {
        // `stac_version` on objects the validator never dispatches on
        // (no `type` member) is not our business.
        let doc = json!({"properties": {"stac_version": "not a version"}});
        assert!(guard_stac_version(&doc).is_ok());
        // Non-string stac_version values fail inside the validator, not here.
        let doc = json!({"type": "Feature", "stac_version": 42});
        assert!(guard_stac_version(&doc).is_ok());
    }
}
