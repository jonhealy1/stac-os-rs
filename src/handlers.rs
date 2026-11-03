// src/handlers.rs
use crate::dto::CreateOrLinkPayload;
use crate::links::LinkEngine;
use crate::store::{
    search_offset, NodeKind, Store, CATALOGS_INDEX, COLLECTIONS_INDEX, DEFAULT_SEARCH_LIMIT,
    ITEMS_INDEX, MAX_SEARCH_LIMIT, ROOT_CATALOG_ID,
};
use axum::{
    extract::{Path, Query, RawQuery, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::json;
use stac::{Bbox, Catalog, Collection, Item, Link};
use stac_api::{GetSearch, ItemCollection, Search};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

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
}

impl AppState {
    /// Either collections-search flag on — gates `bbox_shape` injection
    /// at write time (upstream does the same check).
    pub fn collections_search_enabled(&self) -> bool {
        self.enable_collections_search || self.enable_collections_search_route
    }
}

/// Default page size for catalog/collection/children list endpoints.
const DEFAULT_LIST_LIMIT: u64 = 10;

/// STAC requires `application/geo+json` on Item and ItemCollection payloads.
fn geo_json<T: serde::Serialize>(body: T) -> impl IntoResponse {
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
fn bad_filter(e: String) -> ApiError {
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
fn listing_filter_clause(
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
    let cql2: stac_api::Filter = if filter_lang == Some("cql2-json") {
        stac_api::Filter::Cql2Json(json_map(raw, invalid)?)
    } else {
        match serde_json::from_str::<serde_json::Value>(raw) {
            Ok(v) if v.is_object() => stac_api::Filter::Cql2Json(v.as_object().cloned().unwrap()),
            Ok(_) => return Err(invalid("expected a CQL2 expression with an operator")),
            Err(_) => stac_api::Filter::Cql2Text(crate::store::rewrite_between(raw)),
        }
    };
    crate::store::cql2_clause_with(&cql2, spec)
        .map(Some)
        .map_err(bad_filter)
}

fn json_map(
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
fn list_window(
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

/// Strip `EXCLUDED_FROM_ITEMS` paths from an item — applied before the
/// user's `fields` filter so server-side privacy always wins.
fn strip_excluded_item_fields(
    state: &AppState,
    obj: &mut serde_json::Map<String, serde_json::Value>,
) {
    if !state.excluded_item_fields.is_empty() {
        crate::store::filter_fields(obj, &[], &state.excluded_item_fields);
    }
}

/// `next` link for a paginated list response.
fn list_next_link(
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
fn extra_params(pairs: &[(&str, &str)]) -> String {
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
fn filter_link_extra(filter: Option<&str>, filter_lang: Option<&str>) -> Option<String> {
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
fn pagination_extra(raw: Option<&str>) -> Option<String> {
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
const INFERRED_LINK_RELS: &[&str] = &["self", "item", "parent", "collection", "root", "items"];

/// Rels the API owns on reads (upstream `GENERATED_COLLECTION_RELS`):
/// a stored link with one of these rels is a copy of a generated link
/// and is dropped when the read generated that rel itself.
const GENERATED_RESOURCE_RELS: &[&str] = &[
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
fn resolve_href(href: &str, base_url: &str) -> String {
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
fn filter_stored_links(links: &mut Vec<Link>, base_url: &str) {
    filter_stored_links_rels(links, INFERRED_LINK_RELS, base_url);
}

/// Catalog writes additionally strip `child`/`children` (upstream's
/// `create_catalog`/`update_catalog` dynamic-link filter).
const INFERRED_CATALOG_LINK_RELS: &[&str] = &[
    "self",
    "item",
    "parent",
    "collection",
    "root",
    "items",
    "child",
    "children",
];

fn filter_stored_catalog_links(links: &mut Vec<Link>, base_url: &str) {
    filter_stored_links_rels(links, INFERRED_CATALOG_LINK_RELS, base_url);
}

fn filter_stored_links_rels(links: &mut Vec<Link>, rels: &[&str], base_url: &str) {
    links.retain(|l| !rels.contains(&l.rel.as_str()));
    for l in links.iter_mut() {
        l.href = resolve_href(&l.href, base_url);
    }
}

/// Stored user links for a read: inferred rels removed, hrefs resolved.
fn stored_user_links(doc: &serde_json::Value, base_url: &str) -> Vec<serde_json::Value> {
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
fn merge_doc_links(doc: &mut serde_json::Value, generated: Vec<Link>, base_url: &str) {
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

fn parse_kind(raw: Option<&str>) -> Option<NodeKind> {
    match raw {
        Some("Catalog") => Some(NodeKind::Catalog),
        Some("Collection") => Some(NodeKind::Collection),
        _ => None,
    }
}

/// 404 unless `catalog_id` is the implicit root or a catalog document —
/// a bare hierarchy node (e.g. a collection id) is not a catalog.
async fn require_catalog(state: &AppState, catalog_id: &str) -> Result<(), ApiError> {
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
async fn require_scoped_collection(
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

fn link_json(href: String, rel: &str) -> Link {
    let mut link = Link::new(href, rel);
    link.r#type = Some("application/json".to_string());
    link
}

/// Dynamic links for an Item: `self`/`parent`/`collection`/`root`.
/// `self` and `parent` are scoped paths when a catalog scope is given,
/// canonical `/collections` paths otherwise.
fn item_links(
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
async fn catalog_links(
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
async fn child_links(state: &AppState, catalog_id: &str) -> Result<Vec<Link>, ApiError> {
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

fn collection_links(
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
fn global_collection_links(state: &AppState, collection_id: &str, parents: &[String]) -> Vec<Link> {
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
    // `related` (catalog) + `duplicate` (scoped URI) links instead.
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
    if !state.hide_alternate_parents {
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
fn is_collection_doc(doc: &serde_json::Value) -> bool {
    doc["type"].as_str() == Some("Collection")
}

async fn global_collection_doc(
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
fn catalog_hierarchy_links(state: &AppState, parents: &[String]) -> Vec<Link> {
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
async fn catalog_write_doc(
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
fn item_key(item_id: &str, collection_id: &str) -> String {
    format!("{item_id}|{collection_id}")
}

/// Derived search fields stored on a collection doc: `bbox_shape`, a
/// GeoJSON polygon of the first spatial extent bbox (upstream stores
/// the same derived field; it is stripped back out on reads).
fn collection_search_fields(doc: &mut serde_json::Map<String, serde_json::Value>) {
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
fn strip_collection_internals(doc: &mut serde_json::Value) {
    if let Some(obj) = doc.as_object_mut() {
        obj.remove("bbox_shape");
    }
}

/// Collection doc as stored — serialized plus derived search fields
/// when collections-search is enabled (upstream skips `bbox_shape`
/// otherwise).
fn stored_collection_doc(state: &AppState, collection: &Collection) -> serde_json::Value {
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
fn etag_of(state: &AppState, index: &str, v: &crate::store::DocVersion) -> String {
    format!(
        "\"{}-{}-{}\"",
        v.primary_term,
        v.seq_no,
        state.store.index_hash(index)
    )
}

/// The version portion of an ETag — drop a trailing `-<coding>` suffix.
fn etag_version_part(tag: &str) -> &str {
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
fn if_match_holds(header_value: &str, current_etag: &str) -> bool {
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
fn require_if_match(headers: &axum::http::HeaderMap, current_etag: &str) -> Result<(), ApiError> {
    match headers.get(header::IF_MATCH) {
        Some(v) if !if_match_holds(v.to_str().unwrap_or_default(), current_etag) => {
            Err(ApiError::PreconditionFailed)
        }
        None if std::env::var("REQUIRE_IF_MATCH").is_ok_and(|v| v == "true") => {
            Err(ApiError::PreconditionRequired)
        }
        _ => Ok(()),
    }
}

/// Attach `etag` to a status+body response.
fn etag_response(status: StatusCode, etag: &str, body: impl IntoResponse) -> impl IntoResponse {
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

fn item_doc(state: &AppState, item: &Item, scoped_catalog_id: Option<&str>) -> serde_json::Value {
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
async fn collection_doc(
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

// --- Discovery Handlers ---

fn conformance_classes(state: &AppState) -> Vec<&'static str> {
    let mut conforms_to = vec![
        "https://api.stacspec.org/v1.0.0/core",
        "https://api.stacspec.org/v1.0.0/browseable",
        "https://api.stacspec.org/v1.0.0/ogcapi-features",
        "https://api.stacspec.org/v1.1.0/ogcapi-features#sort",
        "http://www.opengis.net/spec/ogcapi-features-1/1.0/conf/core",
        "http://www.opengis.net/spec/ogcapi-features-1/1.0/conf/geojson",
        "http://www.opengis.net/spec/ogcapi-features-5/1.0/conf/sortables",
        "https://api.stacspec.org/v1.0.0/item-search",
        "https://api.stacspec.org/v1.0.0/item-search#fields",
        "https://api.stacspec.org/v1.0.0/ogcapi-features#fields",
        "https://api.stacspec.org/v1.0.0/item-search#query",
        "https://api.stacspec.org/v1.0.0/ogcapi-features#query",
        "https://api.stacspec.org/v1.0.0-rc.2/item-search#filter",
        "http://www.opengis.net/spec/ogcapi-features-3/1.0/conf/filter",
        "http://www.opengis.net/spec/ogcapi-features-3/1.0/conf/features-filter",
        "http://www.opengis.net/spec/ogcapi-features-3/1.0/conf/queryables",
        "http://www.opengis.net/spec/cql2/1.0/conf/cql2-json",
        "http://www.opengis.net/spec/cql2/1.0/conf/cql2-text",
        "http://www.opengis.net/spec/cql2/1.0/conf/basic-cql2",
        "http://www.opengis.net/spec/cql2/1.0/conf/advanced-comparison-operators",
        "http://www.opengis.net/spec/cql2/1.0/conf/basic-spatial-functions",
        "https://api.stacspec.org/v1.1.0/item-search#sort",
        "https://api.stacspec.org/v1.1.0/item-search#sortables",
        "https://api.stacspec.org/v1.1.0/collection-search#sortables",
        "https://api.stacspec.org/v1.0.0/multi-tenant-catalogs",
        "https://api.stacspec.org/v1.0.0/multi-tenant-catalogs/search",
        "https://api.stacspec.org/v1.0.0/children",
        "https://api.stacspec.org/v1.0.0/children#type-filter",
    ];
    if state.enable_transactions {
        conforms_to.push("https://api.stacspec.org/v1.0.0/multi-tenant-catalogs/transaction");
        conforms_to.push("https://api.stacspec.org/v1.0.0/ogcapi-features/extensions/transaction");
    }
    conforms_to
}

/// GET /conformance — OGC API conformance classes for the whole service.
#[utoipa::path(
    get,
    path = "/conformance",
    tag = "metadata",
    summary = "Conformance classes",
    responses(
        (status = 200, description = "Successful response"),
        (status = 400, description = "Invalid request"),
        (status = 404, description = "Not found")
    )
)]
pub async fn conformance(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(json!({ "conformsTo": conformance_classes(&state) }))
}

/// Core queryables present on every item — upstream DEFAULT_QUERYABLES
/// verbatim, including its "Creation Timestamp" on `updated`.
fn default_queryables() -> serde_json::Map<String, serde_json::Value> {
    serde_json::Map::from_iter([
        ("id", json!({"description": "ID", "$ref": "https://schemas.stacspec.org/v1.0.0/item-spec/json-schema/item.json#/definitions/core/allOf/2/properties/id"})),
        ("collection", json!({"description": "Collection", "$ref": "https://schemas.stacspec.org/v1.0.0/item-spec/json-schema/item.json#/definitions/core/allOf/2/then/properties/collection"})),
        ("geometry", json!({"description": "Geometry", "$ref": "https://schemas.stacspec.org/v1.0.0/item-spec/json-schema/item.json#/definitions/core/allOf/1/oneOf/0/properties/geometry"})),
        ("datetime", json!({"description": "Acquisition Timestamp", "$ref": "https://schemas.stacspec.org/v1.0.0/item-spec/json-schema/datetime.json#/properties/datetime"})),
        ("created", json!({"description": "Creation Timestamp", "$ref": "https://schemas.stacspec.org/v1.0.0/item-spec/json-schema/datetime.json#/properties/created"})),
        ("updated", json!({"description": "Creation Timestamp", "$ref": "https://schemas.stacspec.org/v1.0.0/item-spec/json-schema/datetime.json#/properties/updated"})),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect::<serde_json::Map<String, serde_json::Value>>())
}

/// Upstream ALL_QUERYABLES — DEFAULT plus OPTIONAL (enum-marked fields
/// populated per-collection at read time).
fn all_queryables() -> serde_json::Map<String, serde_json::Value> {
    let mut q = default_queryables();
    q.insert(
        "platform".to_string(),
        json!({"$enum": true, "description": "Satellite platform identifier"}),
    );
    q
}

/// JSON Schema document served by the queryables endpoints.
fn queryables_schema(
    state: &AppState,
    id: String,
    title: &str,
    dynamic: serde_json::Map<String, serde_json::Value>,
) -> serde_json::Value {
    let mut properties = default_queryables();
    // Discovered mapping fields fill in around the core set; core $refs win.
    for (name, schema) in dynamic {
        properties.entry(name).or_insert(schema);
    }
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": format!("{}{}", state.base_url, id),
        "type": "object",
        "title": title,
        "description": "Queryable names for the stac-os-rs Item Search filter.",
        "properties": properties,
    })
}

fn json_schema(body: serde_json::Value) -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "application/schema+json")],
        Json(body),
    )
}

/// GET /queryables — upstream's default is the static DEFAULT_QUERYABLES
/// set (the union view is an env-flagged opt-in we don't implement).
#[utoipa::path(
    get,
    path = "/queryables",
    tag = "schemas",
    summary = "Item queryables schema",
    responses(
        (status = 200, description = "Successful response"),
        (status = 400, description = "Invalid request"),
        (status = 404, description = "Not found")
    )
)]
pub async fn get_queryables(
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, ApiError> {
    Ok(json_schema(queryables_schema(
        &state,
        "/queryables".to_string(),
        "Queryables for stac-os-rs",
        default_queryables(),
    )))
}

/// GET /collections/{collection_id}/queryables — upstream merge: the
/// ALL_QUERYABLES base plus every indexed field of the items index under
/// its un-prefixed name, with `$enum` fields populated by a per-
/// collection terms aggregation.
#[utoipa::path(
    get,
    path = "/collections/{collection_id}/queryables",
    tag = "schemas",
    summary = "Collection queryables schema",
    params(("collection_id" = String, Path, description = "Collection id")),
    responses(
        (status = 200, description = "Successful response"),
        (status = 400, description = "Invalid request"),
        (status = 404, description = "Not found")
    )
)]
pub async fn get_collection_queryables(
    Path(collection_id): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, ApiError> {
    if state
        .store
        .get_document(COLLECTIONS_INDEX, &collection_id)
        .await?
        .is_none()
    {
        return Err(ApiError::NotFound(collection_id));
    }
    let mapping = index_mappings(&state, ITEMS_INDEX).await?;
    let all = all_queryables();
    let mut properties = all.clone();
    let mut enum_fields: Vec<(String, String)> = Vec::new();

    // Upstream collection_queryables walk: full document path keys,
    // exposed un-prefixed (eo:cloud_cover, not properties.eo:cloud_cover).
    let mut stack: Vec<(String, serde_json::Value)> = mapping
        .get("properties")
        .and_then(serde_json::Value::as_object)
        .map(|o| o.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default();
    let is_excluded = |fqn: &str| {
        state
            .excluded_queryables
            .iter()
            .any(|p| fqn == p || fqn.starts_with(&format!("{p}.")))
    };
    while let Some((fqn, def)) = stack.pop() {
        if is_excluded(&fqn) {
            continue;
        }
        if let Some(children) = def.get("properties").and_then(serde_json::Value::as_object) {
            stack.extend(
                children
                    .iter()
                    .filter(|(_, v)| {
                        v.is_object()
                            && v.get("enabled").and_then(serde_json::Value::as_bool) != Some(false)
                    })
                    .map(|(k, v)| (format!("{fqn}.{k}"), v.clone())),
            );
        }
        let Some(ty) = def.get("type").and_then(serde_json::Value::as_str) else {
            continue;
        };
        if def.get("enabled").and_then(serde_json::Value::as_bool) == Some(false) {
            continue;
        }
        let name = fqn
            .strip_prefix("properties.")
            .or_else(|| fqn.strip_prefix("assets."))
            .unwrap_or(&fqn)
            .to_string();
        let mut result = all.get(&name).cloned().unwrap_or_else(|| json!({}));
        let obj = result.as_object_mut().unwrap();
        obj.entry("title".to_string())
            .or_insert_with(|| json!(crate::store::py_title(&name.replace('_', " "))));
        let json_type = match ty {
            "boolean" => "boolean",
            "constant_keyword" | "date" | "date_nanos" | "ip" | "keyword" | "match_only_text"
            | "text" | "version" | "wildcard" => "string",
            "integer" => "integer",
            "byte" | "double" | "float" | "half_float" | "long" | "scaled_float" | "short"
            | "token_count" | "unsigned_long" => "number",
            "geo_point" | "geo_shape" => "object",
            "nested" => "array",
            other => other,
        };
        obj.entry("type".to_string())
            .or_insert_with(|| json!(json_type));
        if matches!(ty, "date" | "date_nanos") {
            obj.entry("format".to_string())
                .or_insert_with(|| json!("date-time"));
        }
        if obj.remove("$enum") == Some(json!(true)) {
            enum_fields.push((fqn.clone(), ty.to_string()));
        }
        properties.insert(name, result);
    }

    if !enum_fields.is_empty() {
        let values = state
            .store
            .unique_values(&collection_id, &enum_fields)
            .await?;
        for (fqn, _) in &enum_fields {
            let name = fqn
                .strip_prefix("properties.")
                .or_else(|| fqn.strip_prefix("assets."))
                .unwrap_or(fqn);
            if let Some(vals) = values.get(fqn) {
                properties[name]["enum"] = vals.clone();
            }
        }
    }

    let mut schema = queryables_schema(
        &state,
        format!("/collections/{collection_id}/queryables"),
        "Queryables for stac-os-rs",
        properties,
    );
    schema["additionalProperties"] = json!(false);
    Ok(json_schema(schema))
}

/// GET /api — OpenAPI service description (hand-maintained).
#[utoipa::path(
    get,
    path = "/api",
    tag = "metadata",
    summary = "OpenAPI service description",
    responses(
        (status = 200, description = "Successful response"),
        (status = 400, description = "Invalid request"),
        (status = 404, description = "Not found")
    )
)]
pub async fn api() -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            "application/vnd.oai.openapi+json;version=3.0",
        )],
        crate::docs::openapi().to_json().unwrap_or_default(),
    )
}

/// `GET /api.html` — human-readable service docs (Swagger UI pointed at
/// `/api`), upstream's `service-doc` link target.
pub async fn api_docs_html() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/html")],
        "<!DOCTYPE html><html><head><title>STAC API Docs</title>\
         <link rel=\"stylesheet\" href=\"https://unpkg.com/swagger-ui-dist/swagger-ui.css\"></head>\
         <body><div id=\"swagger-ui\"></div>\
         <script src=\"https://unpkg.com/swagger-ui-dist/swagger-ui-bundle.js\"></script>\
         <script>window.onload=()=>{SwaggerUIBundle({url:'/api',dom_id:'#swagger-ui'})}</script>\
         </body></html>",
    )
}

/// `GET /_mgmt/ping` — upstream health-check endpoint.
#[utoipa::path(
    get,
    path = "/_mgmt/ping",
    tag = "metadata",
    summary = "Health check",
    responses(
        (status = 200, description = "Successful response"),
        (status = 400, description = "Invalid request"),
        (status = 404, description = "Not found")
    )
)]
pub async fn mgmt_ping() -> Json<serde_json::Value> {
    Json(json!({"message": "PONG"}))
}

#[utoipa::path(
    get,
    path = "/",
    tag = "metadata",
    summary = "Landing page",
    responses(
        (status = 200, description = "Successful response"),
        (status = 400, description = "Invalid request"),
        (status = 404, description = "Not found")
    )
)]
pub async fn root_landing_page(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let base = &state.base_url;
    let mut links = json!([
        { "rel": "self", "type": "application/json", "href": format!("{base}/") },
        { "rel": "root", "type": "application/json", "href": format!("{base}/") },
        { "rel": "conformance", "type": "application/json", "href": format!("{base}/conformance") },
        { "rel": "service-desc", "type": "application/vnd.oai.openapi+json;version=3.0", "href": format!("{base}/api") },
        { "rel": "service-doc", "type": "text/html", "href": format!("{base}/api.html") },
        { "rel": "data", "type": "application/json", "href": format!("{base}/collections") },
        { "rel": "search", "type": "application/geo+json", "href": format!("{base}/search") },
        { "rel": "http://www.opengis.net/def/rel/ogc/1.0/sortables", "type": "application/schema+json", "href": format!("{base}/sortables"), "title": "Sortables" },
        { "rel": "queryables", "type": "application/schema+json", "href": format!("{base}/queryables"), "title": "Queryables" },
        { "rel": "catalogs", "type": "application/json", "href": format!("{base}/catalogs"), "title": "Multi-Tenant Catalogs Registry" }
    ]);
    // Browseable: a child link per root-level node (collections resolve
    // on the canonical /collections surface). Nodes without a backing
    // document are skipped rather than emitting 404 links.
    let children = state.store.get_child_nodes(ROOT_CATALOG_ID, None).await?;
    let cat_ids: Vec<String> = children
        .iter()
        .filter(|n| n.kind == NodeKind::Catalog)
        .map(|n| n.id.clone())
        .collect();
    let col_ids: Vec<String> = children
        .iter()
        .filter(|n| n.kind == NodeKind::Collection)
        .map(|n| n.id.clone())
        .collect();
    let mut live: HashMap<String, Option<String>> = state
        .store
        .get_documents(CATALOGS_INDEX, &cat_ids)
        .await?
        .into_iter()
        .chain(
            state
                .store
                .get_documents(COLLECTIONS_INDEX, &col_ids)
                .await?,
        )
        .filter_map(|d| {
            d["id"]
                .as_str()
                .map(|id| (id.to_string(), d["title"].as_str().map(str::to_owned)))
        })
        .collect();
    for child in children {
        let Some(title) = live.remove(&child.id) else {
            continue;
        };
        let href = match child.kind {
            NodeKind::Collection => format!("{base}/collections/{}", child.id),
            _ => format!("{base}/catalogs/{}", child.id),
        };
        links.as_array_mut().unwrap().push(json!({
            "rel": "child",
            "type": "application/json",
            "href": href,
            "title": title.unwrap_or_else(|| child.id.clone())
        }));
    }
    Ok(Json(json!({
        "stac_version": "1.0.0",
        "type": "Catalog",
        "id": "stac-multi-tenant-root",
        "title": "STAC API with Multi-Tenant Catalogs",
        "description": "Multi-tenant STAC API backed by OpenSearch. Root catalog of the registry.",
        "conformsTo": conformance_classes(&state),
        "links": links
    })))
}

#[utoipa::path(
    get,
    path = "/catalogs",
    tag = "catalogs",
    summary = "List all catalogs (flat registry listing)",
    params(CatalogsQuery),
    responses(
        (status = 200, description = "Successful response"),
        (status = 400, description = "Invalid request"),
        (status = 404, description = "Not found")
    )
)]
pub async fn list_catalogs(
    State(state): State<Arc<AppState>>,
    RawQuery(raw): RawQuery,
    Query(query): Query<CatalogsQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let clause = listing_filter_clause(
        query.filter.as_deref(),
        query.filter_lang.as_deref(),
        crate::store::COLLECTION_FIELDS,
    )?;
    let (limit, offset) = list_window(
        query.limit,
        query.token.as_deref(),
        DEFAULT_LIST_LIMIT,
        None,
    )?;
    // Upstream `/catalogs` is a flat list of every catalog in the
    // registry — nested catalogs carry their parent links.
    let (mut catalogs, matched) = state
        .store
        .search_docs_filtered(&[CATALOGS_INDEX], clause, offset, limit)
        .await?;
    let matched = matched as usize;
    let page: Vec<String> = catalogs
        .iter()
        .filter_map(|d| d["id"].as_str().map(str::to_owned))
        .collect();
    let parents_map = state.store.get_parents_many(&page).await?;
    for doc in catalogs.iter_mut() {
        if let Some(id) = doc["id"].as_str().map(str::to_owned) {
            let parents = parents_map.get(&id).cloned().unwrap_or_default();
            merge_doc_links(
                doc,
                catalog_links(&state, &id, &parents).await?,
                &state.base_url,
            );
        }
    }
    let mut links = json!([
        { "rel": "self", "href": format!("{}/catalogs", state.base_url) },
        { "rel": "root", "href": state.base_url },
        { "rel": "parent", "href": state.base_url }
    ]);
    let returned = catalogs.len();
    let extra = pagination_extra(raw.as_deref());
    if let Some(next) = list_next_link(
        &state,
        "/catalogs",
        limit,
        offset,
        returned,
        matched,
        extra.as_deref(),
    ) {
        links.as_array_mut().unwrap().push(next);
    }
    Ok(Json(json!({
        "catalogs": catalogs,
        "numberReturned": returned,
        "numberMatched": matched,
        "links": links
    })))
}

#[utoipa::path(
    post,
    path = "/catalogs",
    tag = "catalogs",
    summary = "Create a root catalog",
    request_body = serde_json::Value,
    responses(
        (status = 200, description = "Successful response"),
        (status = 400, description = "Invalid request"),
        (status = 404, description = "Not found")
    )
)]
pub async fn create_root_catalog(
    State(state): State<Arc<AppState>>,
    StacJson(mut catalog): StacJson<Catalog>,
) -> Result<impl IntoResponse, ApiError> {
    filter_stored_catalog_links(&mut catalog.links, &state.base_url);
    // Top-level creates are always root-level — a client-supplied
    // `parent_ids` is ignored (the hierarchy index is authoritative).
    catalog.additional_fields.remove("parent_ids");
    // 409 on collision with an existing catalog OR collection id
    for index in [CATALOGS_INDEX, COLLECTIONS_INDEX] {
        if state
            .store
            .get_document(index, &catalog.id)
            .await?
            .is_some()
        {
            return Err(ApiError::Conflict(catalog.id.clone()));
        }
    }
    if !state
        .store
        .set_parents(
            &catalog.id,
            vec![ROOT_CATALOG_ID.to_string()],
            NodeKind::Catalog,
        )
        .await?
    {
        return Err(ApiError::WriteConflict(catalog.id));
    }
    if !state
        .store
        .create_document(CATALOGS_INDEX, &catalog.id, &catalog)
        .await?
    {
        return Err(ApiError::Conflict(catalog.id));
    }
    let (_, version) = state
        .store
        .get_document_versioned(CATALOGS_INDEX, &catalog.id)
        .await?
        .ok_or_else(|| ApiError::Internal("catalog vanished after create".into()))?;
    Ok(etag_response(
        StatusCode::CREATED,
        &etag_of(&state, CATALOGS_INDEX, &version),
        Json(catalog_write_doc(&state, &catalog).await?),
    ))
}

#[utoipa::path(
    get,
    path = "/catalogs/{catalog_id}",
    tag = "catalogs",
    summary = "Get a catalog",
    params(("catalog_id" = String, Path, description = "Catalog id")),
    responses(
        (status = 200, description = "Successful response"),
        (status = 400, description = "Invalid request"),
        (status = 404, description = "Not found")
    )
)]
pub async fn get_catalog(
    Path(catalog_id): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, ApiError> {
    match state
        .store
        .get_document_versioned(CATALOGS_INDEX, &catalog_id)
        .await?
    {
        Some((mut doc, version)) => {
            let parents = state.store.get_parents(&catalog_id).await?;
            merge_doc_links(
                &mut doc,
                catalog_links(&state, &catalog_id, &parents).await?,
                &state.base_url,
            );
            Ok(etag_response(
                StatusCode::OK,
                &etag_of(&state, CATALOGS_INDEX, &version),
                Json(doc),
            ))
        }
        None => Err(ApiError::NotFound(catalog_id)),
    }
}

#[utoipa::path(
    put,
    path = "/catalogs/{catalog_id}",
    tag = "catalogs",
    summary = "Replace a catalog",
    params(("catalog_id" = String, Path, description = "Catalog id"), ("If-Match" = Option<String>, Header, description = "Conditional write: apply only if the resource ETag matches")),
    request_body = serde_json::Value,
    responses(
        (status = 200, description = "Successful response"),
        (status = 400, description = "Invalid request"),
        (status = 404, description = "Not found"),
        (status = 412, description = "If-Match precondition failed"),
        (status = 428, description = "If-Match header required")
    )
)]
pub async fn update_catalog(
    Path(catalog_id): Path<String>,
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    StacJson(mut catalog): StacJson<Catalog>,
) -> Result<impl IntoResponse, ApiError> {
    require_catalog(&state, &catalog_id).await?;
    if catalog.id != catalog_id {
        return Err(ApiError::BadRequest(format!(
            "body id '{}' does not match path id '{catalog_id}'",
            catalog.id
        )));
    }
    let (_, version) = state
        .store
        .get_document_versioned(CATALOGS_INDEX, &catalog_id)
        .await?
        .ok_or_else(|| ApiError::NotFound(catalog_id.clone()))?;
    require_if_match(&headers, &etag_of(&state, CATALOGS_INDEX, &version))?;
    filter_stored_catalog_links(&mut catalog.links, &state.base_url);
    // Update keeps the stored edges — `parent_ids` in the body is ignored.
    catalog.additional_fields.remove("parent_ids");
    let applied = if headers.contains_key(header::IF_MATCH) {
        state
            .store
            .index_document_versioned(CATALOGS_INDEX, &catalog_id, &catalog, version)
            .await?
    } else {
        state
            .store
            .replace_document(CATALOGS_INDEX, &catalog_id, &catalog)
            .await?
    };
    if !applied {
        return Err(if headers.contains_key(header::IF_MATCH) {
            ApiError::PreconditionFailed
        } else {
            ApiError::WriteConflict(catalog_id)
        });
    }
    let (_, version) = state
        .store
        .get_document_versioned(CATALOGS_INDEX, &catalog_id)
        .await?
        .ok_or_else(|| ApiError::Internal("catalog vanished after write".into()))?;
    Ok(etag_response(
        StatusCode::OK,
        &etag_of(&state, CATALOGS_INDEX, &version),
        Json(catalog_write_doc(&state, &catalog).await?),
    ))
}

#[utoipa::path(
    get,
    path = "/catalogs/{catalog_id}/children",
    tag = "catalogs",
    summary = "List catalog children (catalogs + collections)",
    params(("catalog_id" = String, Path, description = "Catalog id"), ChildrenQuery),
    responses(
        (status = 200, description = "Successful response"),
        (status = 400, description = "Invalid request"),
        (status = 404, description = "Not found")
    )
)]
pub async fn get_catalog_children(
    Path(catalog_id): Path<String>,
    RawQuery(raw): RawQuery,
    Query(query): Query<ChildrenQuery>,
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, ApiError> {
    let clause = listing_filter_clause(
        query.filter.as_deref(),
        query.filter_lang.as_deref(),
        crate::store::COLLECTION_FIELDS,
    )?;
    require_catalog(&state, &catalog_id).await?;
    let (limit, offset) = list_window(
        query.limit,
        query.token.as_deref(),
        DEFAULT_LIST_LIMIT,
        None,
    )?;
    let kind = parse_kind(query.r#type.as_deref());
    let nodes = state.store.get_child_nodes(&catalog_id, kind).await?;

    let (children, matched): (Vec<serde_json::Value>, usize) = if clause.is_some() {
        // Filtered listing — query the docs directly, scoped to child ids.
        let ids: Vec<String> = nodes.iter().map(|n| n.id.clone()).collect();
        let kind_map: HashMap<String, NodeKind> =
            nodes.into_iter().map(|n| (n.id.clone(), n.kind)).collect();
        let indexes: &[&str] = match kind {
            Some(NodeKind::Catalog) => &[CATALOGS_INDEX],
            Some(NodeKind::Collection) => &[COLLECTIONS_INDEX],
            None => &[CATALOGS_INDEX, COLLECTIONS_INDEX],
        };
        let (docs, m) = state
            .store
            .search_docs_in_scope(indexes, &ids, clause, offset, limit)
            .await?;
        let doc_ids: Vec<String> = docs
            .iter()
            .filter_map(|d| d["id"].as_str().map(str::to_owned))
            .collect();
        let parents_map = state.store.get_parents_many(&doc_ids).await?;
        let mut out = Vec::with_capacity(docs.len());
        for mut doc in docs {
            let Some(id) = doc["id"].as_str().map(str::to_owned) else {
                continue;
            };
            let parents = parents_map.get(&id).cloned().unwrap_or_default();
            let generated = match kind_map.get(&id) {
                Some(NodeKind::Collection) => collection_links(&state, &id, &catalog_id, &parents),
                _ => catalog_links(&state, &id, &parents).await?,
            };
            merge_doc_links(&mut doc, generated, &state.base_url);
            out.push(doc);
        }
        (out, m as usize)
    } else {
        let matched = nodes.len();
        let nodes: Vec<_> = nodes
            .into_iter()
            .skip(offset as usize)
            .take(limit as usize)
            .collect();

        // Fetch the child documents (split by kind — catalogs and collections
        // live in different indices) then re-assemble in node order.
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
        let mut docs = state.store.get_documents(CATALOGS_INDEX, &cat_ids).await?;
        docs.extend(
            state
                .store
                .get_documents(COLLECTIONS_INDEX, &col_ids)
                .await?,
        );
        let mut docs_by_id: HashMap<String, serde_json::Value> = docs
            .into_iter()
            .filter_map(|d| {
                let id = d["id"].as_str()?.to_string();
                Some((id, d))
            })
            .collect();

        let mut children = Vec::with_capacity(nodes.len());
        for node in &nodes {
            let Some(mut doc) = docs_by_id.remove(&node.id) else {
                continue; // hierarchy node exists but doc is missing — skip
            };
            let generated = match node.kind {
                // parent link locked to this listing's catalog context
                NodeKind::Catalog => catalog_links(&state, &node.id, &node.parents).await?,
                NodeKind::Collection => {
                    collection_links(&state, &node.id, &catalog_id, &node.parents)
                }
            };
            merge_doc_links(&mut doc, generated, &state.base_url);
            children.push(doc);
        }
        (children, matched)
    };

    let mut links = json!([
        { "rel": "self", "href": format!("{}/catalogs/{catalog_id}/children", state.base_url) },
        { "rel": "root", "href": state.base_url },
        { "rel": "parent", "href": format!("{}/catalogs/{catalog_id}", state.base_url) }
    ]);
    let extra = pagination_extra(raw.as_deref());
    if let Some(next) = list_next_link(
        &state,
        &format!("/catalogs/{catalog_id}/children"),
        limit,
        offset,
        children.len(),
        matched,
        extra.as_deref(),
    ) {
        links.as_array_mut().unwrap().push(next);
    }

    Ok(Json(json!({
        "children": children,
        "numberReturned": children.len(),
        "numberMatched": matched,
        "links": links
    })))
}

/// GET /catalogs/{id}/catalogs — the catalog-kind children of a catalog.
#[utoipa::path(
    get,
    path = "/catalogs/{catalog_id}/catalogs",
    tag = "catalogs",
    summary = "List sub-catalogs",
    params(("catalog_id" = String, Path, description = "Catalog id"), CatalogsQuery),
    responses(
        (status = 200, description = "Successful response"),
        (status = 400, description = "Invalid request"),
        (status = 404, description = "Not found")
    )
)]
pub async fn list_sub_catalogs(
    Path(catalog_id): Path<String>,
    RawQuery(raw): RawQuery,
    Query(query): Query<CatalogsQuery>,
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, ApiError> {
    let clause = listing_filter_clause(
        query.filter.as_deref(),
        query.filter_lang.as_deref(),
        crate::store::COLLECTION_FIELDS,
    )?;
    require_catalog(&state, &catalog_id).await?;
    let (limit, offset) = list_window(
        query.limit,
        query.token.as_deref(),
        DEFAULT_LIST_LIMIT,
        None,
    )?;
    let ids = state
        .store
        .get_children_by_kind(&catalog_id, NodeKind::Catalog)
        .await?;
    let (mut catalogs, matched): (Vec<serde_json::Value>, usize) = if clause.is_some() {
        let (docs, m) = state
            .store
            .search_docs_in_scope(&[CATALOGS_INDEX], &ids, clause, offset, limit)
            .await?;
        (docs, m as usize)
    } else {
        let page: Vec<String> = ids
            .iter()
            .skip(offset as usize)
            .take(limit as usize)
            .cloned()
            .collect();
        (
            state.store.get_documents(CATALOGS_INDEX, &page).await?,
            ids.len(),
        )
    };
    let page: Vec<String> = catalogs
        .iter()
        .filter_map(|d| d["id"].as_str().map(str::to_owned))
        .collect();
    let parents_map = state.store.get_parents_many(&page).await?;
    for doc in catalogs.iter_mut() {
        if let Some(id) = doc["id"].as_str().map(str::to_owned) {
            let parents = parents_map.get(&id).cloned().unwrap_or_default();
            merge_doc_links(
                doc,
                catalog_links(&state, &id, &parents).await?,
                &state.base_url,
            );
        }
    }
    let mut links = json!([
        { "rel": "self", "href": format!("{}/catalogs/{catalog_id}/catalogs", state.base_url) },
        { "rel": "root", "href": state.base_url },
        { "rel": "parent", "href": format!("{}/catalogs/{catalog_id}", state.base_url) }
    ]);
    let returned = catalogs.len();
    let extra = pagination_extra(raw.as_deref());
    if let Some(next) = list_next_link(
        &state,
        &format!("/catalogs/{catalog_id}/catalogs"),
        limit,
        offset,
        returned,
        matched,
        extra.as_deref(),
    ) {
        links.as_array_mut().unwrap().push(next);
    }
    Ok(Json(json!({
        "catalogs": catalogs,
        "numberReturned": returned,
        "numberMatched": matched,
        "links": links
    })))
}

/// Upstream `DEFAULT_SORTABLES` — the core item-search sortables set.
fn default_sortables() -> serde_json::Value {
    json!({
        "id": {"type": "string", "title": "Id"},
        "collection": {"type": "string", "title": "Collection"},
        "properties.datetime": {"type": "string", "format": "date-time", "title": "Datetime"},
        "properties.created": {"type": "string", "format": "date-time", "title": "Created"},
        "properties.updated": {"type": "string", "format": "date-time", "title": "Updated"}
    })
}

fn sortables_schema(
    base: &str,
    id_path: &str,
    properties: serde_json::Value,
    additional: bool,
) -> serde_json::Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": format!("{base}{id_path}"),
        "type": "object",
        "title": "Sortables",
        "properties": properties,
        "additionalProperties": additional
    })
}

/// `GET /{index}/_mapping` -> the mappings body, for the sortables/
/// queryables schema builders.
async fn index_mappings(state: &AppState, logical: &str) -> Result<serde_json::Value, ApiError> {
    let mapping = state.store.get_index_mapping(logical).await?;
    Ok(mapping
        .as_object()
        .and_then(|o| o.values().next())
        .and_then(|v| v.get("mappings"))
        .cloned()
        .unwrap_or_else(|| json!({})))
}

/// Sortable fields of the items index — DEFAULT_SORTABLES plus every
/// sortable dynamic leaf (features binding is a closed schema).
async fn items_sortable_fields(
    state: &AppState,
) -> Result<serde_json::Map<String, serde_json::Value>, ApiError> {
    let mapping = index_mappings(state, ITEMS_INDEX).await?;
    let mut fields = crate::store::sortable_properties(&mapping, &state.excluded_queryables);
    for (k, v) in default_sortables().as_object().cloned().unwrap_or_default() {
        fields.entry(k).or_insert(v);
    }
    Ok(fields)
}

/// GET /sortables — Sortables schema for item search (item-search
/// binding advertises `additionalProperties: true`: unmapped fields
/// sort as null rather than erroring).
#[utoipa::path(
    get,
    path = "/sortables",
    tag = "schemas",
    summary = "Item sortables schema",
    responses(
        (status = 200, description = "Successful response"),
        (status = 400, description = "Invalid request"),
        (status = 404, description = "Not found")
    )
)]
pub async fn sortables(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    json_schema(sortables_schema(
        &state.base_url,
        "/sortables",
        default_sortables(),
        true,
    ))
}

/// GET /collections/{id}/sortables — features-binding sortables schema
/// derived from the items index mapping (`additionalProperties: false`).
#[utoipa::path(
    get,
    path = "/collections/{collection_id}/sortables",
    tag = "schemas",
    summary = "Collection sortables schema",
    params(("collection_id" = String, Path, description = "Collection id")),
    responses(
        (status = 200, description = "Successful response"),
        (status = 400, description = "Invalid request"),
        (status = 404, description = "Not found")
    )
)]
pub async fn collection_sortables(
    Path(collection_id): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, ApiError> {
    if state
        .store
        .get_document(COLLECTIONS_INDEX, &collection_id)
        .await?
        .is_none()
    {
        return Err(ApiError::NotFound(collection_id));
    }
    Ok(json_schema(sortables_schema(
        &state.base_url,
        &format!("/collections/{collection_id}/sortables"),
        json!(items_sortable_fields(&state).await?),
        false,
    )))
}

/// GET /collections-sortables — collection-search sortables from the
/// collections index mapping (`additionalProperties: false`).
#[utoipa::path(
    get,
    path = "/collections-sortables",
    tag = "schemas",
    summary = "Collections sortables schema",
    responses(
        (status = 200, description = "Successful response"),
        (status = 400, description = "Invalid request"),
        (status = 404, description = "Not found")
    )
)]
pub async fn collections_sortables(
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, ApiError> {
    let mapping = index_mappings(&state, COLLECTIONS_INDEX).await?;
    let fields = crate::store::sortable_properties(&mapping, &state.excluded_queryables);
    Ok(json_schema(sortables_schema(
        &state.base_url,
        "/collections-sortables",
        json!(fields),
        false,
    )))
}

/// GET /catalogs/{id}/conformance — catalog-scoped conformance classes.
#[utoipa::path(
    get,
    path = "/catalogs/{catalog_id}/conformance",
    tag = "catalogs",
    summary = "Scoped conformance classes",
    params(("catalog_id" = String, Path, description = "Catalog id")),
    responses(
        (status = 200, description = "Successful response"),
        (status = 400, description = "Invalid request"),
        (status = 404, description = "Not found")
    )
)]
pub async fn catalog_conformance(
    Path(catalog_id): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, ApiError> {
    require_catalog(&state, &catalog_id).await?;
    Ok(Json(json!({ "conformsTo": conformance_classes(&state) })))
}

// --- Scoped Item Search ---

/// Shared scoped-search pipeline, in upstream order: pagination token →
/// catalog-scope 403 → extension-param parsing → spec validation → query.
async fn run_scoped_search(
    state: &AppState,
    scope_id: &str,
    mut search: Search,
    req: Option<(axum::http::Uri, bool)>,
) -> Result<impl IntoResponse, ApiError> {
    let empty = || {
        let mut c =
            ItemCollection::new(Vec::new()).map_err(|e| ApiError::Internal(e.to_string()))?;
        c.number_matched = Some(0);
        c.number_returned = Some(0);
        Ok::<_, ApiError>(c)
    };

    // Request-level validation runs before the catalog 404 — pydantic
    // rejects malformed bodies upstream before scope resolution.
    // `validate_datetime`: an interval needs at least one real bound —
    // `""`, `/`, `../..` are rejected with its message.
    if let Some(dt) = &search.items.datetime {
        let (s, e) = dt.split_once('/').unwrap_or((dt.as_str(), dt.as_str()));
        let bound = |b: &str| !matches!(b, "" | ".." | "null");
        if !bound(s) && !bound(e) {
            return Err(ApiError::BadRequest(
                "Invalid parameters provided: At least one of the start or end dates must be provided"
                    .to_string(),
            ));
        }
    }

    // `validate_bbox`: every latitude in [-90, 90], longitude in
    // [-180, 180] — before antimeridian rewriting so `[100,0,0,105]`
    // is a 400, not a wrap.
    if let Some(bbox) = search.items.bbox {
        let in_range = match bbox {
            Bbox::TwoDimensional([xmin, ymin, xmax, ymax])
            | Bbox::ThreeDimensional([xmin, ymin, _, xmax, ymax, _]) => {
                [xmin, xmax].iter().all(|x| (-180.0..=180.0).contains(x))
                    && [ymin, ymax].iter().all(|y| (-90.0..=90.0).contains(y))
            }
        };
        if !in_range {
            return Err(ApiError::BadRequest(
                "Invalid parameters provided: bbox".to_string(),
            ));
        }
    }

    // A scoped search on a missing catalog is a 404, not an empty page
    // (upstream semantics). Root scope always exists.
    if scope_id != ROOT_CATALOG_ID
        && state
            .store
            .get_document(CATALOGS_INDEX, scope_id)
            .await?
            .is_none()
    {
        return Err(ApiError::NotFound(scope_id.to_string()));
    }

    // Pagination token → numeric offset (rejected before any lookup).
    if let Some(t) = search.items.additional_fields.get("token").cloned() {
        if !t.is_null() {
            let offset = match &t {
                serde_json::Value::String(s) => decode_item_token(s)?,
                serde_json::Value::Number(n) => n
                    .as_u64()
                    .ok_or_else(|| ApiError::BadRequest("Invalid pagination token.".to_string()))?,
                _ => {
                    return Err(ApiError::BadRequest(
                        "Invalid pagination token.".to_string(),
                    ))
                }
            };
            search
                .items
                .additional_fields
                .insert("offset".to_string(), json!(offset));
        }
        search.items.additional_fields.shift_remove("token");
    }

    // Scope: intersect requested collections with the catalog's
    // descendants. A request naming only out-of-scope collections is a
    // 403 (upstream semantics); parsing happens after this check.
    let allowed_collections = state.store.get_descendant_collections(scope_id).await?;
    let explicit_collections = search
        .items
        .additional_fields
        .shift_remove("__explicit_collections")
        .is_some();
    if !search.collections.is_empty() || explicit_collections {
        search
            .collections
            .retain(|c| allowed_collections.contains(c));
        if search.collections.is_empty() {
            if scope_id != ROOT_CATALOG_ID {
                return Err(ApiError::Forbidden(
                    "Requested collections are outside the scope of this catalog.".to_string(),
                ));
            }
            return Ok(geo_json(empty()?));
        }
    } else {
        search.collections = allowed_collections.iter().cloned().collect();
    }

    // Deferred GET params the crate can't model — parsed now, in
    // upstream order (query → intersects → filter-lang).
    if let Some(raw) = search
        .items
        .additional_fields
        .get("query")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
    {
        match serde_json::from_str::<serde_json::Value>(&raw) {
            Ok(serde_json::Value::Object(map)) => search.items.query = Some(map),
            Ok(_) => {
                return Err(ApiError::BadRequest(
                    "Invalid parameters provided: query must be a JSON object".to_string(),
                ))
            }
            Err(_) => {
                return Err(ApiError::BadRequest(
                    "Invalid query parameter: expected valid JSON.".to_string(),
                ))
            }
        }
        search.items.additional_fields.shift_remove("query");
    }
    if let Some(raw) = search
        .items
        .additional_fields
        .shift_remove("__intersects")
        .and_then(|v| v.as_str().map(str::to_owned))
    {
        search.intersects = serde_json::from_str(&raw).map_err(|_| {
            ApiError::BadRequest("Invalid intersects parameter: expected valid JSON.".to_string())
        })?;
    }
    if let Some(filter) = &mut search.items.filter {
        if let stac_api::Filter::Cql2Text(raw) = filter {
            if search
                .items
                .additional_fields
                .get("__filter_lang")
                .and_then(|v| v.as_str())
                == Some("cql2-json")
            {
                let parsed: serde_json::Value = serde_json::from_str(raw).map_err(|_| {
                    ApiError::BadRequest(
                        "Invalid filter parameter: expected valid CQL2 JSON.".to_string(),
                    )
                })?;
                let map = parsed.as_object().cloned().ok_or_else(|| {
                    ApiError::BadRequest(
                        "Invalid filter parameter: expected a CQL2 expression with an operator."
                            .to_string(),
                    )
                })?;
                *filter = stac_api::Filter::Cql2Json(map);
            } else {
                *raw = crate::store::rewrite_between(raw);
            }
        }
    }
    search.items.additional_fields.shift_remove("__filter_lang");

    // Antimeridian: a bbox with west > east means "crosses the dateline"
    // per spec, but the stac crate's `Bbox::is_valid` rejects it. Rewrite
    // crossing bboxes as a two-piece `intersects` MultiPolygon split at
    // ±180 before validation — same semantics, no crate fight.
    if let Some(Bbox::TwoDimensional([xmin, ymin, xmax, ymax])) = search.items.bbox {
        if xmin > xmax && ymin <= ymax && search.intersects.is_none() {
            search.intersects = Some(geojson::Geometry::new(geojson::Value::MultiPolygon(vec![
                vec![vec![
                    vec![xmin, ymin],
                    vec![180.0, ymin],
                    vec![180.0, ymax],
                    vec![xmin, ymax],
                    vec![xmin, ymin],
                ]],
                vec![vec![
                    vec![-180.0, ymin],
                    vec![xmax, ymin],
                    vec![xmax, ymax],
                    vec![-180.0, ymax],
                    vec![-180.0, ymin],
                ]],
            ])));
            search.items.bbox = None;
        }
    }

    // Spec-level validation: bbox/intersects mutual exclusion, bbox
    // ordering, and datetime parsing (full RFC3339 — date-only bounds
    // are rejected by the crate).
    search = search
        .valid()
        .map_err(|e| ApiError::BadRequest(format!("Invalid parameters provided: {e}")))?;

    // 3. Extension translation — `query` (STACQL) and `filter` (CQL2)
    // become extra OpenSearch filter clauses. Upstream validates these
    // before the empty-scope short-circuit, so a bad literal on an
    // empty catalog still surfaces its 400.
    let mut extra_filters = Vec::new();
    if let Some(query) = &search.items.query {
        extra_filters.extend(
            crate::store::stacql_clauses(query)
                .map_err(|e| ApiError::BadRequest(format!("Invalid query parameter: {e}")))?,
        );
    }
    if let Some(filter) = &search.items.filter {
        extra_filters.push(crate::store::cql2_clause(filter).map_err(bad_filter)?);
    }

    // A scope with no collections (or a root request filtered to
    // nothing) short-circuits — after parsing, so malformed extension
    // params still surface their 400 (#935 semantics).
    if search.collections.is_empty() {
        return Ok(geo_json(empty()?));
    }

    let (mut items, matched) = state
        .store
        .search_items(&search.collections, &search, extra_filters)
        .await?;
    let base = &state.base_url;
    let link_scope = (scope_id != ROOT_CATALOG_ID).then_some(scope_id);
    for item in items.iter_mut() {
        let item_id = item["id"].as_str().unwrap_or_default().to_string();
        if let Some(col) = item["collection"].as_str().map(str::to_owned) {
            let mut doc = serde_json::Value::Object(std::mem::take(item));
            merge_doc_links(
                &mut doc,
                item_links(state, &item_id, &col, link_scope),
                base,
            );
            *item = doc.as_object().unwrap().clone();
        }
    }
    for item in items.iter_mut() {
        strip_excluded_item_fields(state, item);
    }
    // Fields extension runs last — it strips generated links too.
    if let Some(fields) = &search.items.fields {
        for item in items.iter_mut() {
            crate::store::filter_fields(item, &fields.include, &fields.exclude);
        }
    }
    let returned = items.len() as u64;
    let mut collection =
        ItemCollection::new(items).map_err(|e| ApiError::Internal(e.to_string()))?;
    collection.number_matched = Some(matched);
    collection.number_returned = Some(returned);
    collection.links.push(link_json(format!("{base}/"), "root"));

    // 4. Pagination links — upstream PagingLinks: GET merges the request
    // URL with a `token` query param (`method: GET`); POST carries the
    // search body + `token` (`method: POST`). Self = the request URL.
    let limit = search
        .items
        .limit
        .unwrap_or(DEFAULT_SEARCH_LIMIT)
        .min(MAX_SEARCH_LIMIT);
    let offset = search_offset(&search);
    let get_uri = req.as_ref().filter(|(_, is_get)| *is_get).map(|(u, _)| u);
    let base = &state.base_url;
    let self_href = match &req {
        Some((u, _)) => format!("{base}{u}"),
        None => {
            if scope_id == ROOT_CATALOG_ID {
                format!("{base}/catalogs/search")
            } else {
                format!("{base}/catalogs/{scope_id}/search")
            }
        }
    };
    let post_href = match &req {
        Some((u, _)) => format!("{base}{}", u.path()),
        None => self_href.clone(),
    };
    collection.links.push(link_json(self_href, "self"));
    if offset + returned < matched {
        collection.links.push(match get_uri {
            Some(u) => page_link_get("next", base, u, offset + limit),
            None => page_link("next", &post_href, &search, offset + limit),
        });
    }
    if offset > 0 {
        collection.links.push(match get_uri {
            Some(u) => page_link_get("prev", base, u, offset.saturating_sub(limit)),
            None => page_link("prev", &post_href, &search, offset.saturating_sub(limit)),
        });
    }
    Ok(geo_json(collection))
}

/// GET-side pagination link: rel + href (request URL with `token`
/// merged in) + `method: GET` — upstream `PagingLinks.link_next`.
fn page_link_get(rel: &str, base: &str, uri: &axum::http::Uri, offset: u64) -> Link {
    let pq = uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or_else(|| uri.path());
    let (path, query) = pq.split_once('?').unwrap_or((pq, ""));
    let kept: Vec<&str> = query
        .split('&')
        .filter(|p| !p.is_empty() && !p.starts_with("token="))
        .collect();
    let mut merged = path.to_string();
    merged.push('?');
    if !kept.is_empty() {
        merged.push_str(&kept.join("&"));
        merged.push('&');
    }
    merged.push_str("token=");
    merged.push_str(&encode_item_token(offset));
    let mut link = Link::new(format!("{base}{merged}"), rel);
    link.method = Some("GET".to_string());
    link.r#type = Some("application/json".to_string());
    link
}

/// A STAC pagination link: rel + href + `method: POST` + the original
/// search body with an opaque `token` for the next page.
fn page_link(rel: &str, href: &str, search: &Search, offset: u64) -> Link {
    let mut body = serde_json::to_value(search).unwrap_or_default();
    body["token"] = json!(encode_item_token(offset));
    let mut link = Link::new(href, rel);
    link.method = Some("POST".to_string());
    link.r#type = Some("application/geo+json".to_string());
    link.body = body.as_object().cloned();
    link
}

/// Opaque pagination token: url-safe base64 of a JSON `[offset]` —
/// upstream tokens are a base64'd JSON list too, so clients can't tell
/// the difference.
fn encode_item_token(offset: u64) -> String {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    URL_SAFE_NO_PAD.encode(format!("[{offset}]"))
}

fn decode_item_token(token: &str) -> Result<u64, ApiError> {
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

/// POST /search honors a `?token=` query parameter only when the body
/// has none (`null` counts as absent).
fn apply_query_token(search: &mut Search, query_token: Option<&String>) {
    let body_token = search.items.additional_fields.get("token");
    if (body_token.is_none() || body_token == Some(&serde_json::Value::Null))
        && query_token.is_some()
    {
        search
            .items
            .additional_fields
            .insert("token".to_string(), json!(query_token));
    }
}

/// Decode `+`/`%XX` — upstream decodes `intersects` twice (clients
/// double-encode), so the once-decoded query value gets a second pass.
fn percent_decode_plus(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    out.push(b);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `GetSearch` -> `Search`, preserving the pieces the crate drops:
/// `intersects` gets a second percent-decode and `filter-lang` is
/// stashed — both are parsed inside `run_scoped_search` so the
/// catalog-scope check precedes parsing errors (upstream order).
fn get_to_search(mut params: GetSearch) -> Result<Search, ApiError> {
    // `?collections=` (empty value) is no constraint — upstream's str2list
    // produces an empty list rather than [""] (#935).
    if params
        .collections
        .as_deref()
        .is_some_and(|c| c.trim().is_empty())
    {
        params.collections = None;
    }
    let intersects_raw = params.intersects.take();
    let filter_lang = params.items.filter_lang.take();
    let mut search = Search::try_from(params)
        .map_err(|e| ApiError::BadRequest(format!("Invalid parameters provided: {e}")))?;
    if let Some(raw) = intersects_raw {
        search
            .items
            .additional_fields
            .insert("__intersects".to_string(), json!(percent_decode_plus(&raw)));
    }
    if let Some(lang) = filter_lang {
        search
            .items
            .additional_fields
            .insert("__filter_lang".to_string(), json!(lang));
    }
    Ok(search)
}

/// POST /search body. The stac-api crate's `Fields` requires both `include`
/// and `exclude` members; the extension spec (and upstream) defaults each
/// side to empty — normalize missing members before deserializing.
pub struct SearchBody(pub Search);

impl<'de> serde::Deserialize<'de> for SearchBody {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let mut value = serde_json::Value::deserialize(deserializer)?;
        if let Some(fields) = value.get_mut("fields").and_then(|f| f.as_object_mut()) {
            fields.entry("include").or_insert_with(|| json!([]));
            fields.entry("exclude").or_insert_with(|| json!([]));
        }
        // `filter` without `filter-lang` defaults to cql2-json per spec;
        // the crate's Filter is adjacently tagged and requires it.
        if value.get("filter").is_some() && value.get("filter-lang").is_none() {
            value["filter-lang"] = json!("cql2-json");
        }
        // `"collections": []` names zero collections — upstream matches
        // nothing; absent `collections` instead scopes to all (#935).
        let explicit_collections = value.get("collections").is_some();
        let mut search = serde_json::from_value::<Search>(value).map_err(D::Error::custom)?;
        if explicit_collections && search.collections.is_empty() {
            search
                .items
                .additional_fields
                .insert("__explicit_collections".to_string(), json!(true));
        }
        Ok(Self(search))
    }
}

#[utoipa::path(
    post,
    path = "/catalogs/{catalog_id}/search",
    tag = "search",
    summary = "Item search within a catalog (POST)",
    params(("catalog_id" = String, Path, description = "Catalog id")),
    request_body = serde_json::Value,
    responses(
        (status = 200, description = "Successful response"),
        (status = 400, description = "Invalid request"),
        (status = 404, description = "Not found")
    )
)]
pub async fn scoped_search_post(
    Path(catalog_id): Path<String>,
    State(state): State<Arc<AppState>>,
    uri: axum::http::Uri,
    Query(params): Query<HashMap<String, String>>,
    StacJson(search): StacJson<SearchBody>,
) -> Result<impl IntoResponse, ApiError> {
    let mut search = search.0;
    apply_query_token(&mut search, params.get("token"));
    run_scoped_search(&state, &catalog_id, search, Some((uri, false))).await
}

#[utoipa::path(
    get,
    path = "/catalogs/{catalog_id}/search",
    tag = "search",
    summary = "Item search within a catalog (GET)",
    params(
        ("catalog_id" = String, Path, description = "Catalog id"),
        ("collections" = Option<String>, Query, description = "Comma-separated collection ids to search"),
        ("ids" = Option<String>, Query, description = "Comma-separated item ids to return"),
        ("bbox" = Option<String>, Query, description = "Bounding box as minx,miny,maxx,maxy (antimeridian supported)"),
        ("intersects" = Option<String>, Query, description = "GeoJSON geometry to intersect"),
        ("datetime" = Option<String>, Query, description = "RFC 3339 datetime or open interval"),
        ("limit" = Option<u64>, Query, description = "Maximum items per page"),
        ("token" = Option<String>, Query, description = "Pagination token from a `next` link"),
        ("sortby" = Option<String>, Query, description = "Sort: +field/-field or field:asc/desc, comma-separated"),
        ("fields" = Option<String>, Query, description = "Field filtering: +include/-exclude, comma-separated"),
        ("q" = Option<String>, Query, description = "Free-text search"),
        ("query" = Option<String>, Query, description = "STACQL query (JSON-encoded on GET)"),
        ("filter" = Option<String>, Query, description = "CQL2 filter (text or JSON-encoded)"),
        ("filter-lang" = Option<String>, Query, description = "Filter language: cql2-text or cql2-json"),
        ("filter-crs" = Option<String>, Query, description = "CRS for filter geometries")
    ),
    responses(
        (status = 200, description = "Successful response"),
        (status = 400, description = "Invalid request"),
        (status = 404, description = "Not found")
    )
)]
pub async fn scoped_search_get(
    Path(catalog_id): Path<String>,
    State(state): State<Arc<AppState>>,
    uri: axum::http::Uri,
    Query(params): Query<GetSearch>,
) -> Result<impl IntoResponse, ApiError> {
    let search = get_to_search(params)?;
    run_scoped_search(&state, &catalog_id, search, Some((uri, true))).await
}

/// Search across the whole catalogs registry — equivalent to scoping to
/// `root`, since every orphan is adopted there.
#[utoipa::path(
    post,
    path = "/catalogs/search",
    tag = "search",
    summary = "Registry-wide item search (POST)",
    request_body = serde_json::Value,
    responses(
        (status = 200, description = "Successful response"),
        (status = 400, description = "Invalid request"),
        (status = 404, description = "Not found")
    )
)]
pub async fn catalogs_search_post(
    State(state): State<Arc<AppState>>,
    uri: axum::http::Uri,
    Query(params): Query<HashMap<String, String>>,
    StacJson(search): StacJson<SearchBody>,
) -> Result<impl IntoResponse, ApiError> {
    let mut search = search.0;
    apply_query_token(&mut search, params.get("token"));
    run_scoped_search(&state, ROOT_CATALOG_ID, search, Some((uri, false))).await
}

#[utoipa::path(
    get,
    path = "/catalogs/search",
    tag = "search",
    summary = "Registry-wide item search (GET)",
    params(
        ("collections" = Option<String>, Query, description = "Comma-separated collection ids to search"),
        ("ids" = Option<String>, Query, description = "Comma-separated item ids to return"),
        ("bbox" = Option<String>, Query, description = "Bounding box as minx,miny,maxx,maxy (antimeridian supported)"),
        ("intersects" = Option<String>, Query, description = "GeoJSON geometry to intersect"),
        ("datetime" = Option<String>, Query, description = "RFC 3339 datetime or open interval"),
        ("limit" = Option<u64>, Query, description = "Maximum items per page"),
        ("token" = Option<String>, Query, description = "Pagination token from a `next` link"),
        ("sortby" = Option<String>, Query, description = "Sort: +field/-field or field:asc/desc, comma-separated"),
        ("fields" = Option<String>, Query, description = "Field filtering: +include/-exclude, comma-separated"),
        ("q" = Option<String>, Query, description = "Free-text search"),
        ("query" = Option<String>, Query, description = "STACQL query (JSON-encoded on GET)"),
        ("filter" = Option<String>, Query, description = "CQL2 filter (text or JSON-encoded)"),
        ("filter-lang" = Option<String>, Query, description = "Filter language: cql2-text or cql2-json"),
        ("filter-crs" = Option<String>, Query, description = "CRS for filter geometries")
    ),
    responses(
        (status = 200, description = "Successful response"),
        (status = 400, description = "Invalid request"),
        (status = 404, description = "Not found")
    )
)]
pub async fn catalogs_search_get(
    State(state): State<Arc<AppState>>,
    uri: axum::http::Uri,
    Query(params): Query<GetSearch>,
) -> Result<impl IntoResponse, ApiError> {
    let search = get_to_search(params)?;
    run_scoped_search(&state, ROOT_CATALOG_ID, search, Some((uri, true))).await
}

/// `GET /search` — canonical registry-wide search; same handler surface as
/// `/catalogs/search`, annotated separately so each path appears once in the
/// OpenAPI doc.
#[utoipa::path(
    get,
    path = "/search",
    tag = "search",
    summary = "Registry-wide item search (GET)",
    params(
        ("collections" = Option<String>, Query, description = "Comma-separated collection ids to search"),
        ("ids" = Option<String>, Query, description = "Comma-separated item ids to return"),
        ("bbox" = Option<String>, Query, description = "Bounding box as minx,miny,maxx,maxy (antimeridian supported)"),
        ("intersects" = Option<String>, Query, description = "GeoJSON geometry to intersect"),
        ("datetime" = Option<String>, Query, description = "RFC 3339 datetime or open interval"),
        ("limit" = Option<u64>, Query, description = "Maximum items per page"),
        ("token" = Option<String>, Query, description = "Pagination token from a `next` link"),
        ("sortby" = Option<String>, Query, description = "Sort: +field/-field or field:asc/desc, comma-separated"),
        ("fields" = Option<String>, Query, description = "Field filtering: +include/-exclude, comma-separated"),
        ("q" = Option<String>, Query, description = "Free-text search"),
        ("query" = Option<String>, Query, description = "STACQL query (JSON-encoded on GET)"),
        ("filter" = Option<String>, Query, description = "CQL2 filter (text or JSON-encoded)"),
        ("filter-lang" = Option<String>, Query, description = "Filter language: cql2-text or cql2-json"),
        ("filter-crs" = Option<String>, Query, description = "CRS for filter geometries")
    ),
    responses(
        (status = 200, description = "Successful response"),
        (status = 400, description = "Invalid request"),
        (status = 404, description = "Not found")
    )
)]
pub async fn search_get(
    state: State<Arc<AppState>>,
    uri: axum::http::Uri,
    params: Query<GetSearch>,
) -> Result<impl IntoResponse, ApiError> {
    catalogs_search_get(state, uri, params).await
}

/// `POST /search` — canonical registry-wide search (see [`search_get`]).
#[utoipa::path(
    post,
    path = "/search",
    tag = "search",
    summary = "Registry-wide item search (POST)",
    request_body = serde_json::Value,
    responses(
        (status = 200, description = "Successful response"),
        (status = 400, description = "Invalid request"),
        (status = 404, description = "Not found")
    )
)]
pub async fn search_post(
    state: State<Arc<AppState>>,
    uri: axum::http::Uri,
    params: Query<HashMap<String, String>>,
    search: StacJson<SearchBody>,
) -> Result<impl IntoResponse, ApiError> {
    catalogs_search_post(state, uri, params, search).await
}

// --- Transaction Handlers ---

#[utoipa::path(
    post,
    path = "/catalogs/{catalog_id}/catalogs",
    tag = "catalogs",
    summary = "Create or link a sub-catalog",
    params(("catalog_id" = String, Path, description = "Catalog id")),
    request_body = serde_json::Value,
    responses(
        (status = 200, description = "Successful response"),
        (status = 400, description = "Invalid request"),
        (status = 404, description = "Not found")
    )
)]
pub async fn link_or_create_sub_catalog(
    Path(catalog_id): Path<String>,
    State(state): State<Arc<AppState>>,
    StacJson(payload): StacJson<CreateOrLinkPayload<Catalog>>,
) -> Result<impl IntoResponse, ApiError> {
    require_catalog(&state, &catalog_id).await?;
    match payload {
        CreateOrLinkPayload::LinkReference { id } => {
            // Mode B: link must target an existing catalog — 404 otherwise
            if state
                .store
                .get_document(CATALOGS_INDEX, &id)
                .await?
                .is_none()
            {
                return Err(ApiError::NotFound(id));
            }
            if !state
                .store
                .link(&id, &catalog_id, NodeKind::Catalog)
                .await?
            {
                return Err(ApiError::WriteConflict(id));
            }
            let (mut doc, version) = state
                .store
                .get_document_versioned(CATALOGS_INDEX, &id)
                .await?
                .unwrap();
            let parents = state.store.get_parents(&id).await?;
            merge_doc_links(
                &mut doc,
                catalog_hierarchy_links(&state, &parents),
                &state.base_url,
            );
            Ok(etag_response(
                StatusCode::OK,
                &etag_of(&state, CATALOGS_INDEX, &version),
                Json(doc),
            ))
        }
        CreateOrLinkPayload::FullResource(mut new_catalog) => {
            filter_stored_catalog_links(&mut new_catalog.links, &state.base_url);
            // The path parent is the edge — body `parent_ids` is ignored.
            new_catalog.additional_fields.remove("parent_ids");
            // Mode A: full-body repost of an existing id is a conflict
            for index in [CATALOGS_INDEX, COLLECTIONS_INDEX] {
                if state
                    .store
                    .get_document(index, &new_catalog.id)
                    .await?
                    .is_some()
                {
                    return Err(ApiError::Conflict(new_catalog.id.clone()));
                }
            }
            if !state
                .store
                .set_parents(&new_catalog.id, vec![catalog_id], NodeKind::Catalog)
                .await?
            {
                return Err(ApiError::WriteConflict(new_catalog.id));
            }
            if !state
                .store
                .create_document(CATALOGS_INDEX, &new_catalog.id, &new_catalog)
                .await?
            {
                return Err(ApiError::Conflict(new_catalog.id));
            }
            let (_, version) = state
                .store
                .get_document_versioned(CATALOGS_INDEX, &new_catalog.id)
                .await?
                .ok_or_else(|| ApiError::Internal("catalog vanished after create".into()))?;
            Ok(etag_response(
                StatusCode::CREATED,
                &etag_of(&state, CATALOGS_INDEX, &version),
                Json(catalog_write_doc(&state, &new_catalog).await?),
            ))
        }
    }
}

#[utoipa::path(
    delete,
    path = "/catalogs/{catalog_id}/catalogs/{sub_id}",
    tag = "catalogs",
    summary = "Unlink a sub-catalog (removes parent edge only)",
    params(("catalog_id" = String, Path, description = "Catalog id"), ("sub_id" = String, Path, description = "Sub-catalog id")),
    responses(
        (status = 200, description = "Successful response"),
        (status = 400, description = "Invalid request"),
        (status = 404, description = "Not found")
    )
)]
pub async fn unlink_sub_catalog(
    Path((catalog_id, sub_id)): Path<(String, String)>,
    State(state): State<Arc<AppState>>,
) -> Result<StatusCode, ApiError> {
    // The edge must exist AND the child must be a catalog document —
    // unlinking a non-child or a non-catalog is a 404
    let parents = state.store.get_parents(&sub_id).await?;
    let is_catalog = state
        .store
        .get_document(CATALOGS_INDEX, &sub_id)
        .await?
        .is_some();
    if !parents.iter().any(|p| p == &catalog_id) || !is_catalog {
        return Err(ApiError::NotFound(format!("{catalog_id}/{sub_id}")));
    }
    // Unlink only; orphans are adopted by root rather than deleted
    if !state.store.unlink_and_adopt(&sub_id, &catalog_id).await? {
        return Err(ApiError::WriteConflict(sub_id));
    }
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    get,
    path = "/catalogs/{catalog_id}/collections",
    tag = "collections",
    summary = "List collections in a catalog",
    params(("catalog_id" = String, Path, description = "Catalog id"), ScopedCollectionsQuery),
    responses(
        (status = 200, description = "Successful response"),
        (status = 400, description = "Invalid request"),
        (status = 404, description = "Not found")
    )
)]
pub async fn list_scoped_collections(
    Path(catalog_id): Path<String>,
    RawQuery(raw): RawQuery,
    Query(query): Query<ScopedCollectionsQuery>,
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, ApiError> {
    let clause = listing_filter_clause(
        query.filter.as_deref(),
        query.filter_lang.as_deref(),
        crate::store::COLLECTION_FIELDS,
    )?;
    require_catalog(&state, &catalog_id).await?;
    let (limit, offset) = list_window(
        query.limit,
        query.token.as_deref(),
        state.default_collection_limit,
        state.max_collection_limit,
    )?;
    // Collections-search param surface on the scoped route — same
    // semantics as /collections-search, restricted to catalog members.
    let sortby = parse_get_sortby(query.sortby.as_deref());
    validate_collection_sortby(&state, &sortby).await?;
    let fields = parse_get_fields(query.fields.as_deref());
    let ids = state
        .store
        .get_children_by_kind(&catalog_id, NodeKind::Collection)
        .await?;
    let mut filters = Vec::new();
    if let Some(q) = query.q.as_deref().filter(|s| !s.trim().is_empty()) {
        filters.push(json!({"multi_match": {
            "query": q,
            "fields": ["id", "title^3", "description", "keywords"],
            "type": "best_fields",
            "fuzziness": "AUTO"
        }}));
    }
    if let Some(c) = clause {
        filters.push(c);
    }
    let (mut collections, matched): (Vec<serde_json::Value>, usize) =
        if !filters.is_empty() || !sortby.is_empty() {
            filters.insert(0, json!({"terms": {"id": ids}}));
            let (docs, m) = state
                .store
                .search_collections(filters, sortby, limit, offset)
                .await?;
            (docs, m as usize)
        } else {
            let page: Vec<String> = ids
                .iter()
                .skip(offset as usize)
                .take(limit as usize)
                .cloned()
                .collect();
            (
                state.store.get_documents(COLLECTIONS_INDEX, &page).await?,
                ids.len(),
            )
        };
    let page: Vec<String> = collections
        .iter()
        .filter_map(|d| d["id"].as_str().map(str::to_owned))
        .collect();
    let parents_map = state.store.get_parents_many(&page).await?;
    for doc in collections.iter_mut() {
        strip_collection_internals(doc);
        if let Some(id) = doc["id"].as_str().map(str::to_owned) {
            let parents = parents_map.get(&id).cloned().unwrap_or_default();
            merge_doc_links(
                doc,
                collection_links(&state, &id, &catalog_id, &parents),
                &state.base_url,
            );
        }
        if let (Some(obj), Some((include, exclude))) = (doc.as_object_mut(), &fields) {
            crate::store::filter_fields(obj, include, exclude);
        }
    }
    let mut links = json!([
        { "rel": "self", "href": format!("{}/catalogs/{catalog_id}/collections", state.base_url) },
        { "rel": "root", "href": state.base_url },
        { "rel": "parent", "href": format!("{}/catalogs/{catalog_id}", state.base_url) }
    ]);
    let returned = collections.len();
    let extra = pagination_extra(raw.as_deref());
    if let Some(next) = list_next_link(
        &state,
        &format!("/catalogs/{catalog_id}/collections"),
        limit,
        offset,
        returned,
        matched,
        extra.as_deref(),
    ) {
        links.as_array_mut().unwrap().push(next);
    }
    Ok(Json(json!({
        "collections": collections,
        "numberReturned": returned,
        "numberMatched": matched,
        "links": links
    })))
}

#[utoipa::path(
    post,
    path = "/catalogs/{catalog_id}/collections",
    tag = "collections",
    summary = "Create or link a collection in a catalog",
    params(("catalog_id" = String, Path, description = "Catalog id")),
    request_body = serde_json::Value,
    responses(
        (status = 200, description = "Successful response"),
        (status = 400, description = "Invalid request"),
        (status = 404, description = "Not found")
    )
)]
pub async fn link_or_create_scoped_collection(
    Path(catalog_id): Path<String>,
    State(state): State<Arc<AppState>>,
    StacJson(payload): StacJson<CreateOrLinkPayload<Collection>>,
) -> Result<impl IntoResponse, ApiError> {
    require_catalog(&state, &catalog_id).await?;
    match payload {
        CreateOrLinkPayload::LinkReference { id } => {
            // Mode B: link must target an existing collection — 404 otherwise
            if state
                .store
                .get_document(COLLECTIONS_INDEX, &id)
                .await?
                .is_none()
            {
                return Err(ApiError::NotFound(id));
            }
            if !state
                .store
                .link(&id, &catalog_id, NodeKind::Collection)
                .await?
            {
                return Err(ApiError::WriteConflict(id));
            }
            let (mut doc, version) = state
                .store
                .get_document_versioned(COLLECTIONS_INDEX, &id)
                .await?
                .unwrap();
            strip_collection_internals(&mut doc);
            let parents = state.store.get_parents(&id).await?;
            merge_doc_links(
                &mut doc,
                collection_links(&state, &id, &catalog_id, &parents),
                &state.base_url,
            );
            Ok(etag_response(
                StatusCode::OK,
                &etag_of(&state, COLLECTIONS_INDEX, &version),
                Json(doc),
            ))
        }
        CreateOrLinkPayload::FullResource(collection) => {
            // Mode A: full-body repost of an existing id is a conflict
            for index in [COLLECTIONS_INDEX, CATALOGS_INDEX] {
                if state
                    .store
                    .get_document(index, &collection.id)
                    .await?
                    .is_some()
                {
                    return Err(ApiError::Conflict(collection.id.clone()));
                }
            }
            if !state
                .store
                .set_parents(
                    &collection.id,
                    vec![catalog_id.clone()],
                    NodeKind::Collection,
                )
                .await?
            {
                return Err(ApiError::WriteConflict(collection.id));
            }
            if !state
                .store
                .create_document(
                    COLLECTIONS_INDEX,
                    &collection.id,
                    &stored_collection_doc(&state, &collection),
                )
                .await?
            {
                return Err(ApiError::Conflict(collection.id));
            }
            let (_, version) = state
                .store
                .get_document_versioned(COLLECTIONS_INDEX, &collection.id)
                .await?
                .ok_or_else(|| ApiError::Internal("collection vanished after create".into()))?;
            Ok(etag_response(
                StatusCode::CREATED,
                &etag_of(&state, COLLECTIONS_INDEX, &version),
                Json(collection_doc(&state, &collection, &catalog_id).await?),
            ))
        }
    }
}

#[utoipa::path(
    get,
    path = "/catalogs/{catalog_id}/collections/{collection_id}",
    tag = "collections",
    summary = "Get a collection in a catalog",
    params(("catalog_id" = String, Path, description = "Catalog id"), ("collection_id" = String, Path, description = "Collection id")),
    responses(
        (status = 200, description = "Successful response"),
        (status = 400, description = "Invalid request"),
        (status = 404, description = "Not found")
    )
)]
pub async fn get_scoped_collection(
    Path((catalog_id, collection_id)): Path<(String, String)>,
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, ApiError> {
    require_scoped_collection(&state, &catalog_id, &collection_id).await?;
    match state
        .store
        .get_document_versioned(COLLECTIONS_INDEX, &collection_id)
        .await?
    {
        Some((mut doc, version)) => {
            strip_collection_internals(&mut doc);
            let parents = state.store.get_parents(&collection_id).await?;
            merge_doc_links(
                &mut doc,
                collection_links(&state, &collection_id, &catalog_id, &parents),
                &state.base_url,
            );
            Ok(etag_response(
                StatusCode::OK,
                &etag_of(&state, COLLECTIONS_INDEX, &version),
                Json(doc),
            ))
        }
        None => Err(ApiError::NotFound(collection_id)),
    }
}

#[utoipa::path(
    put,
    path = "/catalogs/{catalog_id}/collections/{collection_id}",
    tag = "collections",
    summary = "Replace a collection in a catalog",
    params(("catalog_id" = String, Path, description = "Catalog id"), ("collection_id" = String, Path, description = "Collection id"), ("If-Match" = Option<String>, Header, description = "Conditional write: apply only if the resource ETag matches")),
    request_body = serde_json::Value,
    responses(
        (status = 200, description = "Successful response"),
        (status = 400, description = "Invalid request"),
        (status = 404, description = "Not found"),
        (status = 412, description = "If-Match precondition failed"),
        (status = 428, description = "If-Match header required")
    )
)]
pub async fn update_scoped_collection(
    Path((catalog_id, collection_id)): Path<(String, String)>,
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    StacJson(collection): StacJson<Collection>,
) -> Result<impl IntoResponse, ApiError> {
    require_scoped_collection(&state, &catalog_id, &collection_id).await?;
    if collection.id != collection_id {
        return Err(ApiError::BadRequest(format!(
            "body id '{}' does not match path id '{collection_id}'",
            collection.id
        )));
    }
    let (_, version) = state
        .store
        .get_document_versioned(COLLECTIONS_INDEX, &collection_id)
        .await?
        .ok_or_else(|| ApiError::NotFound(collection_id.clone()))?;
    require_if_match(&headers, &etag_of(&state, COLLECTIONS_INDEX, &version))?;
    let applied = if headers.contains_key(header::IF_MATCH) {
        state
            .store
            .index_document_versioned(
                COLLECTIONS_INDEX,
                &collection_id,
                &stored_collection_doc(&state, &collection),
                version,
            )
            .await?
    } else {
        state
            .store
            .replace_document(
                COLLECTIONS_INDEX,
                &collection_id,
                &stored_collection_doc(&state, &collection),
            )
            .await?
    };
    if !applied {
        return Err(if headers.contains_key(header::IF_MATCH) {
            ApiError::PreconditionFailed
        } else {
            ApiError::WriteConflict(collection_id)
        });
    }
    let (_, version) = state
        .store
        .get_document_versioned(COLLECTIONS_INDEX, &collection_id)
        .await?
        .ok_or_else(|| ApiError::Internal("collection vanished after write".into()))?;
    Ok(etag_response(
        StatusCode::OK,
        &etag_of(&state, COLLECTIONS_INDEX, &version),
        Json(collection_doc(&state, &collection, &catalog_id).await?),
    ))
}

#[utoipa::path(
    delete,
    path = "/catalogs/{catalog_id}/collections/{collection_id}",
    tag = "collections",
    summary = "Unlink a collection from a catalog",
    params(("catalog_id" = String, Path, description = "Catalog id"), ("collection_id" = String, Path, description = "Collection id")),
    responses(
        (status = 200, description = "Successful response"),
        (status = 400, description = "Invalid request"),
        (status = 404, description = "Not found")
    )
)]
pub async fn unlink_scoped_collection(
    Path((catalog_id, collection_id)): Path<(String, String)>,
    State(state): State<Arc<AppState>>,
) -> Result<StatusCode, ApiError> {
    // The edge must exist AND the child must be a collection document
    let parents = state.store.get_parents(&collection_id).await?;
    let is_collection = state
        .store
        .get_document(COLLECTIONS_INDEX, &collection_id)
        .await?
        .is_some();
    if !parents.iter().any(|p| p == &catalog_id) || !is_collection {
        return Err(ApiError::NotFound(format!("{catalog_id}/{collection_id}")));
    }
    if !state
        .store
        .unlink_and_adopt(&collection_id, &catalog_id)
        .await?
    {
        return Err(ApiError::WriteConflict(collection_id));
    }
    Ok(StatusCode::NO_CONTENT)
}

// --- Global Collections (core STAC routes) ---

/// GET /collections — every collection in the registry. With
/// `enable_collections_search` off (upstream `ENABLE_COLLECTIONS_SEARCH`,
/// default on) the search params are ignored: plain `limit`/`token`
/// listing, mirroring upstream's bare request model.
#[utoipa::path(
    get,
    path = "/collections",
    tag = "collections",
    summary = "List collections (collections-search params)",
    params(CollectionsQuery),
    responses(
        (status = 200, description = "Successful response"),
        (status = 400, description = "Invalid request"),
        (status = 404, description = "Not found")
    )
)]
pub async fn list_collections(
    Query(query): Query<CollectionsQuery>,
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, ApiError> {
    let params = if state.enable_collections_search {
        collections_get_params(query)?
    } else {
        CollectionsSearchParams {
            limit: query.limit,
            token: query.token,
            ..Default::default()
        }
    };
    collections_page(&state, params, "/collections", false).await
}

/// GET /collections-search — upstream collections-search route;
/// same parameter surface as the extended GET /collections.
#[utoipa::path(
    get,
    path = "/collections-search",
    tag = "collections",
    summary = "Search collections",
    params(CollectionsQuery),
    responses(
        (status = 200, description = "Successful response"),
        (status = 400, description = "Invalid request"),
        (status = 404, description = "Not found")
    )
)]
pub async fn collections_search_get(
    Query(query): Query<CollectionsQuery>,
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, ApiError> {
    collections_page(
        &state,
        collections_get_params(query)?,
        "/collections-search",
        false,
    )
    .await
}

/// POST /collections-search — structured body variant
/// (`sortby: [{field,direction}]`, `fields: {include,exclude}`).
#[utoipa::path(
    post,
    path = "/collections-search",
    tag = "collections",
    summary = "Search collections (POST body)",
    request_body = serde_json::Value,
    responses(
        (status = 200, description = "Successful response"),
        (status = 400, description = "Invalid request"),
        (status = 404, description = "Not found")
    )
)]
pub async fn collections_search_post(
    State(state): State<Arc<AppState>>,
    StacJson(body): StacJson<serde_json::Value>,
) -> Result<impl IntoResponse, ApiError> {
    collections_page(
        &state,
        collections_post_params(body)?,
        "/collections-search",
        true,
    )
    .await
}

/// GET-side parameters for /collections and /collections-search.
#[derive(Deserialize, Default, utoipa::IntoParams)]
pub struct CollectionsQuery {
    pub limit: Option<u64>,
    pub token: Option<String>,
    pub sortby: Option<String>,
    pub fields: Option<String>,
    pub q: Option<String>,
    pub filter: Option<String>,
    #[serde(rename = "filter-lang")]
    pub filter_lang: Option<String>,
    pub query: Option<String>,
    pub datetime: Option<String>,
    pub bbox: Option<String>,
}

/// Normalized collections-search params shared by GET and POST.
#[derive(Default)]
struct CollectionsSearchParams {
    limit: Option<u64>,
    token: Option<String>,
    /// (field, "asc"|"desc") pairs.
    sortby: Vec<(String, String)>,
    /// (include, exclude) field paths.
    fields: Option<(Vec<String>, Vec<String>)>,
    q: Vec<String>,
    filter: Option<stac_api::Filter>,
    query: Option<serde_json::Map<String, serde_json::Value>>,
    datetime: Option<String>,
    /// 2D bbox [minx, miny, maxx, maxy].
    bbox: Option<[f64; 4]>,
}

/// `?sortby=+id,-title` -> (field, direction) pairs.
fn parse_get_sortby(raw: Option<&str>) -> Vec<(String, String)> {
    raw.into_iter()
        .flat_map(|s| s.split(','))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| match s.strip_prefix('-') {
            Some(f) => (f.to_string(), "desc".to_string()),
            None => (
                s.strip_prefix('+').unwrap_or(s).to_string(),
                "asc".to_string(),
            ),
        })
        .collect()
}

/// Reject `sortby` fields the collections sortables schema doesn't
/// declare — upstream's `validate_sort_fields_shared` + the engine-side
/// 400 for unmapped text sorts (e.g. `description`, which maps as plain
/// `text` and has no keyword sub-field). Excluded fields get upstream's
/// exact message.
async fn validate_collection_sortby(
    state: &AppState,
    sortby: &[(String, String)],
) -> Result<(), ApiError> {
    if sortby.is_empty() {
        return Ok(());
    }
    for (field, _) in sortby {
        if state
            .excluded_queryables
            .iter()
            .any(|p| field == p || field.starts_with(&format!("{p}.")))
        {
            return Err(ApiError::BadRequest(format!(
                "Field '{field}' is not sortable: it is excluded via EXCLUDED_FROM_QUERYABLES."
            )));
        }
    }
    let mapping = index_mappings(state, COLLECTIONS_INDEX).await?;
    let allowed = crate::store::sortable_properties(&mapping, &state.excluded_queryables);
    for (field, _) in sortby {
        // `datetime` sorts on the temporal extent (upstream remap).
        if field != "datetime" && !allowed.contains_key(field.as_str()) {
            return Err(ApiError::BadRequest(format!(
                "Field '{field}' is not sortable."
            )));
        }
    }
    Ok(())
}

/// `?fields=a,b,-c` -> (include, exclude).
fn parse_get_fields(raw: Option<&str>) -> Option<(Vec<String>, Vec<String>)> {
    let raw = raw?;
    let mut include = Vec::new();
    let mut exclude = Vec::new();
    for e in raw.split(',').map(str::trim).filter(|e| !e.is_empty()) {
        if let Some(f) = e.strip_prefix('-') {
            exclude.push(f.to_string());
        } else {
            include.push(e.strip_prefix('+').unwrap_or(e).to_string());
        }
    }
    Some((include, exclude))
}

/// 4- or 6-element bbox -> 2D (3D drops the altitude bounds, same as
/// upstream's bbox_shape extraction).
fn normalize_bbox(nums: Vec<f64>) -> Result<[f64; 4], ApiError> {
    match nums.len() {
        4 => Ok([nums[0], nums[1], nums[2], nums[3]]),
        6 => Ok([nums[0], nums[1], nums[3], nums[4]]),
        _ => Err(ApiError::BadRequest(
            "Invalid parameters provided: bbox".to_string(),
        )),
    }
}

/// `datetime` param -> (start, end) bounds; `..` maps to concrete
/// sentinels, a single instant is a closed point range (upstream
/// `apply_collections_datetime_filter_shared`).
fn datetime_bounds(datetime: &str) -> (String, String) {
    match datetime.split_once('/') {
        Some((s, e)) => (
            if s == ".." {
                "1800-01-01T00:00:00Z".to_string()
            } else {
                s.to_string()
            },
            if e == ".." {
                "2999-12-31T23:59:59Z".to_string()
            } else {
                e.to_string()
            },
        ),
        None => (datetime.to_string(), datetime.to_string()),
    }
}

fn parse_cql2_filter(raw: &str, lang: Option<&str>) -> Result<stac_api::Filter, ApiError> {
    if lang == Some("cql2-json") {
        match serde_json::from_str::<serde_json::Value>(raw) {
            Ok(serde_json::Value::Object(map)) => Ok(stac_api::Filter::Cql2Json(map)),
            _ => Err(ApiError::BadRequest(
                "Invalid filter parameter: expected valid CQL2 JSON.".to_string(),
            )),
        }
    } else {
        Ok(stac_api::Filter::Cql2Text(crate::store::rewrite_between(
            raw,
        )))
    }
}

fn parse_query_param(raw: &str) -> Result<serde_json::Map<String, serde_json::Value>, ApiError> {
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(serde_json::Value::Object(map)) => Ok(map),
        _ => Err(ApiError::BadRequest(
            "Invalid query parameter: expected valid JSON.".to_string(),
        )),
    }
}

fn collections_get_params(q: CollectionsQuery) -> Result<CollectionsSearchParams, ApiError> {
    let mut p = CollectionsSearchParams {
        limit: q.limit,
        token: q.token,
        sortby: parse_get_sortby(q.sortby.as_deref()),
        fields: parse_get_fields(q.fields.as_deref()),
        q: q.q.into_iter().collect(),
        datetime: q.datetime,
        ..Default::default()
    };
    if let Some(raw) = &q.filter {
        p.filter = Some(parse_cql2_filter(raw, q.filter_lang.as_deref())?);
    }
    if let Some(raw) = &q.query {
        p.query = Some(parse_query_param(raw)?);
    }
    if let Some(raw) = &q.bbox {
        let nums: Vec<f64> = raw
            .split(',')
            .map(|s| s.trim().parse())
            .collect::<Result<_, _>>()
            .map_err(|_| ApiError::BadRequest("Invalid parameters provided: bbox".to_string()))?;
        p.bbox = Some(normalize_bbox(nums)?);
    }
    Ok(p)
}

/// POST /collections-search body -> params. Accepts `filter` as a CQL2
/// JSON object or a text/json string (`filter_lang` or `filter-lang`),
/// `query` as an object or JSON string, and `fields`/`sortby` objects.
fn collections_post_params(body: serde_json::Value) -> Result<CollectionsSearchParams, ApiError> {
    let bad = |m: &str| ApiError::BadRequest(format!("Invalid parameters provided: {m}"));
    let obj = body.as_object();
    let get = |k: &str| obj.and_then(|o| o.get(k));
    let mut p = CollectionsSearchParams {
        limit: get("limit").and_then(|v| v.as_u64()),
        token: get("token").and_then(|v| v.as_str()).map(str::to_owned),
        datetime: get("datetime").and_then(|v| v.as_str()).map(str::to_owned),
        ..Default::default()
    };
    if let Some(v) = get("sortby").and_then(|v| v.as_array()) {
        for s in v {
            let field = s["field"].as_str().ok_or_else(|| bad("sortby"))?;
            let dir = s["direction"].as_str().unwrap_or("asc");
            p.sortby.push((field.to_string(), dir.to_string()));
        }
    }
    if let Some(f) = get("fields").and_then(|v| v.as_object()) {
        let list = |k: &str| {
            f.get(k)
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
                .filter_map(|x| x.as_str().map(str::to_owned))
                .collect()
        };
        p.fields = Some((list("include"), list("exclude")));
    }
    if let Some(v) = get("q") {
        p.q = match v {
            serde_json::Value::String(s) => vec![s.clone()],
            serde_json::Value::Array(a) => a
                .iter()
                .filter_map(|x| x.as_str().map(str::to_owned))
                .collect(),
            _ => vec![],
        };
    }
    let lang = get("filter_lang")
        .or_else(|| get("filter-lang"))
        .and_then(|v| v.as_str());
    if let Some(v) = get("filter") {
        p.filter = Some(match v {
            serde_json::Value::Object(m) => stac_api::Filter::Cql2Json(m.clone()),
            serde_json::Value::String(s) => parse_cql2_filter(s, lang)?,
            _ => return Err(bad("filter")),
        });
    }
    if let Some(v) = get("query") {
        p.query = Some(match v {
            serde_json::Value::Object(m) => m.clone(),
            serde_json::Value::String(s) => parse_query_param(s)?,
            _ => return Err(bad("query")),
        });
    }
    if let Some(v) = get("bbox") {
        let nums: Vec<f64> = v
            .as_array()
            .map(|a| a.iter().filter_map(|x| x.as_f64()).collect())
            .ok_or_else(|| bad("bbox"))?;
        p.bbox = Some(normalize_bbox(nums)?);
    }
    Ok(p)
}

/// Shared runner for GET /collections, GET/POST /collections-search.
/// `post` selects the `next`-link shape (href token vs body token).
async fn collections_page(
    state: &AppState,
    params: CollectionsSearchParams,
    self_path: &str,
    post: bool,
) -> Result<impl IntoResponse, ApiError> {
    let limit = params
        .limit
        .unwrap_or(state.default_collection_limit)
        .min(state.max_collection_limit.unwrap_or(u64::MAX));
    if limit == 0 {
        return Err(ApiError::BadRequest("limit must be >= 1".to_string()));
    }
    // Collection cursors stay lenient — `?token=!!!!` is a 200 (first
    // page), matching upstream's arbitrary-cursor contract here.
    let offset = params
        .token
        .as_deref()
        .map(|t| decode_item_token(t).unwrap_or(0))
        .unwrap_or(0);

    let mut filters = Vec::new();
    if !params.q.is_empty() {
        filters.push(json!({"multi_match": {
            "query": params.q.join(" "),
            "fields": ["id", "title^3", "description", "keywords"],
            "type": "best_fields",
            "fuzziness": "AUTO"
        }}));
    }
    if let Some(f) = &params.filter {
        filters.push(
            crate::store::cql2_clause_with(f, crate::store::COLLECTION_FIELDS)
                .map_err(bad_filter)?,
        );
    }
    if let Some(q) = &params.query {
        filters.extend(
            crate::store::stacql_clauses_with(q, crate::store::COLLECTION_FIELDS)
                .map_err(|e| ApiError::BadRequest(format!("Invalid query parameter: {e}")))?,
        );
    }
    if let Some(dt) = &params.datetime {
        let (start, end) = datetime_bounds(dt);
        filters.push(json!({"bool": {"filter": [
            {"range": {"extent.temporal.interval": {"lte": end}}},
            {"range": {"extent.temporal.interval": {"gte": start}}}
        ]}}));
    }
    if let Some(b) = &params.bbox {
        filters.push(json!({"geo_shape": {"bbox_shape": {
            "shape": {"type": "envelope", "coordinates": [[b[0], b[3]], [b[2], b[1]]]},
            "relation": "intersects"
        }}}));
    }

    let (mut docs, matched) = state
        .store
        .search_collections(filters, params.sortby, limit, offset)
        .await?;
    let ids: Vec<String> = docs
        .iter()
        .filter_map(|d| d["id"].as_str().map(str::to_owned))
        .collect();
    let parents_map = state.store.get_parents_many(&ids).await?;
    for doc in docs.iter_mut() {
        let Some(obj) = doc.as_object_mut() else {
            continue;
        };
        obj.remove("bbox_shape");
        let id = obj.get("id").and_then(|v| v.as_str()).map(str::to_owned);
        if let Some(id) = id {
            let parents = parents_map.get(&id).cloned().unwrap_or_default();
            obj.insert(
                "links".to_string(),
                json!(global_collection_links(state, &id, &parents)),
            );
        }
        if let Some((include, exclude)) = &params.fields {
            crate::store::filter_fields(obj, include, exclude);
        }
    }
    let returned = docs.len();
    let mut links = json!([
        { "rel": "self", "href": format!("{}{self_path}", state.base_url) },
        { "rel": "root", "href": state.base_url }
    ]);
    if state.enable_collections_search || state.enable_collections_search_route {
        links.as_array_mut().unwrap().push(json!({
            "rel": "http://www.opengis.net/def/rel/ogc/1.0/sortables",
            "type": "application/schema+json",
            "href": format!("{}/collections-sortables", state.base_url)
        }));
    }
    if offset + (returned as u64) < matched {
        let next = if post {
            json!({
                "rel": "next",
                "href": format!("{}{self_path}", state.base_url),
                "method": "POST",
                "body": {
                    "limit": limit,
                    "token": encode_item_token(offset + limit)
                }
            })
        } else {
            json!({
                "rel": "next",
                "href": format!(
                    "{}{self_path}?limit={}&token={}",
                    state.base_url,
                    limit,
                    encode_item_token(offset + limit)
                )
            })
        };
        links.as_array_mut().unwrap().push(next);
    }
    Ok(Json(json!({
        "collections": docs,
        "numberReturned": returned,
        "numberMatched": matched,
        "links": links
    })))
}

/// GET /collections/{id} — canonical collection read.
#[utoipa::path(
    get,
    path = "/collections/{collection_id}",
    tag = "collections",
    summary = "Get a collection",
    params(("collection_id" = String, Path, description = "Collection id")),
    responses(
        (status = 200, description = "Successful response"),
        (status = 400, description = "Invalid request"),
        (status = 404, description = "Not found")
    )
)]
pub async fn get_collection(
    Path(collection_id): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, ApiError> {
    let Some((_, version)) = state
        .store
        .get_document_versioned(COLLECTIONS_INDEX, &collection_id)
        .await?
        .filter(|t| is_collection_doc(&t.0))
    else {
        return Err(ApiError::NotFound(collection_id));
    };
    Ok(etag_response(
        StatusCode::OK,
        &etag_of(&state, COLLECTIONS_INDEX, &version),
        Json(global_collection_doc(&state, &collection_id).await?),
    ))
}

/// GET /collections/{id}/items — global item listing for a collection.
/// Upstream-compatible `?limit=&token=` paging only.
#[utoipa::path(
    get,
    path = "/collections/{collection_id}/items",
    tag = "items",
    summary = "List collection items",
    params(("collection_id" = String, Path, description = "Collection id"), ListQuery),
    responses(
        (status = 200, description = "Successful response"),
        (status = 400, description = "Invalid request"),
        (status = 404, description = "Not found")
    )
)]
pub async fn list_collection_items(
    Path(collection_id): Path<String>,
    Query(query): Query<ListQuery>,
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, ApiError> {
    if state
        .store
        .get_document(COLLECTIONS_INDEX, &collection_id)
        .await?
        .is_none()
    {
        return Err(ApiError::NotFound(collection_id));
    }
    let (mut collection, offset) =
        items_page(&state, std::slice::from_ref(&collection_id), &query, None).await?;
    let limit = query.limit.unwrap_or(DEFAULT_LIST_LIMIT);
    collection.links.push(link_json(
        format!("{}/collections/{collection_id}/items", state.base_url),
        "self",
    ));
    collection
        .links
        .push(link_json(format!("{}/", state.base_url), "root"));
    if let Some(next) = items_next_link(
        &state,
        &format!("/collections/{collection_id}/items"),
        limit,
        offset,
        &collection,
        filter_link_extra(query.filter.as_deref(), query.filter_lang.as_deref()).as_deref(),
    ) {
        collection.links.push(next);
    }
    Ok(geo_json(collection))
}

/// GET /collections/{id}/items/{item_id} — global item read.
#[utoipa::path(
    get,
    path = "/collections/{collection_id}/items/{item_id}",
    tag = "items",
    summary = "Get an item",
    params(("collection_id" = String, Path, description = "Collection id"), ("item_id" = String, Path, description = "Item id")),
    responses(
        (status = 200, description = "Successful response"),
        (status = 400, description = "Invalid request"),
        (status = 404, description = "Not found")
    )
)]
pub async fn get_collection_item(
    Path((collection_id, item_id)): Path<(String, String)>,
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, ApiError> {
    if state
        .store
        .get_document(COLLECTIONS_INDEX, &collection_id)
        .await?
        .is_none()
    {
        return Err(ApiError::NotFound(collection_id));
    }
    match state
        .store
        .get_document_versioned(ITEMS_INDEX, &item_key(&item_id, &collection_id))
        .await?
    {
        Some((mut doc, version)) if doc["collection"] == collection_id => {
            merge_doc_links(
                &mut doc,
                item_links(&state, &item_id, &collection_id, None),
                &state.base_url,
            );
            if let Some(obj) = doc.as_object_mut() {
                strip_excluded_item_fields(&state, obj);
            }
            Ok(etag_response(
                StatusCode::OK,
                &etag_of(&state, ITEMS_INDEX, &version),
                geo_json(doc),
            ))
        }
        _ => Err(ApiError::NotFound(item_id)),
    }
}

#[utoipa::path(
    delete,
    path = "/catalogs/{catalog_id}",
    tag = "catalogs",
    summary = "Disband a catalog (unlink children, keep resources)",
    params(("catalog_id" = String, Path, description = "Catalog id"), ("If-Match" = Option<String>, Header, description = "Conditional write: apply only if the resource ETag matches")),
    responses(
        (status = 200, description = "Successful response"),
        (status = 400, description = "Invalid request"),
        (status = 404, description = "Not found"),
        (status = 412, description = "If-Match precondition failed"),
        (status = 428, description = "If-Match header required")
    )
)]
pub async fn disband_catalog(
    Path(catalog_id): Path<String>,
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
) -> Result<StatusCode, ApiError> {
    let current = state
        .store
        .get_document_versioned(CATALOGS_INDEX, &catalog_id)
        .await?;
    if let Some((_, version)) = &current {
        require_if_match(&headers, &etag_of(&state, CATALOGS_INDEX, version))?;
        // Atomic check-and-delete — a raced write between our read
        // and the delete surfaces as 412 *before* any child edges
        // are touched.
        if !state
            .store
            .delete_document_versioned(CATALOGS_INDEX, &catalog_id, *version)
            .await?
        {
            return Err(ApiError::PreconditionFailed);
        }
    }

    // Safety Disband: unlink direct children only and auto-adopt orphans
    // to Root. Cleanup runs when the doc was deleted above, and also when
    // the doc is already gone but the node is still a catalog — a retried
    // delete releases edges left behind by a failed earlier one. A
    // non-catalog node id (e.g. a collection) is left untouched.
    let cleanup = current.is_some()
        || matches!(
            state.store.get_node_kind(&catalog_id).await?,
            Some(NodeKind::Catalog)
        );
    if cleanup {
        let direct_children = state.store.get_children(&catalog_id).await?;
        for child in direct_children {
            if !state.store.unlink_and_adopt(&child, &catalog_id).await? {
                return Err(ApiError::WriteConflict(child));
            }
        }

        // Deleting the node's doc detaches it from all parents (children are
        // derived from `parents` lookups — no reverse cleanup needed)
        state.store.remove_node(&catalog_id).await?;
    }

    if current.is_none() {
        return Err(ApiError::NotFound(catalog_id));
    }
    Ok(StatusCode::NO_CONTENT)
}

// --- Scoped Items (reads: always on; writes: ENABLE_TRANSACTIONS_EXTENSIONS) ---

/// Enforce that the item's declared `collection` matches the path.
fn validate_item_collection(
    item: &mut Item,
    collection_id: &str,
    base_url: &str,
) -> Result<(), ApiError> {
    filter_stored_links(&mut item.links, base_url);
    match &item.collection {
        Some(c) if c != collection_id => Err(ApiError::BadRequest(
            "Collection ID from path does not match Collection ID from request body".to_string(),
        )),
        Some(_) => Ok(()),
        None => {
            item.collection = Some(collection_id.to_string());
            Ok(())
        }
    }
}

#[utoipa::path(
    get,
    path = "/catalogs/{catalog_id}/collections/{collection_id}/items",
    tag = "items",
    summary = "List items in a scoped collection",
    params(("catalog_id" = String, Path, description = "Catalog id"), ("collection_id" = String, Path, description = "Collection id"), ListQuery),
    responses(
        (status = 200, description = "Successful response"),
        (status = 400, description = "Invalid request"),
        (status = 404, description = "Not found")
    )
)]
pub async fn list_scoped_items(
    Path((catalog_id, collection_id)): Path<(String, String)>,
    Query(query): Query<ListQuery>,
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, ApiError> {
    require_scoped_collection(&state, &catalog_id, &collection_id).await?;
    let (mut collection, offset) = items_page(
        &state,
        std::slice::from_ref(&collection_id),
        &query,
        Some(&catalog_id),
    )
    .await?;
    let limit = query.limit.unwrap_or(DEFAULT_LIST_LIMIT);
    collection.links.push(link_json(
        format!(
            "{}/catalogs/{catalog_id}/collections/{collection_id}/items",
            state.base_url
        ),
        "self",
    ));
    collection
        .links
        .push(link_json(format!("{}/", state.base_url), "root"));
    if let Some(next) = items_next_link(
        &state,
        &format!("/catalogs/{catalog_id}/collections/{collection_id}/items"),
        limit,
        offset,
        &collection,
        filter_link_extra(query.filter.as_deref(), query.filter_lang.as_deref()).as_deref(),
    ) {
        collection.links.push(next);
    }
    Ok(geo_json(collection))
}

/// Shared items-listing pipeline: `?limit=&token=` + the features-binding
/// filters (`ids`/`query`/`filter`/`bbox`/`datetime`) -> paginated search.
/// `scoped_catalog_id` selects scoped vs canonical item link paths.
/// Returns the collection and the page offset (for the `next` link).
async fn items_page(
    state: &AppState,
    collections: &[String],
    query: &ListQuery,
    scoped_catalog_id: Option<&str>,
) -> Result<(ItemCollection, u64), ApiError> {
    let (limit, _) = list_window(
        query.limit,
        query.token.as_deref(),
        state.default_item_limit,
        state.max_item_limit,
    )?;
    let offset = query
        .token
        .as_deref()
        .map(decode_item_token)
        .transpose()?
        .unwrap_or(0);
    let mut search = Search::default();
    search.items.limit = Some(limit);
    search
        .items
        .additional_fields
        .insert("offset".to_string(), json!(offset));
    if let Some(ids) = &query.ids {
        search.ids = ids.split(',').map(|s| s.to_string()).collect();
    }
    if let Some(bbox) = &query.bbox {
        let nums: Vec<f64> = bbox
            .split(',')
            .map(|s| s.parse())
            .collect::<Result<_, _>>()
            .map_err(|_| ApiError::BadRequest("Invalid parameters provided: bbox".to_string()))?;
        let parsed: Bbox = nums
            .try_into()
            .map_err(|_| ApiError::BadRequest("Invalid parameters provided: bbox".to_string()))?;
        let in_range = match parsed {
            Bbox::TwoDimensional([xmin, ymin, xmax, ymax])
            | Bbox::ThreeDimensional([xmin, ymin, _, xmax, ymax, _]) => {
                [xmin, xmax].iter().all(|x| (-180.0..=180.0).contains(x))
                    && [ymin, ymax].iter().all(|y| (-90.0..=90.0).contains(y))
            }
        };
        if !in_range {
            return Err(ApiError::BadRequest(
                "Invalid parameters provided: bbox".to_string(),
            ));
        }
        search.items.bbox = Some(parsed);
    }
    if let Some(datetime) = &query.datetime {
        search.items.datetime = Some(datetime.clone());
    }
    if let Some(sortby) = &query.sortby {
        // Features binding is a closed sortables schema — upstream 400s
        // on fields the collection sortables do not declare.
        let allowed = items_sortable_fields(state).await?;
        let mut undeclared: Vec<String> = sortby
            .split(',')
            .map(|s| s.trim().trim_start_matches(['+', '-']).to_string())
            .filter(|f| !allowed.contains_key(f.as_str()))
            .collect();
        if !undeclared.is_empty() {
            undeclared.sort();
            return Err(ApiError::BadRequest(format!(
                "Undeclared sort fields: {}.",
                undeclared.join(", ")
            )));
        }
        search.items.sortby = sortby
            .split(',')
            .map(|s| s.parse().expect("infallible"))
            .collect();
    }
    if let Some(fields) = &query.fields {
        search.items.fields = Some(fields.parse().expect("infallible"));
    }
    let mut extra_filters = Vec::new();
    if let Some(raw) = &query.query {
        match serde_json::from_str::<serde_json::Value>(raw) {
            Ok(serde_json::Value::Object(map)) => extra_filters.extend(
                crate::store::stacql_clauses(&map)
                    .map_err(|e| ApiError::BadRequest(format!("Invalid query parameter: {e}")))?,
            ),
            _ => {
                return Err(ApiError::BadRequest(
                    "Invalid query parameter: expected valid JSON.".to_string(),
                ))
            }
        }
    }
    if let Some(expr) = &query.filter {
        // Upstream listing try-both: cql2-json is strict JSON; cql2-text
        // or an absent filter-lang tries JSON first, then CQL2 text.
        let filter = if query.filter_lang.as_deref() == Some("cql2-json") {
            match serde_json::from_str::<serde_json::Value>(expr) {
                Ok(serde_json::Value::Object(map)) => stac_api::Filter::Cql2Json(map),
                _ => {
                    return Err(ApiError::BadRequest(
                        "Invalid filter parameter: expected valid CQL2 JSON.".to_string(),
                    ))
                }
            }
        } else {
            match serde_json::from_str::<serde_json::Value>(expr) {
                Ok(serde_json::Value::Object(map)) => stac_api::Filter::Cql2Json(map),
                _ => stac_api::Filter::Cql2Text(crate::store::rewrite_between(expr)),
            }
        };
        extra_filters.push(crate::store::cql2_clause(&filter).map_err(bad_filter)?);
    }
    let (mut items, matched) = state
        .store
        .search_items(collections, &search, extra_filters)
        .await?;
    for item in items.iter_mut() {
        let item_id = item["id"].as_str().unwrap_or_default().to_string();
        if let Some(col) = item["collection"].as_str().map(str::to_owned) {
            item.insert(
                "links".to_string(),
                json!(item_links(state, &item_id, &col, scoped_catalog_id)),
            );
        }
    }
    for item in items.iter_mut() {
        strip_excluded_item_fields(state, item);
    }
    // Fields extension runs last — it strips generated links too.
    if let Some(fields) = &search.items.fields {
        for item in items.iter_mut() {
            crate::store::filter_fields(item, &fields.include, &fields.exclude);
        }
    }
    let returned = items.len() as u64;
    let mut collection =
        ItemCollection::new(items).map_err(|e| ApiError::Internal(e.to_string()))?;
    collection.number_matched = Some(matched);
    collection.number_returned = Some(returned);
    Ok((collection, offset))
}

/// `next` link for a paged items listing (GET href with limit+token).
fn items_next_link(
    state: &AppState,
    path: &str,
    limit: u64,
    offset: u64,
    collection: &ItemCollection,
    extra: Option<&str>,
) -> Option<Link> {
    let returned = collection.number_returned.unwrap_or(0);
    let matched = collection.number_matched.unwrap_or(0);
    if offset + returned >= matched {
        return None;
    }
    let mut href = format!(
        "{}{path}?limit={}&token={}",
        state.base_url,
        limit,
        encode_item_token(offset + limit)
    );
    if let Some(e) = extra {
        href.push_str(e);
    }
    let mut link = Link::new(href, "next");
    link.r#type = Some("application/geo+json".to_string());
    Some(link)
}

#[utoipa::path(
    get,
    path = "/catalogs/{catalog_id}/collections/{collection_id}/items/{item_id}",
    tag = "items",
    summary = "Get an item in a scoped collection",
    params(("catalog_id" = String, Path, description = "Catalog id"), ("collection_id" = String, Path, description = "Collection id"), ("item_id" = String, Path, description = "Item id")),
    responses(
        (status = 200, description = "Successful response"),
        (status = 400, description = "Invalid request"),
        (status = 404, description = "Not found")
    )
)]
pub async fn get_scoped_item(
    Path((catalog_id, collection_id, item_id)): Path<(String, String, String)>,
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, ApiError> {
    require_scoped_collection(&state, &catalog_id, &collection_id).await?;
    match state
        .store
        .get_document_versioned(ITEMS_INDEX, &item_key(&item_id, &collection_id))
        .await?
    {
        Some((mut doc, version)) if doc["collection"] == collection_id => {
            merge_doc_links(
                &mut doc,
                item_links(&state, &item_id, &collection_id, Some(&catalog_id)),
                &state.base_url,
            );
            if let Some(obj) = doc.as_object_mut() {
                strip_excluded_item_fields(&state, obj);
            }
            Ok(etag_response(
                StatusCode::OK,
                &etag_of(&state, ITEMS_INDEX, &version),
                geo_json(doc),
            ))
        }
        _ => Err(ApiError::NotFound(item_id)),
    }
}

#[utoipa::path(
    post,
    path = "/catalogs/{catalog_id}/collections/{collection_id}/items",
    tag = "items",
    summary = "Create an item in a scoped collection",
    params(("catalog_id" = String, Path, description = "Catalog id"), ("collection_id" = String, Path, description = "Collection id")),
    request_body = serde_json::Value,
    responses(
        (status = 200, description = "Successful response"),
        (status = 400, description = "Invalid request"),
        (status = 404, description = "Not found")
    )
)]
pub async fn create_scoped_item(
    Path((catalog_id, collection_id)): Path<(String, String)>,
    State(state): State<Arc<AppState>>,
    StacJson(mut item): StacJson<Item>,
) -> Result<impl IntoResponse, ApiError> {
    require_scoped_collection(&state, &catalog_id, &collection_id).await?;
    validate_item_collection(&mut item, &collection_id, &state.base_url)?;
    // Atomic create — 409 on repost, no get-then-write race
    if !state
        .store
        .create_document(ITEMS_INDEX, &item_key(&item.id, &collection_id), &item)
        .await?
    {
        return Err(ApiError::Conflict(item.id));
    }
    Ok((
        StatusCode::CREATED,
        geo_json(item_doc(&state, &item, Some(&catalog_id))),
    ))
}

#[utoipa::path(
    put,
    path = "/catalogs/{catalog_id}/collections/{collection_id}/items/{item_id}",
    tag = "items",
    summary = "Replace an item in a scoped collection",
    params(("catalog_id" = String, Path, description = "Catalog id"), ("collection_id" = String, Path, description = "Collection id"), ("item_id" = String, Path, description = "Item id")),
    request_body = serde_json::Value,
    responses(
        (status = 200, description = "Successful response"),
        (status = 400, description = "Invalid request"),
        (status = 404, description = "Not found")
    )
)]
pub async fn update_scoped_item(
    Path((catalog_id, collection_id, item_id)): Path<(String, String, String)>,
    State(state): State<Arc<AppState>>,
    StacJson(mut item): StacJson<Item>,
) -> Result<impl IntoResponse, ApiError> {
    require_scoped_collection(&state, &catalog_id, &collection_id).await?;
    validate_item_collection(&mut item, &collection_id, &state.base_url)?;
    if !state
        .store
        .put_document(ITEMS_INDEX, &item_key(&item_id, &collection_id), &item)
        .await?
    {
        return Err(ApiError::WriteConflict(item_id));
    }
    Ok(geo_json(item_doc(&state, &item, Some(&catalog_id))))
}

#[utoipa::path(
    delete,
    path = "/catalogs/{catalog_id}/collections/{collection_id}/items/{item_id}",
    tag = "items",
    summary = "Delete an item in a scoped collection",
    params(("catalog_id" = String, Path, description = "Catalog id"), ("collection_id" = String, Path, description = "Collection id"), ("item_id" = String, Path, description = "Item id")),
    responses(
        (status = 200, description = "Successful response"),
        (status = 400, description = "Invalid request"),
        (status = 404, description = "Not found")
    )
)]
pub async fn delete_scoped_item(
    Path((catalog_id, collection_id, item_id)): Path<(String, String, String)>,
    State(state): State<Arc<AppState>>,
) -> Result<StatusCode, ApiError> {
    require_scoped_collection(&state, &catalog_id, &collection_id).await?;
    match state
        .store
        .get_document(ITEMS_INDEX, &item_key(&item_id, &collection_id))
        .await?
    {
        Some(doc) if doc["collection"] == collection_id => {
            state
                .store
                .delete_document(ITEMS_INDEX, &item_key(&item_id, &collection_id))
                .await?;
            Ok(StatusCode::NO_CONTENT)
        }
        _ => Err(ApiError::NotFound(item_id)),
    }
}

// --- Core Transaction Handlers (canonical /collections surface) ---

/// POST /collections — create a collection parented under root.
#[utoipa::path(
    post,
    path = "/collections",
    tag = "collections",
    summary = "Create a collection",
    request_body = serde_json::Value,
    responses(
        (status = 200, description = "Successful response"),
        (status = 400, description = "Invalid request"),
        (status = 404, description = "Not found")
    )
)]
pub async fn create_root_collection(
    State(state): State<Arc<AppState>>,
    StacJson(collection): StacJson<Collection>,
) -> Result<impl IntoResponse, ApiError> {
    if state
        .store
        .get_document(CATALOGS_INDEX, &collection.id)
        .await?
        .is_some()
        || state
            .store
            .get_document(COLLECTIONS_INDEX, &collection.id)
            .await?
            .is_some()
    {
        return Err(ApiError::Conflict(collection.id));
    }
    if !state
        .store
        .set_parents(
            &collection.id,
            vec![ROOT_CATALOG_ID.to_string()],
            NodeKind::Collection,
        )
        .await?
    {
        return Err(ApiError::WriteConflict(collection.id));
    }
    if !state
        .store
        .create_document(
            COLLECTIONS_INDEX,
            &collection.id,
            &stored_collection_doc(&state, &collection),
        )
        .await?
    {
        return Err(ApiError::Conflict(collection.id));
    }
    let (_, version) = state
        .store
        .get_document_versioned(COLLECTIONS_INDEX, &collection.id)
        .await?
        .ok_or_else(|| ApiError::Internal("collection vanished after create".into()))?;
    Ok(etag_response(
        StatusCode::CREATED,
        &etag_of(&state, COLLECTIONS_INDEX, &version),
        Json(global_collection_doc(&state, &collection.id).await?),
    ))
}

/// PUT /collections/{id} — update a collection; DAG memberships preserved.
#[utoipa::path(
    put,
    path = "/collections/{collection_id}",
    tag = "collections",
    summary = "Replace a collection",
    params(("collection_id" = String, Path, description = "Collection id"), ("If-Match" = Option<String>, Header, description = "Conditional write: apply only if the resource ETag matches")),
    request_body = serde_json::Value,
    responses(
        (status = 200, description = "Successful response"),
        (status = 400, description = "Invalid request"),
        (status = 404, description = "Not found"),
        (status = 412, description = "If-Match precondition failed"),
        (status = 428, description = "If-Match header required")
    )
)]
pub async fn update_root_collection(
    Path(collection_id): Path<String>,
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    StacJson(collection): StacJson<Collection>,
) -> Result<impl IntoResponse, ApiError> {
    if collection.id != collection_id {
        return Err(ApiError::BadRequest(format!(
            "body id '{}' does not match path '{collection_id}'",
            collection.id
        )));
    }
    let Some((_, version)) = state
        .store
        .get_document_versioned(COLLECTIONS_INDEX, &collection_id)
        .await?
        .filter(|t| is_collection_doc(&t.0))
    else {
        return Err(ApiError::NotFound(collection_id));
    };
    require_if_match(&headers, &etag_of(&state, COLLECTIONS_INDEX, &version))?;
    // With If-Match the write is conditioned on the version that
    // satisfied the check — check and write are one atomic step.
    let applied = if headers.contains_key(header::IF_MATCH) {
        state
            .store
            .index_document_versioned(
                COLLECTIONS_INDEX,
                &collection_id,
                &stored_collection_doc(&state, &collection),
                version,
            )
            .await?
    } else {
        state
            .store
            .replace_document(
                COLLECTIONS_INDEX,
                &collection_id,
                &stored_collection_doc(&state, &collection),
            )
            .await?
    };
    if !applied {
        return Err(if headers.contains_key(header::IF_MATCH) {
            ApiError::PreconditionFailed
        } else {
            ApiError::WriteConflict(collection_id)
        });
    }
    let (_, version) = state
        .store
        .get_document_versioned(COLLECTIONS_INDEX, &collection_id)
        .await?
        .ok_or_else(|| ApiError::Internal("collection vanished after write".into()))?;
    Ok(etag_response(
        StatusCode::OK,
        &etag_of(&state, COLLECTIONS_INDEX, &version),
        Json(global_collection_doc(&state, &collection.id).await?),
    ))
}

/// DELETE /collections/{id} — removes the collection, its DAG node, and
/// all of its items (canonical route semantics: a collection owns items).
#[utoipa::path(
    delete,
    path = "/collections/{collection_id}",
    tag = "collections",
    summary = "Delete a collection",
    params(("collection_id" = String, Path, description = "Collection id"), ("If-Match" = Option<String>, Header, description = "Conditional write: apply only if the resource ETag matches")),
    responses(
        (status = 200, description = "Successful response"),
        (status = 400, description = "Invalid request"),
        (status = 404, description = "Not found"),
        (status = 412, description = "If-Match precondition failed"),
        (status = 428, description = "If-Match header required")
    )
)]
pub async fn delete_root_collection(
    Path(collection_id): Path<String>,
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
) -> Result<StatusCode, ApiError> {
    let Some((_, version)) = state
        .store
        .get_document_versioned(COLLECTIONS_INDEX, &collection_id)
        .await?
        .filter(|t| is_collection_doc(&t.0))
    else {
        return Err(ApiError::NotFound(collection_id));
    };
    require_if_match(&headers, &etag_of(&state, COLLECTIONS_INDEX, &version))?;
    state
        .store
        .delete_items_by_collection(&collection_id)
        .await?;
    state.store.remove_node(&collection_id).await?;
    let deleted = if headers.contains_key(header::IF_MATCH) {
        state
            .store
            .delete_document_versioned(COLLECTIONS_INDEX, &collection_id, version)
            .await?
    } else {
        state
            .store
            .delete_document(COLLECTIONS_INDEX, &collection_id)
            .await?
    };
    if !deleted {
        return Err(if headers.contains_key(header::IF_MATCH) {
            ApiError::PreconditionFailed
        } else {
            ApiError::NotFound(collection_id)
        });
    }
    Ok(StatusCode::NO_CONTENT)
}

/// POST /collections/{id}/items — create an item in a collection.
#[utoipa::path(
    post,
    path = "/collections/{collection_id}/items",
    tag = "items",
    summary = "Create an item",
    params(("collection_id" = String, Path, description = "Collection id")),
    request_body = serde_json::Value,
    responses(
        (status = 200, description = "Successful response"),
        (status = 400, description = "Invalid request"),
        (status = 404, description = "Not found")
    )
)]
pub async fn create_root_item(
    Path(collection_id): Path<String>,
    State(state): State<Arc<AppState>>,
    StacJson(body): StacJson<serde_json::Value>,
) -> Result<axum::response::Response, ApiError> {
    if state
        .store
        .get_document(COLLECTIONS_INDEX, &collection_id)
        .await?
        .is_none()
    {
        return Err(ApiError::NotFound(collection_id));
    }
    if body.get("type").and_then(serde_json::Value::as_str) == Some("FeatureCollection") {
        return create_root_items_batch(&state, &collection_id, &body)
            .await
            .map(IntoResponse::into_response);
    }
    let mut item: Item = serde_json::from_value(body)
        .map_err(|e| ApiError::BadRequest(format!("invalid item: {e}")))?;
    validate_item_collection(&mut item, &collection_id, &state.base_url)?;
    if state
        .store
        .get_document(ITEMS_INDEX, &item_key(&item.id, &collection_id))
        .await?
        .is_some()
    {
        return Err(ApiError::Conflict(item.id));
    }
    if !state
        .store
        .create_document(ITEMS_INDEX, &item_key(&item.id, &collection_id), &item)
        .await?
    {
        return Err(ApiError::Conflict(item.id));
    }
    let (_, version) = state
        .store
        .get_document_versioned(ITEMS_INDEX, &item_key(&item.id, &collection_id))
        .await?
        .ok_or_else(|| ApiError::Internal("item vanished after create".into()))?;
    Ok(etag_response(
        StatusCode::CREATED,
        &etag_of(&state, ITEMS_INDEX, &version),
        geo_json(item_doc(&state, &item, None)),
    )
    .into_response())
}

/// FeatureCollection batch POST — all-or-nothing. Every feature's
/// collection identity is preflighted (URI-populated when absent) and any
/// mismatch or existing-id conflict rejects the whole batch before a
/// single write. In-batch duplicate ids collapse to one write (last wins).
async fn create_root_items_batch(
    state: &AppState,
    collection_id: &str,
    body: &serde_json::Value,
) -> Result<(StatusCode, impl IntoResponse), ApiError> {
    let features = body
        .get("features")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| ApiError::BadRequest("FeatureCollection missing features".into()))?;
    let mut order: Vec<String> = Vec::new();
    let mut by_id: std::collections::HashMap<String, Item> = std::collections::HashMap::new();
    for f in features {
        let mut item: Item = serde_json::from_value(f.clone())
            .map_err(|e| ApiError::BadRequest(format!("invalid feature: {e}")))?;
        validate_item_collection(&mut item, collection_id, &state.base_url)?;
        if !by_id.contains_key(&item.id) {
            order.push(item.id.clone());
        }
        by_id.insert(item.id.clone(), item);
    }
    for id in &order {
        if state
            .store
            .get_document(ITEMS_INDEX, &item_key(id, collection_id))
            .await?
            .is_some()
        {
            return Err(ApiError::Conflict(id.clone()));
        }
    }
    let mut docs = Vec::with_capacity(order.len());
    for id in &order {
        let item = &by_id[id];
        if !state
            .store
            .create_document(ITEMS_INDEX, &item_key(id, collection_id), item)
            .await?
        {
            return Err(ApiError::Conflict(id.clone()));
        }
        docs.push(item_doc(state, item, None));
    }
    Ok((
        StatusCode::CREATED,
        geo_json(serde_json::json!({"type": "FeatureCollection", "features": docs})),
    ))
}

/// PUT /collections/{id}/items/{item_id} — update an item.
#[utoipa::path(
    put,
    path = "/collections/{collection_id}/items/{item_id}",
    tag = "items",
    summary = "Replace an item",
    params(("collection_id" = String, Path, description = "Collection id"), ("item_id" = String, Path, description = "Item id"), ("If-Match" = Option<String>, Header, description = "Conditional write: apply only if the resource ETag matches")),
    request_body = serde_json::Value,
    responses(
        (status = 200, description = "Successful response"),
        (status = 400, description = "Invalid request"),
        (status = 404, description = "Not found"),
        (status = 412, description = "If-Match precondition failed"),
        (status = 428, description = "If-Match header required")
    )
)]
pub async fn update_root_item(
    Path((collection_id, item_id)): Path<(String, String)>,
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    StacJson(mut item): StacJson<Item>,
) -> Result<impl IntoResponse, ApiError> {
    if item.id != item_id {
        return Err(ApiError::BadRequest(format!(
            "body id '{}' does not match path '{item_id}'",
            item.id
        )));
    }
    if state
        .store
        .get_document(COLLECTIONS_INDEX, &collection_id)
        .await?
        .is_none()
    {
        return Err(ApiError::NotFound(collection_id));
    }
    match state
        .store
        .get_document_versioned(ITEMS_INDEX, &item_key(&item_id, &collection_id))
        .await?
    {
        Some((doc, version)) if doc["collection"] == collection_id => {
            validate_item_collection(&mut item, &collection_id, &state.base_url)?;
            require_if_match(&headers, &etag_of(&state, ITEMS_INDEX, &version))?;
            let key = item_key(&item_id, &collection_id);
            let applied = if headers.contains_key(header::IF_MATCH) {
                state
                    .store
                    .index_document_versioned(ITEMS_INDEX, &key, &item, version)
                    .await?
            } else {
                state
                    .store
                    .replace_document(ITEMS_INDEX, &key, &item)
                    .await?
            };
            if !applied {
                return Err(if headers.contains_key(header::IF_MATCH) {
                    ApiError::PreconditionFailed
                } else {
                    ApiError::WriteConflict(item_id)
                });
            }
            let (_, version) = state
                .store
                .get_document_versioned(ITEMS_INDEX, &item_key(&item_id, &collection_id))
                .await?
                .ok_or_else(|| ApiError::Internal("item vanished after write".into()))?;
            Ok(etag_response(
                StatusCode::OK,
                &etag_of(&state, ITEMS_INDEX, &version),
                geo_json(item_doc(&state, &item, None)),
            ))
        }
        _ => Err(ApiError::NotFound(item_id)),
    }
}

/// DELETE /collections/{id}/items/{item_id} — delete an item.
#[utoipa::path(
    delete,
    path = "/collections/{collection_id}/items/{item_id}",
    tag = "items",
    summary = "Delete an item",
    params(("collection_id" = String, Path, description = "Collection id"), ("item_id" = String, Path, description = "Item id"), ("If-Match" = Option<String>, Header, description = "Conditional write: apply only if the resource ETag matches")),
    responses(
        (status = 200, description = "Successful response"),
        (status = 400, description = "Invalid request"),
        (status = 404, description = "Not found"),
        (status = 412, description = "If-Match precondition failed"),
        (status = 428, description = "If-Match header required")
    )
)]
pub async fn delete_root_item(
    Path((collection_id, item_id)): Path<(String, String)>,
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
) -> Result<StatusCode, ApiError> {
    if state
        .store
        .get_document(COLLECTIONS_INDEX, &collection_id)
        .await?
        .is_none()
    {
        return Err(ApiError::NotFound(collection_id));
    }
    match state
        .store
        .get_document_versioned(ITEMS_INDEX, &item_key(&item_id, &collection_id))
        .await?
    {
        Some((doc, version)) if doc["collection"] == collection_id => {
            require_if_match(&headers, &etag_of(&state, ITEMS_INDEX, &version))?;
            let key = item_key(&item_id, &collection_id);
            let deleted = if headers.contains_key(header::IF_MATCH) {
                state
                    .store
                    .delete_document_versioned(ITEMS_INDEX, &key, version)
                    .await?
            } else {
                state.store.delete_document(ITEMS_INDEX, &key).await?
            };
            if !deleted {
                return Err(if headers.contains_key(header::IF_MATCH) {
                    ApiError::PreconditionFailed
                } else {
                    ApiError::NotFound(item_id)
                });
            }
            Ok(StatusCode::NO_CONTENT)
        }
        _ => Err(ApiError::NotFound(item_id)),
    }
}

/// Apply a request body as a patch. `application/json-patch+json` + an
/// array body is RFC 6902 JSON-Patch; an object body under
/// `application/merge-patch+json` or `application/json` is RFC 7386
/// JSON Merge Patch (same split as upstream).
fn apply_patch(
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
fn patch_addresses_field(patch: &serde_json::Value, field: &str) -> bool {
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
fn merge_patch(target: &mut serde_json::Value, patch: &serde_json::Value) {
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

/// PATCH /collections/{id} — JSON Merge Patch or JSON-Patch on the
/// collection document; `id` is immutable (400 if a patch changes it).
#[utoipa::path(
    patch,
    path = "/collections/{collection_id}",
    tag = "collections",
    summary = "Patch a collection (JSON Patch / Merge Patch)",
    params(("collection_id" = String, Path, description = "Collection id"), ("If-Match" = Option<String>, Header, description = "Conditional write: apply only if the resource ETag matches")),
    request_body = serde_json::Value,
    responses(
        (status = 200, description = "Successful response"),
        (status = 400, description = "Invalid request"),
        (status = 404, description = "Not found"),
        (status = 412, description = "If-Match precondition failed"),
        (status = 428, description = "If-Match header required")
    )
)]
pub async fn patch_root_collection(
    Path(collection_id): Path<String>,
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    StacJson(patch): StacJson<serde_json::Value>,
) -> Result<impl IntoResponse, ApiError> {
    // Request-level rejection — upstream rejects before the doc lookup:
    // memberships only change through the catalog link/unlink routes.
    if patch_addresses_field(&patch, "parent_ids") {
        return Err(ApiError::BadRequest(
            "A patch may not address parent_ids; use /catalogs/{catalog_id}/collections."
                .to_string(),
        ));
    }
    let (mut doc, version) = state
        .store
        .get_document_versioned(COLLECTIONS_INDEX, &collection_id)
        .await?
        .filter(|t| is_collection_doc(&t.0))
        .ok_or_else(|| ApiError::NotFound(collection_id.clone()))?;
    apply_patch(
        &mut doc,
        headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default(),
        &patch,
    )?;
    if doc["id"].as_str() != Some(collection_id.as_str()) || !is_collection_doc(&doc) {
        return Err(ApiError::BadRequest(
            "a patch may not change the collection id or type".to_string(),
        ));
    }
    // `parent_ids` is DAG-managed — a patch may not rewrite edges.
    if let Some(obj) = doc.as_object_mut() {
        obj.remove("parent_ids");
    }
    require_if_match(&headers, &etag_of(&state, COLLECTIONS_INDEX, &version))?;
    if state.collections_search_enabled() {
        if let Some(obj) = doc.as_object_mut() {
            collection_search_fields(obj);
        }
    }
    let applied = if headers.contains_key(header::IF_MATCH) {
        state
            .store
            .index_document_versioned(COLLECTIONS_INDEX, &collection_id, &doc, version)
            .await?
    } else {
        state
            .store
            .replace_document(COLLECTIONS_INDEX, &collection_id, &doc)
            .await?
    };
    if !applied {
        return Err(if headers.contains_key(header::IF_MATCH) {
            ApiError::PreconditionFailed
        } else {
            ApiError::WriteConflict(collection_id)
        });
    }
    let (_, version) = state
        .store
        .get_document_versioned(COLLECTIONS_INDEX, &collection_id)
        .await?
        .ok_or_else(|| ApiError::Internal("collection vanished after write".into()))?;
    Ok(etag_response(
        StatusCode::OK,
        &etag_of(&state, COLLECTIONS_INDEX, &version),
        Json(global_collection_doc(&state, &collection_id).await?),
    ))
}

/// PATCH /collections/{id}/items/{item_id} — JSON Merge Patch or
/// JSON-Patch on the item document; `id` and `collection` are immutable
/// (400 if a patch changes them).
#[utoipa::path(
    patch,
    path = "/collections/{collection_id}/items/{item_id}",
    tag = "items",
    summary = "Patch an item (JSON Patch / Merge Patch)",
    params(("collection_id" = String, Path, description = "Collection id"), ("item_id" = String, Path, description = "Item id"), ("If-Match" = Option<String>, Header, description = "Conditional write: apply only if the resource ETag matches")),
    request_body = serde_json::Value,
    responses(
        (status = 200, description = "Successful response"),
        (status = 400, description = "Invalid request"),
        (status = 404, description = "Not found"),
        (status = 412, description = "If-Match precondition failed"),
        (status = 428, description = "If-Match header required")
    )
)]
pub async fn patch_root_item(
    Path((collection_id, item_id)): Path<(String, String)>,
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    StacJson(patch): StacJson<serde_json::Value>,
) -> Result<impl IntoResponse, ApiError> {
    if state
        .store
        .get_document(COLLECTIONS_INDEX, &collection_id)
        .await?
        .is_none()
    {
        return Err(ApiError::NotFound(collection_id));
    }
    match state
        .store
        .get_document_versioned(ITEMS_INDEX, &item_key(&item_id, &collection_id))
        .await?
    {
        Some((mut doc, version)) if doc["collection"] == collection_id => {
            apply_patch(
                &mut doc,
                headers
                    .get(header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default(),
                &patch,
            )?;
            if doc["id"].as_str() != Some(item_id.as_str())
                || doc["collection"].as_str() != Some(collection_id.as_str())
            {
                return Err(ApiError::BadRequest(
                    "a patch may not change the item id or collection".to_string(),
                ));
            }
            require_if_match(&headers, &etag_of(&state, ITEMS_INDEX, &version))?;
            let key = item_key(&item_id, &collection_id);
            let applied = if headers.contains_key(header::IF_MATCH) {
                state
                    .store
                    .index_document_versioned(ITEMS_INDEX, &key, &doc, version)
                    .await?
            } else {
                state
                    .store
                    .replace_document(ITEMS_INDEX, &key, &doc)
                    .await?
            };
            if !applied {
                return Err(if headers.contains_key(header::IF_MATCH) {
                    ApiError::PreconditionFailed
                } else {
                    ApiError::WriteConflict(item_id)
                });
            }
            merge_doc_links(
                &mut doc,
                item_links(&state, &item_id, &collection_id, None),
                &state.base_url,
            );
            let (_, version) = state
                .store
                .get_document_versioned(ITEMS_INDEX, &item_key(&item_id, &collection_id))
                .await?
                .ok_or_else(|| ApiError::Internal("item vanished after write".into()))?;
            Ok(etag_response(
                StatusCode::OK,
                &etag_of(&state, ITEMS_INDEX, &version),
                geo_json(doc),
            ))
        }
        _ => Err(ApiError::NotFound(item_id)),
    }
}

/// POST /collections/{id}/bulk_items — bulk transactions extension:
/// `{"items": {"id": {item}}, "method": "insert"|"upsert"}` →
/// `{received, success, skipped, errors[]}`. Per-item processing:
/// collection populated from the path when absent, per-item error on
/// mismatch/malformed/conflict — never all-or-nothing.
#[utoipa::path(
    post,
    path = "/collections/{collection_id}/bulk_items",
    tag = "items",
    summary = "Bulk item insert (Bulk Transaction extension)",
    params(("collection_id" = String, Path, description = "Collection id")),
    request_body = serde_json::Value,
    responses(
        (status = 200, description = "Successful response"),
        (status = 400, description = "Invalid request"),
        (status = 404, description = "Not found"),
        (status = 409, description = "Duplicate item id")
    )
)]
pub async fn bulk_create_items(
    Path(collection_id): Path<String>,
    State(state): State<Arc<AppState>>,
    StacJson(body): StacJson<serde_json::Value>,
) -> Result<impl IntoResponse, ApiError> {
    if state
        .store
        .get_document(COLLECTIONS_INDEX, &collection_id)
        .await?
        .filter(is_collection_doc)
        .is_none()
    {
        return Err(ApiError::NotFound(collection_id));
    }
    let upsert = body.get("method").and_then(serde_json::Value::as_str) == Some("upsert");
    if upsert && std::env::var("REQUIRE_IF_MATCH").is_ok_and(|v| v == "true") {
        return Err(ApiError::PreconditionRequired);
    }
    let items = body
        .get("items")
        .and_then(serde_json::Value::as_object)
        .cloned()
        .ok_or_else(|| ApiError::BadRequest("body missing 'items' object".into()))?;
    let raise_on_error = std::env::var("RAISE_ON_BULK_ERROR").is_ok_and(|v| v == "true");
    let received = items.len();
    let (mut success, mut skipped) = (0usize, 0usize);
    let mut errors: Vec<serde_json::Value> = Vec::new();
    // Admission on raw entries, keyed by map key, matching upstream's
    // bulk_item_insert: the path collection is authoritative — absent,
    // null or "" `collection` is rewritten to it; any other value errors.
    let mut admitted: Vec<serde_json::Value> = Vec::new();
    for (key, raw) in &items {
        if !raw.is_object() {
            errors.push(json!({"id": key, "msg": "Item must be a JSON object."}));
            continue;
        }
        match raw.get("id").and_then(serde_json::Value::as_str) {
            Some(s) if !s.is_empty() => {}
            _ => {
                errors.push(json!({"id": key, "msg": "Item must have a non-empty string id."}));
                continue;
            }
        }
        match raw.get("collection") {
            None => {
                let mut o = raw.clone();
                o["collection"] = json!(collection_id);
                admitted.push(o);
            }
            Some(v)
                if v.is_null()
                    || v.as_str() == Some("")
                    || v.as_str() == Some(collection_id.as_str()) =>
            {
                let mut o = raw.clone();
                o["collection"] = json!(collection_id);
                admitted.push(o);
            }
            Some(v) => {
                let shown = v
                    .as_str()
                    .map(|s| format!("'{s}'"))
                    .unwrap_or_else(|| v.to_string());
                errors.push(json!({"id": key, "msg": format!("Item collection {shown} does not match path collection '{collection_id}'")}));
            }
        }
    }
    if raise_on_error && !errors.is_empty() {
        return Err(ApiError::BadRequest(
            json!({
                "message": format!("Bulk insertion rejected. {} items are malformed.", errors.len()),
                "errors": errors,
            })
            .to_string(),
        ));
    }
    // Dedupe by item id, last occurrence wins (upstream dict semantics),
    // preserving first-occurrence order of the ids.
    let mut last: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (i, raw) in admitted.iter().enumerate() {
        last.insert(raw["id"].as_str().unwrap().to_string(), i);
    }
    let mut unique: Vec<&serde_json::Value> = Vec::new();
    let mut emitted = std::collections::HashSet::new();
    for raw in admitted.iter() {
        let id = raw["id"].as_str().unwrap();
        if emitted.insert(id.to_string()) {
            unique.push(&admitted[last[id]]);
        }
    }
    skipped += admitted.len() - unique.len();
    for raw in unique {
        let mut item: Item = match serde_json::from_value(raw.clone()) {
            Ok(i) => i,
            Err(e) => {
                errors.push(json!({"id": raw["id"], "msg": format!("invalid item: {e}")}));
                continue;
            }
        };
        if let Err(e) = validate_item_collection(&mut item, &collection_id, &state.base_url) {
            let msg = match e {
                ApiError::BadRequest(m) => m,
                _ => "invalid collection identity".to_string(),
            };
            errors.push(json!({"id": item.id, "msg": msg}));
            continue;
        }
        let key = item_key(&item.id, &collection_id);
        let applied = if upsert {
            state.store.put_document(ITEMS_INDEX, &key, &item).await?
        } else {
            state
                .store
                .create_document(ITEMS_INDEX, &key, &item)
                .await?
        };
        if applied {
            success += 1;
        } else {
            skipped += 1;
            if raise_on_error {
                return Err(ApiError::Conflict(format!(
                    "Item '{}' already exists in collection '{collection_id}'",
                    item.id
                )));
            }
            errors.push(json!({"id": item.id, "msg": "item already exists or write conflict"}));
        }
    }
    Ok(Json(json!({
        "received": received,
        "success": success,
        "skipped": skipped,
        "errors": errors,
    })))
}

// --- Error Handling ---

pub enum ApiError {
    BadRequest(String),
    /// Requested resources sit outside the scoped catalog (403).
    Forbidden(String),
    NotFound(String),
    Conflict(String),
    /// `If-Match` precondition failed.
    PreconditionFailed,
    /// `If-Match` header required but absent (428).
    PreconditionRequired,
    /// Optimistic-concurrency retries exhausted — retry the request.
    WriteConflict(String),
    Internal(String),
}

impl From<opensearch::Error> for ApiError {
    fn from(e: opensearch::Error) -> Self {
        ApiError::Internal(e.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, msg) = match self {
            ApiError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg),
            ApiError::Forbidden(msg) => (StatusCode::FORBIDDEN, msg),
            ApiError::NotFound(id) => (StatusCode::NOT_FOUND, format!("Resource '{id}' not found")),
            ApiError::Conflict(id) => (
                StatusCode::CONFLICT,
                format!("Resource '{id}' already exists"),
            ),
            ApiError::PreconditionFailed => (
                StatusCode::PRECONDITION_FAILED,
                "If-Match precondition failed".to_string(),
            ),
            ApiError::PreconditionRequired => (
                StatusCode::PRECONDITION_REQUIRED,
                "If-Match header is required".to_string(),
            ),
            ApiError::WriteConflict(id) => (
                StatusCode::CONFLICT,
                format!("Resource '{id}' was modified concurrently — retry the request"),
            ),
            ApiError::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg),
        };
        (status, Json(json!({ "detail": msg }))).into_response()
    }
}
