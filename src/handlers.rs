// src/handlers.rs
use crate::dto::CreateOrLinkPayload;
use crate::links::LinkEngine;
use crate::store::{
    search_offset, NodeKind, Store, CATALOGS_INDEX, COLLECTIONS_INDEX, DEFAULT_SEARCH_LIMIT,
    ITEMS_INDEX, MAX_SEARCH_LIMIT, ROOT_CATALOG_ID,
};
use axum::{
    extract::{Path, Query, State},
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

/// Default page size for catalog/collection/children list endpoints.
const DEFAULT_LIST_LIMIT: u64 = 10;

/// STAC requires `application/geo+json` on Item and ItemCollection payloads.
fn geo_json<T: serde::Serialize>(body: T) -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "application/geo+json")], Json(body))
}

#[derive(Deserialize, Default)]
pub struct ChildrenQuery {
    pub r#type: Option<String>, // Filter by "Catalog" or "Collection"
    pub limit: Option<u64>,
    /// Opaque pagination cursor (offset-encoded).
    pub token: Option<String>,
}

#[derive(Deserialize, Default)]
pub struct ListQuery {
    pub limit: Option<u64>,
    pub token: Option<String>,
    /// Sort extension (features binding): `?sortby=+field,-field`.
    pub sortby: Option<String>,
    /// Fields extension (features binding): `?fields=+include,-exclude`.
    pub fields: Option<String>,
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

/// Resolve `?limit=&token=` into `(limit, offset)` — absent limit falls
/// back to the endpoint's configured default, then the configured max
/// caps it (`None` = uncapped). `limit=0` is a 400; an unparseable token
/// restarts from 0 (matching upstream leniency).
fn list_window(
    q: &ListQuery,
    default_limit: u64,
    max_limit: Option<u64>,
) -> Result<(u64, u64), ApiError> {
    let limit = q
        .limit
        .unwrap_or(default_limit)
        .min(max_limit.unwrap_or(u64::MAX));
    if limit == 0 {
        return Err(ApiError::BadRequest("limit must be >= 1".to_string()));
    }
    let offset = q
        .token
        .as_deref()
        .and_then(|t| t.parse::<u64>().ok())
        .unwrap_or(0);
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
        links.push(link_json(base.clone(), "parent"));
    } else {
        links.push(link_json(format!("{base}/catalogs/{}", real[0]), "parent"));
        if !state.hide_alternate_parents {
            for p in &real[1..] {
                links.push(
                    link_json(format!("{base}/catalogs/{p}"), "related")
                        .title(format!("Alternate parent: {p}")),
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

/// `rel=child` links for a catalog's direct children. Catalogs get
/// canonical `/catalogs/{id}` hrefs, collections get scoped
/// `/catalogs/{cat}/collections/{id}` hrefs. Nodes without a backing
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
            NodeKind::Catalog => format!("{base}/catalogs/{}", node.id),
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
    state.links.format_scoped_collection_links(
        collection_id,
        scoped_catalog_id,
        parents,
        state.hide_alternate_parents,
    )
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
    let mut real: Vec<&str> = parents
        .iter()
        .map(String::as_str)
        .filter(|p| *p != ROOT_CATALOG_ID)
        .collect();
    real.sort_unstable();
    real.dedup();
    if real.is_empty() {
        links.push(link_json(base.clone(), "parent"));
        return links;
    }
    let shown: &[&str] = if state.hide_alternate_parents {
        &real[..1]
    } else {
        &real
    };
    for p in shown {
        links.push(link_json(format!("{base}/catalogs/{p}"), "parent"));
    }
    if !state.hide_alternate_parents {
        for p in &real {
            let mut related = link_json(format!("{base}/catalogs/{p}"), "related");
            related.title = Some(format!("Alternate parent: {p}"));
            links.push(related);
            links.push(link_json(
                format!("{base}/catalogs/{p}/collections/{collection_id}"),
                "duplicate",
            ));
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
    let parents = state.store.get_parents(collection_id).await?;
    doc["links"] = json!(global_collection_links(state, collection_id, &parents));
    Ok(doc)
}

/// Serialize a catalog with its DAG-derived links injected — write
/// responses mirror what a subsequent GET would return.
async fn catalog_doc(state: &AppState, catalog: &Catalog) -> Result<serde_json::Value, ApiError> {
    let parents = state.store.get_parents(&catalog.id).await?;
    let mut doc = serde_json::to_value(catalog).map_err(|e| ApiError::Internal(e.to_string()))?;
    doc["links"] = json!(catalog_links(state, &catalog.id, &parents).await?);
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
fn require_if_match(headers: &axum::http::HeaderMap, current_etag: &str) -> Result<(), ApiError> {
    match headers.get(header::IF_MATCH) {
        Some(v) if !if_match_holds(v.to_str().unwrap_or_default(), current_etag) => {
            Err(ApiError::PreconditionFailed)
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
            axum::http::HeaderValue::from_str(etag).unwrap_or_else(|_| {
                axum::http::HeaderValue::from_static("\"invalid\"")
            }),
        )],
        body,
    )
}

fn item_doc(state: &AppState, item: &Item, scoped_catalog_id: Option<&str>) -> serde_json::Value {
    let mut doc = serde_json::to_value(item).unwrap_or_default();
    if let Some(col) = item.collection.as_deref() {
        doc["links"] = json!(item_links(state, &item.id, col, scoped_catalog_id));
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
    doc["links"] = json!(collection_links(
        state,
        &collection.id,
        scoped_catalog_id,
        &parents
    ));
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
pub async fn conformance(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(json!({ "conformsTo": conformance_classes(&state) }))
}

/// Core queryables present on every item (upstream DEFAULT_QUERYABLES —
/// `$ref`s into the STAC item-spec schema).
fn default_queryables() -> serde_json::Map<String, serde_json::Value> {
    serde_json::Map::from_iter([
        ("id", json!({"description": "ID", "$ref": "https://schemas.stacspec.org/v1.0.0/item-spec/json-schema/item.json#/definitions/core/allOf/2/properties/id"})),
        ("collection", json!({"description": "Collection", "$ref": "https://schemas.stacspec.org/v1.0.0/item-spec/json-schema/item.json#/definitions/core/allOf/2/then/properties/collection"})),
        ("geometry", json!({"description": "Geometry", "$ref": "https://schemas.stacspec.org/v1.0.0/item-spec/json-schema/item.json#/definitions/core/allOf/1/oneOf/0/properties/geometry"})),
        ("datetime", json!({"description": "Acquisition Timestamp", "$ref": "https://schemas.stacspec.org/v1.0.0/item-spec/json-schema/datetime.json#/properties/datetime"})),
        ("created", json!({"description": "Creation Timestamp", "$ref": "https://schemas.stacspec.org/v1.0.0/item-spec/json-schema/datetime.json#/properties/created"})),
        ("updated", json!({"description": "Update Timestamp", "$ref": "https://schemas.stacspec.org/v1.0.0/item-spec/json-schema/datetime.json#/properties/updated"})),
        ("title", json!({"type": "string", "description": "Item title"})),
        ("description", json!({"type": "string", "description": "Item description"})),
        ("keywords", json!({"type": "array", "items": {"type": "string"}, "description": "Item keywords"})),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect::<serde_json::Map<String, serde_json::Value>>())
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

/// GET /queryables — union of queryables across all items.
pub async fn get_queryables(
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, ApiError> {
    let mapping = state.store.get_index_mapping(ITEMS_INDEX).await?;
    let index_mapping = mapping
        .as_object()
        .and_then(|o| o.values().next())
        .and_then(|v| v.get("mappings"))
        .cloned()
        .unwrap_or_else(|| json!({}));
    let dynamic = crate::store::queryable_properties(&index_mapping, &state.excluded_queryables);
    Ok(json_schema(queryables_schema(
        &state,
        "/queryables".to_string(),
        "Queryables for stac-os-rs",
        dynamic,
    )))
}

/// GET /collections/{collection_id}/queryables — same union scoped to a
/// collection (our shared items index means the advertised set is global;
/// upstream per-collection indices would narrow it).
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
    let mapping = state.store.get_index_mapping(ITEMS_INDEX).await?;
    let index_mapping = mapping
        .as_object()
        .and_then(|o| o.values().next())
        .and_then(|v| v.get("mappings"))
        .cloned()
        .unwrap_or_else(|| json!({}));
    let dynamic = crate::store::queryable_properties(&index_mapping, &state.excluded_queryables);
    Ok(json_schema(queryables_schema(
        &state,
        format!("/collections/{collection_id}/queryables"),
        "Queryables for stac-os-rs",
        dynamic,
    )))
}

/// GET /api — OpenAPI service description (hand-maintained).
pub async fn api() -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            "application/vnd.oai.openapi+json;version=3.0",
        )],
        include_str!("openapi.json"),
    )
}

pub async fn root_landing_page(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let base = &state.base_url;
    let mut links = json!([
        { "rel": "self", "type": "application/json", "href": format!("{base}/") },
        { "rel": "root", "type": "application/json", "href": format!("{base}/") },
        { "rel": "conformance", "type": "application/json", "href": format!("{base}/conformance") },
        { "rel": "service-desc", "type": "application/vnd.oai.openapi+json;version=3.0", "href": format!("{base}/api") },
        { "rel": "data", "type": "application/json", "href": format!("{base}/collections") },
        { "rel": "search", "type": "application/geo+json", "href": format!("{base}/search") },
        { "rel": "http://www.opengis.net/def/rel/ogc/1.0/sortables", "type": "application/schema+json", "href": format!("{base}/sortables"), "title": "Sortables" },
        { "rel": "http://www.opengis.net/def/rel/ogc/1.0/queryables", "type": "application/schema+json", "href": format!("{base}/queryables"), "title": "Queryables" },
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

pub async fn list_catalogs(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let (limit, offset) = list_window(&query, DEFAULT_LIST_LIMIT, None)?;
    let ids = state
        .store
        .get_children_by_kind(ROOT_CATALOG_ID, NodeKind::Catalog)
        .await?;
    let matched = ids.len();
    let page: Vec<String> = ids
        .into_iter()
        .skip(offset as usize)
        .take(limit as usize)
        .collect();
    let parents_map = state.store.get_parents_many(&page).await?;
    let mut catalogs = state.store.get_documents(CATALOGS_INDEX, &page).await?;
    for doc in catalogs.iter_mut() {
        if let Some(id) = doc["id"].as_str() {
            let parents = parents_map.get(id).cloned().unwrap_or_default();
            doc["links"] = json!(catalog_links(&state, id, &parents).await?);
        }
    }
    let mut links = json!([
        { "rel": "self", "href": format!("{}/catalogs", state.base_url) },
        { "rel": "root", "href": state.base_url },
        { "rel": "parent", "href": state.base_url }
    ]);
    let returned = catalogs.len();
    if let Some(next) = list_next_link(&state, "/catalogs", limit, offset, returned, matched, None)
    {
        links.as_array_mut().unwrap().push(next);
    }
    Ok(Json(json!({
        "catalogs": catalogs,
        "numberReturned": returned,
        "numberMatched": matched,
        "links": links
    })))
}

pub async fn create_root_catalog(
    State(state): State<Arc<AppState>>,
    StacJson(catalog): StacJson<Catalog>,
) -> Result<impl IntoResponse, ApiError> {
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
    Ok((
        StatusCode::CREATED,
        Json(catalog_doc(&state, &catalog).await?),
    ))
}

pub async fn get_catalog(
    Path(catalog_id): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, ApiError> {
    match state
        .store
        .get_document(CATALOGS_INDEX, &catalog_id)
        .await?
    {
        Some(mut doc) => {
            let parents = state.store.get_parents(&catalog_id).await?;
            doc["links"] = json!(catalog_links(&state, &catalog_id, &parents).await?);
            Ok(Json(doc))
        }
        None => Err(ApiError::NotFound(catalog_id)),
    }
}

pub async fn update_catalog(
    Path(catalog_id): Path<String>,
    State(state): State<Arc<AppState>>,
    StacJson(catalog): StacJson<Catalog>,
) -> Result<impl IntoResponse, ApiError> {
    require_catalog(&state, &catalog_id).await?;
    if catalog.id != catalog_id {
        return Err(ApiError::BadRequest(format!(
            "body id '{}' does not match path id '{catalog_id}'",
            catalog.id
        )));
    }
    if !state
        .store
        .replace_document(CATALOGS_INDEX, &catalog_id, &catalog)
        .await?
    {
        return Err(ApiError::WriteConflict(catalog_id));
    }
    Ok(Json(catalog_doc(&state, &catalog).await?))
}

pub async fn get_catalog_children(
    Path(catalog_id): Path<String>,
    Query(query): Query<ChildrenQuery>,
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, ApiError> {
    require_catalog(&state, &catalog_id).await?;
    let (limit, offset) = list_window(
        &ListQuery {
            limit: query.limit,
            token: query.token.clone(),
            ..Default::default()
        },
        DEFAULT_LIST_LIMIT,
        None,
    )?;
    let kind = parse_kind(query.r#type.as_deref());
    let nodes = state.store.get_child_nodes(&catalog_id, kind).await?;
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
        doc["links"] = match node.kind {
            // parent link locked to this listing's catalog context
            NodeKind::Catalog => json!(catalog_links(&state, &node.id, &node.parents).await?),
            NodeKind::Collection => json!(collection_links(
                &state,
                &node.id,
                &catalog_id,
                &node.parents
            )),
        };
        children.push(doc);
    }

    let mut links = json!([
        { "rel": "self", "href": format!("{}/catalogs/{catalog_id}/children", state.base_url) },
        { "rel": "root", "href": state.base_url },
        { "rel": "parent", "href": format!("{}/catalogs/{catalog_id}", state.base_url) }
    ]);
    let type_param = query.r#type.as_deref().map(|t| format!("&type={t}"));
    if let Some(next) = list_next_link(
        &state,
        &format!("/catalogs/{catalog_id}/children"),
        limit,
        offset,
        children.len(),
        matched,
        type_param.as_deref(),
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
pub async fn list_sub_catalogs(
    Path(catalog_id): Path<String>,
    Query(query): Query<ListQuery>,
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, ApiError> {
    require_catalog(&state, &catalog_id).await?;
    let (limit, offset) = list_window(&query, DEFAULT_LIST_LIMIT, None)?;
    let ids = state
        .store
        .get_children_by_kind(&catalog_id, NodeKind::Catalog)
        .await?;
    let matched = ids.len();
    let page: Vec<String> = ids
        .into_iter()
        .skip(offset as usize)
        .take(limit as usize)
        .collect();
    let parents_map = state.store.get_parents_many(&page).await?;
    let mut catalogs = state.store.get_documents(CATALOGS_INDEX, &page).await?;
    for doc in catalogs.iter_mut() {
        if let Some(id) = doc["id"].as_str() {
            let parents = parents_map.get(id).cloned().unwrap_or_default();
            doc["links"] = json!(catalog_links(&state, id, &parents).await?);
        }
    }
    let mut links = json!([
        { "rel": "self", "href": format!("{}/catalogs/{catalog_id}/catalogs", state.base_url) },
        { "rel": "root", "href": state.base_url },
        { "rel": "parent", "href": format!("{}/catalogs/{catalog_id}", state.base_url) }
    ]);
    let returned = catalogs.len();
    if let Some(next) = list_next_link(
        &state,
        &format!("/catalogs/{catalog_id}/catalogs"),
        limit,
        offset,
        returned,
        matched,
        None,
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

/// Shared OGC Sortables schema — `additionalProperties: true` matches our
/// permissive sort (unknown fields evaluate to null, sorting last).
fn sortables_schema(base: &str, id_path: &str, title: &str) -> serde_json::Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": format!("{base}{id_path}"),
        "type": "object",
        "title": title,
        "description": "Fields usable in the `sortby` parameter.",
        "properties": {
            "id": {"type": "string"},
            "collection": {"type": "string"},
            "datetime": {"type": "string", "format": "date-time"},
            "properties.datetime": {"type": "string", "format": "date-time"}
        },
        "additionalProperties": true
    })
}

/// GET /sortables — Sortables schema for item search (item-search binding).
pub async fn sortables(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(sortables_schema(
        &state.base_url,
        "/sortables",
        "Item Search Sortables",
    ))
}

/// GET /collections/{id}/sortables — features-binding sortables schema.
pub async fn collection_sortables(
    Path(collection_id): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if state
        .store
        .get_document(COLLECTIONS_INDEX, &collection_id)
        .await?
        .is_none()
    {
        return Err(ApiError::NotFound(collection_id));
    }
    Ok(Json(sortables_schema(
        &state.base_url,
        &format!("/collections/{collection_id}/sortables"),
        "Collection Items Sortables",
    )))
}

/// GET /catalogs/{id}/conformance — catalog-scoped conformance classes.
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
) -> Result<impl IntoResponse, ApiError> {
    let empty = || {
        let mut c =
            ItemCollection::new(Vec::new()).map_err(|e| ApiError::Internal(e.to_string()))?;
        c.number_matched = Some(0);
        c.number_returned = Some(0);
        Ok::<_, ApiError>(c)
    };

    // Pagination token → numeric offset (rejected before any lookup).
    if let Some(t) = search.items.additional_fields.get("token").cloned() {
        if !t.is_null() {
            let offset = match &t {
                serde_json::Value::String(s) => decode_item_token(s)?,
                serde_json::Value::Number(n) => n.as_u64().ok_or_else(|| {
                    ApiError::BadRequest("Invalid pagination token.".to_string())
                })?,
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
    if !search.collections.is_empty() {
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

    // A scope with no collections (or a root request filtered to
    // nothing) short-circuits — after parsing, so malformed extension
    // params still surface their 400 (#935 semantics).
    if search.collections.is_empty() {
        return Ok(geo_json(empty()?));
    }

    // 3. Query OpenSearch — collections scope + bbox/datetime/intersects/ids
    // + the `query` (STACQL) and `filter` (CQL2) extensions, translated
    // to extra filter clauses (malformed expressions are a 400).
    let mut extra_filters = Vec::new();
    if let Some(query) = &search.items.query {
        extra_filters.extend(
            crate::store::stacql_clauses(query)
                .map_err(|e| ApiError::BadRequest(format!("Invalid query parameter: {e}")))?,
        );
    }
    if let Some(filter) = &search.items.filter {
        extra_filters.push(
            crate::store::cql2_clause(filter)
                .map_err(|e| ApiError::BadRequest(format!("Invalid filter parameter: {e}.")))?,
        );
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
            item.insert(
                "links".to_string(),
                json!(item_links(state, &item_id, &col, link_scope)),
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
    collection.links.push(link_json(format!("{base}/"), "root"));

    // 4. Offset pagination links (STAC pagination: method+body on the link).
    let limit = search
        .items
        .limit
        .unwrap_or(DEFAULT_SEARCH_LIMIT)
        .min(MAX_SEARCH_LIMIT);
    let offset = search_offset(&search);
    let href = if scope_id == ROOT_CATALOG_ID {
        format!("{}/catalogs/search", state.base_url)
    } else {
        format!("{}/catalogs/{scope_id}/search", state.base_url)
    };
    collection.links.push(link_json(href.clone(), "self"));
    if offset + returned < matched {
        collection
            .links
            .push(page_link("next", &href, &search, offset + limit));
    }
    if offset > 0 {
        collection.links.push(page_link(
            "prev",
            &href,
            &search,
            offset.saturating_sub(limit),
        ));
    }
    Ok(geo_json(collection))
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
        serde_json::from_value::<Search>(value)
            .map(Self)
            .map_err(D::Error::custom)
    }
}

pub async fn scoped_search_post(
    Path(catalog_id): Path<String>,
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
    StacJson(search): StacJson<SearchBody>,
) -> Result<impl IntoResponse, ApiError> {
    let mut search = search.0;
    apply_query_token(&mut search, params.get("token"));
    run_scoped_search(&state, &catalog_id, search).await
}

pub async fn scoped_search_get(
    Path(catalog_id): Path<String>,
    State(state): State<Arc<AppState>>,
    Query(params): Query<GetSearch>,
) -> Result<impl IntoResponse, ApiError> {
    let search = get_to_search(params)?;
    run_scoped_search(&state, &catalog_id, search).await
}

/// Search across the whole catalogs registry — equivalent to scoping to
/// `root`, since every orphan is adopted there.
pub async fn catalogs_search_post(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
    StacJson(search): StacJson<SearchBody>,
) -> Result<impl IntoResponse, ApiError> {
    let mut search = search.0;
    apply_query_token(&mut search, params.get("token"));
    run_scoped_search(&state, ROOT_CATALOG_ID, search).await
}

pub async fn catalogs_search_get(
    State(state): State<Arc<AppState>>,
    Query(params): Query<GetSearch>,
) -> Result<impl IntoResponse, ApiError> {
    let search = get_to_search(params)?;
    run_scoped_search(&state, ROOT_CATALOG_ID, search).await
}

// --- Transaction Handlers ---

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
            let mut doc = state
                .store
                .get_document(CATALOGS_INDEX, &id)
                .await?
                .unwrap();
            let parents = state.store.get_parents(&id).await?;
            doc["links"] = json!(catalog_links(&state, &id, &parents).await?);
            Ok((StatusCode::OK, Json(doc)))
        }
        CreateOrLinkPayload::FullResource(new_catalog) => {
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
            Ok((
                StatusCode::CREATED,
                Json(catalog_doc(&state, &new_catalog).await?),
            ))
        }
    }
}

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

pub async fn list_scoped_collections(
    Path(catalog_id): Path<String>,
    Query(query): Query<ListQuery>,
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, ApiError> {
    require_catalog(&state, &catalog_id).await?;
    let (limit, offset) = list_window(
        &query,
        state.default_collection_limit,
        state.max_collection_limit,
    )?;
    let ids = state
        .store
        .get_children_by_kind(&catalog_id, NodeKind::Collection)
        .await?;
    let matched = ids.len();
    let page: Vec<String> = ids
        .into_iter()
        .skip(offset as usize)
        .take(limit as usize)
        .collect();
    let parents_map = state.store.get_parents_many(&page).await?;
    let mut collections = state.store.get_documents(COLLECTIONS_INDEX, &page).await?;
    for doc in collections.iter_mut() {
        if let Some(id) = doc["id"].as_str() {
            let parents = parents_map.get(id).cloned().unwrap_or_default();
            doc["links"] = json!(collection_links(&state, id, &catalog_id, &parents));
        }
    }
    let mut links = json!([
        { "rel": "self", "href": format!("{}/catalogs/{catalog_id}/collections", state.base_url) },
        { "rel": "root", "href": state.base_url },
        { "rel": "parent", "href": format!("{}/catalogs/{catalog_id}", state.base_url) }
    ]);
    let returned = collections.len();
    if let Some(next) = list_next_link(
        &state,
        &format!("/catalogs/{catalog_id}/collections"),
        limit,
        offset,
        returned,
        matched,
        None,
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
            let mut doc = state
                .store
                .get_document(COLLECTIONS_INDEX, &id)
                .await?
                .unwrap();
            let parents = state.store.get_parents(&id).await?;
            doc["links"] = json!(collection_links(&state, &id, &catalog_id, &parents));
            Ok((StatusCode::OK, Json(doc)))
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
                .create_document(COLLECTIONS_INDEX, &collection.id, &collection)
                .await?
            {
                return Err(ApiError::Conflict(collection.id));
            }
            Ok((
                StatusCode::CREATED,
                Json(collection_doc(&state, &collection, &catalog_id).await?),
            ))
        }
    }
}

pub async fn get_scoped_collection(
    Path((catalog_id, collection_id)): Path<(String, String)>,
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, ApiError> {
    require_scoped_collection(&state, &catalog_id, &collection_id).await?;
    match state
        .store
        .get_document(COLLECTIONS_INDEX, &collection_id)
        .await?
    {
        Some(mut doc) => {
            let parents = state.store.get_parents(&collection_id).await?;
            doc["links"] = json!(collection_links(
                &state,
                &collection_id,
                &catalog_id,
                &parents
            ));
            Ok(Json(doc))
        }
        None => Err(ApiError::NotFound(collection_id)),
    }
}

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
                &collection,
                version,
            )
            .await?
    } else {
        state
            .store
            .replace_document(COLLECTIONS_INDEX, &collection_id, &collection)
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

/// GET /collections — every collection in the registry (all descendants
/// of root; orphans are adopted there automatically).
pub async fn list_collections(
    Query(query): Query<ListQuery>,
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, ApiError> {
    let (limit, offset) = list_window(
        &query,
        state.default_collection_limit,
        state.max_collection_limit,
    )?;
    let mut ids: Vec<String> = state
        .store
        .get_descendant_collections(ROOT_CATALOG_ID)
        .await?
        .into_iter()
        .collect();
    ids.sort(); // HashSet order is unstable — sort for deterministic pages
    let matched = ids.len();
    let page: Vec<String> = ids
        .into_iter()
        .skip(offset as usize)
        .take(limit as usize)
        .collect();
    let parents_map = state.store.get_parents_many(&page).await?;
    let mut collections = state.store.get_documents(COLLECTIONS_INDEX, &page).await?;
    for doc in collections.iter_mut() {
        if let Some(id) = doc["id"].as_str() {
            let parents = parents_map.get(id).cloned().unwrap_or_default();
            doc["links"] = json!(global_collection_links(&state, id, &parents));
        }
    }
    let mut links = json!([
        { "rel": "self", "href": format!("{}/collections", state.base_url) },
        { "rel": "root", "href": state.base_url },
        { "rel": "parent", "href": state.base_url }
    ]);
    let returned = collections.len();
    if let Some(next) = list_next_link(
        &state,
        "/collections",
        limit,
        offset,
        returned,
        matched,
        None,
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

/// GET /collections/{id} — canonical collection read.
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
    ) {
        collection.links.push(next);
    }
    Ok(geo_json(collection))
}

/// GET /collections/{id}/items/{item_id} — global item read.
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
            doc["links"] = json!(item_links(&state, &item_id, &collection_id, None));
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

pub async fn disband_catalog(
    Path(catalog_id): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Result<StatusCode, ApiError> {
    require_catalog(&state, &catalog_id).await?;
    // Safety Disband: Unlink direct children only and auto-adopt orphans to Root.
    let direct_children = state.store.get_children(&catalog_id).await?;
    for child in direct_children {
        if !state.store.unlink_and_adopt(&child, &catalog_id).await? {
            return Err(ApiError::WriteConflict(child));
        }
    }

    // Deleting the node's doc detaches it from all parents (children are
    // derived from `parents` lookups — no reverse cleanup needed)
    state.store.remove_node(&catalog_id).await?;

    // Delete the catalog document itself (never child collections or items)
    state
        .store
        .delete_document(CATALOGS_INDEX, &catalog_id)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

// --- Scoped Items (reads: always on; writes: ENABLE_TRANSACTIONS_EXTENSIONS) ---

/// Enforce that the item's declared `collection` matches the path.
fn validate_item_collection(item: &mut Item, collection_id: &str) -> Result<(), ApiError> {
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
    let (limit, _) = list_window(query, state.default_item_limit, state.max_item_limit)?;
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
            .map_err(|_| {
                ApiError::BadRequest("Invalid parameters provided: bbox".to_string())
            })?;
        search.items.bbox = Some(nums.try_into().map_err(|_| {
            ApiError::BadRequest("Invalid parameters provided: bbox".to_string())
        })?);
    }
    if let Some(datetime) = &query.datetime {
        search.items.datetime = Some(datetime.clone());
    }
    if let Some(sortby) = &query.sortby {
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
            stac_api::Filter::Cql2Text(expr.clone())
        };
        extra_filters.push(
            crate::store::cql2_clause(&filter)
                .map_err(|e| ApiError::BadRequest(format!("Invalid filter parameter: {e}.")))?,
        );
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
) -> Option<Link> {
    let returned = collection.number_returned.unwrap_or(0);
    let matched = collection.number_matched.unwrap_or(0);
    if offset + returned >= matched {
        return None;
    }
    let mut link = Link::new(
        format!(
            "{}{path}?limit={}&token={}",
            state.base_url,
            limit,
            encode_item_token(offset + limit)
        ),
        "next",
    );
    link.r#type = Some("application/geo+json".to_string());
    Some(link)
}

pub async fn get_scoped_item(
    Path((catalog_id, collection_id, item_id)): Path<(String, String, String)>,
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, ApiError> {
    require_scoped_collection(&state, &catalog_id, &collection_id).await?;
    match state
        .store
        .get_document(ITEMS_INDEX, &item_key(&item_id, &collection_id))
        .await?
    {
        Some(mut doc) if doc["collection"] == collection_id => {
            doc["links"] = json!(item_links(
                &state,
                &item_id,
                &collection_id,
                Some(&catalog_id)
            ));
            if let Some(obj) = doc.as_object_mut() {
                strip_excluded_item_fields(&state, obj);
            }
            Ok(geo_json(doc))
        }
        _ => Err(ApiError::NotFound(item_id)),
    }
}

pub async fn create_scoped_item(
    Path((catalog_id, collection_id)): Path<(String, String)>,
    State(state): State<Arc<AppState>>,
    StacJson(mut item): StacJson<Item>,
) -> Result<impl IntoResponse, ApiError> {
    require_scoped_collection(&state, &catalog_id, &collection_id).await?;
    validate_item_collection(&mut item, &collection_id)?;
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

pub async fn update_scoped_item(
    Path((catalog_id, collection_id, item_id)): Path<(String, String, String)>,
    State(state): State<Arc<AppState>>,
    StacJson(mut item): StacJson<Item>,
) -> Result<impl IntoResponse, ApiError> {
    require_scoped_collection(&state, &catalog_id, &collection_id).await?;
    validate_item_collection(&mut item, &collection_id)?;
    if !state
        .store
        .put_document(ITEMS_INDEX, &item_key(&item_id, &collection_id), &item)
        .await?
    {
        return Err(ApiError::WriteConflict(item_id));
    }
    Ok(geo_json(item_doc(&state, &item, Some(&catalog_id))))
}

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
        .create_document(COLLECTIONS_INDEX, &collection.id, &collection)
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
pub async fn update_root_collection(
    Path(collection_id): Path<String>,
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    StacJson(mut collection): StacJson<Collection>,
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
    collection.links.clear();
    // With If-Match the write is conditioned on the version that
    // satisfied the check — check and write are one atomic step.
    let applied = if headers.contains_key(header::IF_MATCH) {
        state
            .store
            .index_document_versioned(
                COLLECTIONS_INDEX,
                &collection_id,
                &collection,
                version,
            )
            .await?
    } else {
        state
            .store
            .replace_document(COLLECTIONS_INDEX, &collection_id, &collection)
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
    validate_item_collection(&mut item, &collection_id)?;
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
        validate_item_collection(&mut item, collection_id)?;
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
            validate_item_collection(&mut item, &collection_id)?;
            require_if_match(&headers, &etag_of(&state, ITEMS_INDEX, &version))?;
            let key = item_key(&item_id, &collection_id);
            let applied = if headers.contains_key(header::IF_MATCH) {
                state
                    .store
                    .index_document_versioned(ITEMS_INDEX, &key, &item, version)
                    .await?
            } else {
                state.store.replace_document(ITEMS_INDEX, &key, &item).await?
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
                    )
                        || seen.insert(op.to_string())
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
pub async fn patch_root_collection(
    Path(collection_id): Path<String>,
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    StacJson(patch): StacJson<serde_json::Value>,
) -> Result<impl IntoResponse, ApiError> {
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
    if doc["id"].as_str() != Some(collection_id.as_str())
        || !is_collection_doc(&doc)
    {
        return Err(ApiError::BadRequest(
            "a patch may not change the collection id or type".to_string(),
        ));
    }
    require_if_match(&headers, &etag_of(&state, COLLECTIONS_INDEX, &version))?;
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
                state.store.replace_document(ITEMS_INDEX, &key, &doc).await?
            };
            if !applied {
                return Err(if headers.contains_key(header::IF_MATCH) {
                    ApiError::PreconditionFailed
                } else {
                    ApiError::WriteConflict(item_id)
                });
            }
            doc["links"] = json!(item_links(&state, &item_id, &collection_id, None));
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
    let items = body
        .get("items")
        .and_then(serde_json::Value::as_object)
        .cloned()
        .ok_or_else(|| ApiError::BadRequest("body missing 'items' object".into()))?;
    let received = items.len();
    let (mut success, mut skipped) = (0usize, 0usize);
    let mut errors: Vec<serde_json::Value> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (id, raw) in items {
        if !seen.insert(id.clone()) {
            skipped += 1;
            continue;
        }
        let mut item: Item = match serde_json::from_value(raw) {
            Ok(i) => i,
            Err(e) => {
                errors.push(json!({"id": id, "msg": format!("invalid item: {e}")}));
                continue;
            }
        };
        if let Err(e) = validate_item_collection(&mut item, &collection_id) {
            let msg = match e {
                ApiError::BadRequest(m) => m,
                _ => "invalid collection identity".to_string(),
            };
            errors.push(json!({"id": id, "msg": msg}));
            continue;
        }
        let key = item_key(&item.id, &collection_id);
        let applied = if upsert {
            state.store.put_document(ITEMS_INDEX, &key, &item).await?
        } else {
            state.store.create_document(ITEMS_INDEX, &key, &item).await?
        };
        if applied {
            success += 1;
        } else {
            errors.push(json!({"id": id, "msg": "item already exists or write conflict"}));
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
            ApiError::WriteConflict(id) => (
                StatusCode::CONFLICT,
                format!("Resource '{id}' was modified concurrently — retry the request"),
            ),
            ApiError::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg),
        };
        (status, Json(json!({ "detail": msg }))).into_response()
    }
}
