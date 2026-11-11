// src/handlers/collections.rs
use super::common::*;
use super::metadata::index_mappings;
use crate::dto::CreateOrLinkPayload;
use crate::store::{NodeKind, CATALOGS_INDEX, COLLECTIONS_INDEX, ROOT_CATALOG_ID};
use axum::{
    extract::{Path, Query, RawQuery, State},
    http::{header, StatusCode},
    response::IntoResponse,
    Json,
};
use serde::Deserialize;
use serde_json::json;
use stac::Collection;
use std::sync::Arc;

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
            "query": split_q_terms(q).join(" "),
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
        (status = 201, description = "Created"),
        (status = 400, description = "Invalid request"),
        (status = 404, description = "Not found"),
        (status = 409, description = "Resource id already exists or write conflict")
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
            validate_stac_doc(&state, &collection).await?;
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
        (status = 409, description = "Resource id already exists or write conflict"),
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
    require_if_match(
        &state,
        &headers,
        &etag_of(&state, COLLECTIONS_INDEX, &version),
    )?;
    validate_stac_doc(&state, &collection).await?;
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
        (status = 204, description = "No content"),
        (status = 404, description = "Not found"),
        (status = 409, description = "Resource id already exists or write conflict")
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
    filter: Option<crate::api_types::Filter>,
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

/// `q=EO,Earth Observation` -> `["EO", "Earth Observation"]` — commas
/// are the basic free-text OR separator (spaces have no special
/// meaning, so each comma-separated term is a phrase).
fn split_q_terms(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect()
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

fn parse_cql2_filter(raw: &str, lang: Option<&str>) -> Result<crate::api_types::Filter, ApiError> {
    if lang == Some("cql2-json") {
        match serde_json::from_str::<serde_json::Value>(raw) {
            Ok(serde_json::Value::Object(map)) => Ok(crate::api_types::Filter::Cql2Json(map)),
            _ => Err(ApiError::BadRequest(
                "Invalid filter parameter: expected valid CQL2 JSON.".to_string(),
            )),
        }
    } else {
        Ok(crate::api_types::Filter::Cql2Text(
            crate::store::rewrite_between(raw),
        ))
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
        q: q.q.map(|s| split_q_terms(&s)).unwrap_or_default(),
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
            serde_json::Value::String(s) => split_q_terms(s),
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
            serde_json::Value::Object(m) => crate::api_types::Filter::Cql2Json(m.clone()),
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

// --- Core Transaction Handlers (canonical /collections surface) ---

/// POST /collections — create a collection parented under root.
#[utoipa::path(
    post,
    path = "/collections",
    tag = "collections",
    summary = "Create a collection",
    request_body = serde_json::Value,
    responses(
        (status = 201, description = "Created"),
        (status = 400, description = "Invalid request"),
        (status = 404, description = "Not found"),
        (status = 409, description = "Resource id already exists or write conflict")
    )
)]
pub async fn create_root_collection(
    State(state): State<Arc<AppState>>,
    StacJson(collection): StacJson<Collection>,
) -> Result<impl IntoResponse, ApiError> {
    validate_stac_doc(&state, &collection).await?;
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
        (status = 409, description = "Resource id already exists or write conflict"),
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
    require_if_match(
        &state,
        &headers,
        &etag_of(&state, COLLECTIONS_INDEX, &version),
    )?;
    validate_stac_doc(&state, &collection).await?;
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
        (status = 204, description = "No content"),
        (status = 404, description = "Not found"),
        (status = 409, description = "Resource id already exists or write conflict"),
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
    require_if_match(
        &state,
        &headers,
        &etag_of(&state, COLLECTIONS_INDEX, &version),
    )?;
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
    require_if_match(
        &state,
        &headers,
        &etag_of(&state, COLLECTIONS_INDEX, &version),
    )?;
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
    // Schema-validate the merged result before our derived fields
    // (bbox_shape) are injected.
    validate_stac_doc(&state, &doc).await?;
    if state.collections_search_enabled() {
        if let Some(obj) = doc.as_object_mut() {
            collection_search_fields(obj);
        }
    }
    // Upstream `patch_item` forces refresh=True — same rationale here.
    let applied = if headers.contains_key(header::IF_MATCH) {
        state
            .store
            .index_document_versioned_with(
                COLLECTIONS_INDEX,
                &collection_id,
                &doc,
                version,
                opensearch::params::Refresh::True,
            )
            .await?
    } else {
        state
            .store
            .replace_document_with(
                COLLECTIONS_INDEX,
                &collection_id,
                &doc,
                opensearch::params::Refresh::True,
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
        Json(global_collection_doc(&state, &collection_id).await?),
    ))
}
