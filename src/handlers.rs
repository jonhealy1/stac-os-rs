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
}

/// Resolve `?limit=&token=` into `(limit, offset)`. `limit=0` is a 400;
/// an unparseable token restarts from 0 (matching upstream leniency).
fn list_window(q: &ListQuery) -> Result<(u64, u64), ApiError> {
    let limit = q.limit.unwrap_or(DEFAULT_LIST_LIMIT);
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
async fn global_collection_doc(
    state: &AppState,
    collection_id: &str,
) -> Result<serde_json::Value, ApiError> {
    let mut doc = state
        .store
        .get_document(COLLECTIONS_INDEX, collection_id)
        .await?
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
    let (limit, offset) = list_window(&query)?;
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
    let (limit, offset) = list_window(&ListQuery {
        limit: query.limit,
        token: query.token.clone(),
        sortby: None,
    })?;
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
    let (limit, offset) = list_window(&query)?;
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

/// Shared scoped-search pipeline: intersect the request's collections with
/// the scope's descendants, then run the OpenSearch query.
async fn run_scoped_search(
    state: &AppState,
    scope_id: &str,
    mut search: Search,
) -> Result<impl IntoResponse, ApiError> {
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
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;

    let empty = || {
        let mut c =
            ItemCollection::new(Vec::new()).map_err(|e| ApiError::Internal(e.to_string()))?;
        c.number_matched = Some(0);
        c.number_returned = Some(0);
        Ok::<_, ApiError>(c)
    };

    // 1. Resolve all descendant collection IDs in the scope's DAG
    let allowed_collections = state.store.get_descendant_collections(scope_id).await?;
    if allowed_collections.is_empty() {
        return Ok(geo_json(empty()?));
    }

    // 2. Security & Intersection: Enforce scope boundaries on search payload
    if search.collections.is_empty() {
        // No filter provided — restrict to all descendants of this scope
        search.collections = allowed_collections.into_iter().collect();
    } else {
        // Intersect requested collections with allowed descendants
        search
            .collections
            .retain(|c| allowed_collections.contains(c));
        if search.collections.is_empty() {
            return Ok(geo_json(empty()?));
        }
    }

    // 3. Query OpenSearch — collections scope + bbox/datetime/intersects/ids
    let (mut items, matched) = state
        .store
        .search_items(&search.collections, &search)
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
/// search body with an updated `offset`.
fn page_link(rel: &str, href: &str, search: &Search, offset: u64) -> Link {
    let mut body = serde_json::to_value(search).unwrap_or_default();
    body["offset"] = json!(offset);
    let mut link = Link::new(href, rel);
    link.method = Some("POST".to_string());
    link.r#type = Some("application/geo+json".to_string());
    link.body = body.as_object().cloned();
    link
}

pub async fn scoped_search_post(
    Path(catalog_id): Path<String>,
    State(state): State<Arc<AppState>>,
    StacJson(search): StacJson<Search>,
) -> Result<impl IntoResponse, ApiError> {
    run_scoped_search(&state, &catalog_id, search).await
}

pub async fn scoped_search_get(
    Path(catalog_id): Path<String>,
    State(state): State<Arc<AppState>>,
    Query(params): Query<GetSearch>,
) -> Result<impl IntoResponse, ApiError> {
    let search = Search::try_from(params).map_err(|e| ApiError::BadRequest(e.to_string()))?;
    run_scoped_search(&state, &catalog_id, search).await
}

/// Search across the whole catalogs registry — equivalent to scoping to
/// `root`, since every orphan is adopted there.
pub async fn catalogs_search_post(
    State(state): State<Arc<AppState>>,
    StacJson(search): StacJson<Search>,
) -> Result<impl IntoResponse, ApiError> {
    run_scoped_search(&state, ROOT_CATALOG_ID, search).await
}

pub async fn catalogs_search_get(
    State(state): State<Arc<AppState>>,
    Query(params): Query<GetSearch>,
) -> Result<impl IntoResponse, ApiError> {
    let search = Search::try_from(params).map_err(|e| ApiError::BadRequest(e.to_string()))?;
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
    let (limit, offset) = list_window(&query)?;
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
    StacJson(collection): StacJson<Collection>,
) -> Result<impl IntoResponse, ApiError> {
    require_scoped_collection(&state, &catalog_id, &collection_id).await?;
    if collection.id != collection_id {
        return Err(ApiError::BadRequest(format!(
            "body id '{}' does not match path id '{collection_id}'",
            collection.id
        )));
    }
    if !state
        .store
        .replace_document(COLLECTIONS_INDEX, &collection_id, &collection)
        .await?
    {
        return Err(ApiError::WriteConflict(collection_id));
    }
    Ok(Json(
        collection_doc(&state, &collection, &catalog_id).await?,
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
    let (limit, offset) = list_window(&query)?;
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
    Ok(Json(global_collection_doc(&state, &collection_id).await?))
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
    let mut collection =
        items_page(&state, std::slice::from_ref(&collection_id), &query, None).await?;
    let offset = query
        .token
        .as_deref()
        .and_then(|t| t.parse::<u64>().ok())
        .unwrap_or(0);
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
    match state.store.get_document(ITEMS_INDEX, &item_id).await? {
        Some(mut doc) if doc["collection"] == collection_id => {
            doc["links"] = json!(item_links(&state, &item_id, &collection_id, None));
            Ok(geo_json(doc))
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
        Some(c) if c != collection_id => Err(ApiError::BadRequest(format!(
            "item collection '{c}' does not match path collection '{collection_id}'"
        ))),
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
    let mut collection = items_page(
        &state,
        std::slice::from_ref(&collection_id),
        &query,
        Some(&catalog_id),
    )
    .await?;
    let offset = query
        .token
        .as_deref()
        .and_then(|t| t.parse::<u64>().ok())
        .unwrap_or(0);
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

/// Shared items-listing pipeline: `?limit=&token=` -> paginated search.
/// `scoped_catalog_id` selects scoped vs canonical item link paths.
async fn items_page(
    state: &AppState,
    collections: &[String],
    query: &ListQuery,
    scoped_catalog_id: Option<&str>,
) -> Result<ItemCollection, ApiError> {
    let (limit, offset) = list_window(query)?;
    let mut search = Search::default();
    search.items.limit = Some(limit);
    search
        .items
        .additional_fields
        .insert("offset".to_string(), json!(offset));
    if let Some(sortby) = &query.sortby {
        search.items.sortby = sortby
            .split(',')
            .map(|s| s.parse().expect("infallible"))
            .collect();
    }
    let (mut items, matched) = state.store.search_items(collections, &search).await?;
    for item in items.iter_mut() {
        let item_id = item["id"].as_str().unwrap_or_default().to_string();
        if let Some(col) = item["collection"].as_str().map(str::to_owned) {
            item.insert(
                "links".to_string(),
                json!(item_links(state, &item_id, &col, scoped_catalog_id)),
            );
        }
    }
    let returned = items.len() as u64;
    let mut collection =
        ItemCollection::new(items).map_err(|e| ApiError::Internal(e.to_string()))?;
    collection.number_matched = Some(matched);
    collection.number_returned = Some(returned);
    Ok(collection)
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
            offset + limit
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
    match state.store.get_document(ITEMS_INDEX, &item_id).await? {
        Some(mut doc) if doc["collection"] == collection_id => {
            doc["links"] = json!(item_links(
                &state,
                &item_id,
                &collection_id,
                Some(&catalog_id)
            ));
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
        .create_document(ITEMS_INDEX, &item.id, &item)
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
        .put_document(ITEMS_INDEX, &item_id, &item)
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
    match state.store.get_document(ITEMS_INDEX, &item_id).await? {
        Some(doc) if doc["collection"] == collection_id => {
            state.store.delete_document(ITEMS_INDEX, &item_id).await?;
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
    Ok((
        StatusCode::CREATED,
        Json(global_collection_doc(&state, &collection.id).await?),
    ))
}

/// PUT /collections/{id} — update a collection; DAG memberships preserved.
pub async fn update_root_collection(
    Path(collection_id): Path<String>,
    State(state): State<Arc<AppState>>,
    StacJson(mut collection): StacJson<Collection>,
) -> Result<impl IntoResponse, ApiError> {
    if collection.id != collection_id {
        return Err(ApiError::BadRequest(format!(
            "body id '{}' does not match path '{collection_id}'",
            collection.id
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
    collection.links.clear();
    if !state
        .store
        .replace_document(COLLECTIONS_INDEX, &collection_id, &collection)
        .await?
    {
        return Err(ApiError::WriteConflict(collection_id));
    }
    Ok(Json(global_collection_doc(&state, &collection.id).await?))
}

/// DELETE /collections/{id} — removes the collection, its DAG node, and
/// all of its items (canonical route semantics: a collection owns items).
pub async fn delete_root_collection(
    Path(collection_id): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Result<StatusCode, ApiError> {
    if state
        .store
        .get_document(COLLECTIONS_INDEX, &collection_id)
        .await?
        .is_none()
    {
        return Err(ApiError::NotFound(collection_id));
    }
    state
        .store
        .delete_items_by_collection(&collection_id)
        .await?;
    state.store.remove_node(&collection_id).await?;
    state
        .store
        .delete_document(COLLECTIONS_INDEX, &collection_id)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// POST /collections/{id}/items — create an item in a collection.
pub async fn create_root_item(
    Path(collection_id): Path<String>,
    State(state): State<Arc<AppState>>,
    StacJson(mut item): StacJson<Item>,
) -> Result<impl IntoResponse, ApiError> {
    if state
        .store
        .get_document(COLLECTIONS_INDEX, &collection_id)
        .await?
        .is_none()
    {
        return Err(ApiError::NotFound(collection_id));
    }
    validate_item_collection(&mut item, &collection_id)?;
    if state
        .store
        .get_document(ITEMS_INDEX, &item.id)
        .await?
        .is_some()
    {
        return Err(ApiError::Conflict(item.id));
    }
    if !state
        .store
        .create_document(ITEMS_INDEX, &item.id, &item)
        .await?
    {
        return Err(ApiError::Conflict(item.id));
    }
    Ok((StatusCode::CREATED, geo_json(item_doc(&state, &item, None))))
}

/// PUT /collections/{id}/items/{item_id} — update an item.
pub async fn update_root_item(
    Path((collection_id, item_id)): Path<(String, String)>,
    State(state): State<Arc<AppState>>,
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
    match state.store.get_document(ITEMS_INDEX, &item_id).await? {
        Some(doc) if doc["collection"] == collection_id => {
            validate_item_collection(&mut item, &collection_id)?;
            if !state
                .store
                .replace_document(ITEMS_INDEX, &item_id, &item)
                .await?
            {
                return Err(ApiError::WriteConflict(item_id));
            }
            Ok(geo_json(item_doc(&state, &item, None)))
        }
        _ => Err(ApiError::NotFound(item_id)),
    }
}

/// DELETE /collections/{id}/items/{item_id} — delete an item.
pub async fn delete_root_item(
    Path((collection_id, item_id)): Path<(String, String)>,
    State(state): State<Arc<AppState>>,
) -> Result<StatusCode, ApiError> {
    if state
        .store
        .get_document(COLLECTIONS_INDEX, &collection_id)
        .await?
        .is_none()
    {
        return Err(ApiError::NotFound(collection_id));
    }
    match state.store.get_document(ITEMS_INDEX, &item_id).await? {
        Some(doc) if doc["collection"] == collection_id => {
            state.store.delete_document(ITEMS_INDEX, &item_id).await?;
            Ok(StatusCode::NO_CONTENT)
        }
        _ => Err(ApiError::NotFound(item_id)),
    }
}

// --- Error Handling ---

pub enum ApiError {
    BadRequest(String),
    NotFound(String),
    Conflict(String),
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
            ApiError::NotFound(id) => (StatusCode::NOT_FOUND, format!("Resource '{id}' not found")),
            ApiError::Conflict(id) => (
                StatusCode::CONFLICT,
                format!("Resource '{id}' already exists"),
            ),
            ApiError::WriteConflict(id) => (
                StatusCode::CONFLICT,
                format!("Resource '{id}' was modified concurrently — retry the request"),
            ),
            ApiError::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg),
        };
        (
            status,
            Json(json!({ "code": status.as_u16(), "description": msg })),
        )
            .into_response()
    }
}
