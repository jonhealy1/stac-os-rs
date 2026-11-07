// src/handlers/items.rs
use super::common::*;
use super::metadata::items_sortable_fields;
use crate::api_types::{ItemCollection, Search};
use crate::store::{COLLECTIONS_INDEX, ITEMS_INDEX};
use axum::{
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::IntoResponse,
    Json,
};
use futures_util::{StreamExt, TryStreamExt};
use serde_json::json;
use stac::{Bbox, Item, Link};
use std::sync::Arc;

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

/// Schema-validation shape for an item write: the doc with the path
/// collection injected (when absent), generated links merged in, and
/// `assets` defaulted — the item schema requires `links`/`assets` and a
/// `rel=collection` link whenever `collection` is set, all of which
/// this API serves but never stores (inferred rels are stripped on
/// write, `Item` serializes no `assets` key when empty).
fn item_validation_doc(
    state: &AppState,
    doc: &serde_json::Value,
    collection_id: &str,
    scope: Option<&str>,
) -> serde_json::Value {
    let mut doc = doc.clone();
    let Some(obj) = doc.as_object_mut() else {
        // Non-object bodies get no injection — the validator reports
        // them as scalar JSON on its own.
        return doc;
    };
    obj.entry("collection")
        .or_insert_with(|| json!(collection_id));
    obj.entry("assets").or_insert_with(|| json!({}));
    let id = doc["id"].as_str().unwrap_or_default().to_string();
    merge_doc_links(
        &mut doc,
        item_links(state, &id, collection_id, scope),
        &state.base_url,
    );
    doc
}

/// `validate_stac_doc` over an owned doc — the named-fn boundary lets
/// batch code hand owned futures to `buffered` (an `async` block inside
/// a closure can't satisfy the handler's `FnOnce` bounds).
async fn validate_member_doc(state: &AppState, doc: serde_json::Value) -> Result<(), ApiError> {
    validate_stac_doc(state, &doc).await
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
                Ok(serde_json::Value::Object(map)) => crate::api_types::Filter::Cql2Json(map),
                _ => {
                    return Err(ApiError::BadRequest(
                        "Invalid filter parameter: expected valid CQL2 JSON.".to_string(),
                    ))
                }
            }
        } else {
            match serde_json::from_str::<serde_json::Value>(expr) {
                Ok(serde_json::Value::Object(map)) => crate::api_types::Filter::Cql2Json(map),
                _ => crate::api_types::Filter::Cql2Text(crate::store::rewrite_between(expr)),
            }
        };
        extra_filters.push(crate::store::cql2_clause(&filter).map_err(bad_filter)?);
    }
    let (mut items, matched) = state
        .store
        .search_items(
            collections,
            &search,
            extra_filters,
            state.default_item_limit,
        )
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
        (status = 201, description = "Created"),
        (status = 400, description = "Invalid request"),
        (status = 404, description = "Not found"),
        (status = 409, description = "Resource id already exists or write conflict")
    )
)]
pub async fn create_scoped_item(
    Path((catalog_id, collection_id)): Path<(String, String)>,
    State(state): State<Arc<AppState>>,
    StacJson(body): StacJson<serde_json::Value>,
) -> Result<axum::response::Response, ApiError> {
    require_scoped_collection(&state, &catalog_id, &collection_id).await?;
    // Scope check precedes body validation (upstream order). A
    // FeatureCollection body validates each member in the batch.
    if body.get("type").and_then(serde_json::Value::as_str) == Some("FeatureCollection") {
        return create_items_batch(&state, &collection_id, Some(&catalog_id), &body)
            .await
            .map(IntoResponse::into_response);
    }
    validate_stac_doc(
        &state,
        &item_validation_doc(&state, &body, &collection_id, Some(&catalog_id)),
    )
    .await?;
    let mut item: Item = serde_json::from_value(body)
        .map_err(|e| ApiError::BadRequest(format!("invalid item: {e}")))?;
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
    )
        .into_response())
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
        (status = 404, description = "Not found"),
        (status = 409, description = "Resource id already exists or write conflict")
    )
)]
pub async fn update_scoped_item(
    Path((catalog_id, collection_id, item_id)): Path<(String, String, String)>,
    State(state): State<Arc<AppState>>,
    StacJson(mut item): StacJson<Item>,
) -> Result<impl IntoResponse, ApiError> {
    require_scoped_collection(&state, &catalog_id, &collection_id).await?;
    validate_item_collection(&mut item, &collection_id, &state.base_url)?;
    let doc = serde_json::to_value(&item).map_err(|e| ApiError::Internal(e.to_string()))?;
    validate_stac_doc(
        &state,
        &item_validation_doc(&state, &doc, &collection_id, Some(&catalog_id)),
    )
    .await?;
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
        (status = 204, description = "No content"),
        (status = 404, description = "Not found"),
        (status = 409, description = "Resource id already exists or write conflict")
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

/// POST /collections/{id}/items — create an item in a collection.
#[utoipa::path(
    post,
    path = "/collections/{collection_id}/items",
    tag = "items",
    summary = "Create an item",
    params(("collection_id" = String, Path, description = "Collection id")),
    request_body = serde_json::Value,
    responses(
        (status = 201, description = "Created"),
        (status = 400, description = "Invalid request"),
        (status = 404, description = "Not found"),
        (status = 409, description = "Resource id already exists or write conflict")
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
        return create_items_batch(&state, &collection_id, None, &body)
            .await
            .map(IntoResponse::into_response);
    }
    validate_stac_doc(
        &state,
        &item_validation_doc(&state, &body, &collection_id, None),
    )
    .await?;
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
/// `scope` is the catalog context for response link rendering.
///
/// Storage: one mget existence preflight + one `_bulk` create — a single
/// refresh for the whole batch. The old per-doc loop made 2N round-trips,
/// each waiting for an index refresh. A raced create between preflight
/// and bulk still reports 409 (docs already written in that request stay,
/// same partial-write exposure the sequential loop had).
async fn create_items_batch(
    state: &AppState,
    collection_id: &str,
    scope: Option<&str>,
    body: &serde_json::Value,
) -> Result<(StatusCode, impl IntoResponse), ApiError> {
    let features = body
        .get("features")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| ApiError::BadRequest("FeatureCollection missing features".into()))?;
    // Schema-validate members in parallel across the validator pool —
    // sequential per-member awaits would pin a single validator for the
    // whole batch. `buffered` keeps POOL_SIZE validations in flight and
    // preserves doc order, so the first error is deterministic.
    let docs: Vec<serde_json::Value> = features
        .iter()
        .map(|f| item_validation_doc(state, f, collection_id, scope))
        .collect();
    futures_util::stream::iter(docs)
        .map(|doc| validate_member_doc(state, doc))
        .buffered(VALIDATOR_POOL_SIZE)
        .try_collect::<()>()
        .await?;
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
    let keys: Vec<String> = order.iter().map(|id| item_key(id, collection_id)).collect();
    let existing = state.store.existing_ids(ITEMS_INDEX, &keys).await?;
    if let Some(id) = order
        .iter()
        .find(|id| existing.contains(&item_key(id, collection_id)))
    {
        return Err(ApiError::Conflict(id.clone()));
    }
    let mut ops = Vec::with_capacity(order.len());
    for (id, key) in order.iter().zip(&keys) {
        let doc =
            serde_json::to_value(&by_id[id]).map_err(|e| ApiError::Internal(e.to_string()))?;
        ops.push((key.clone(), doc, true));
    }
    let results = state.store.bulk_write(ITEMS_INDEX, &ops).await?;
    for (id, res) in order.iter().zip(&results) {
        match res.status {
            s if (200..300).contains(&s) => {}
            409 => return Err(ApiError::Conflict(id.clone())),
            _ => {
                return Err(ApiError::Internal(format!(
                    "bulk create failed for '{id}': {}",
                    res.error.as_deref().unwrap_or("unknown error")
                )));
            }
        }
    }
    let docs: Vec<serde_json::Value> = order
        .iter()
        .map(|id| item_doc(state, &by_id[id], scope))
        .collect();
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
        (status = 409, description = "Resource id already exists or write conflict"),
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
            require_if_match(&state, &headers, &etag_of(&state, ITEMS_INDEX, &version))?;
            validate_item_collection(&mut item, &collection_id, &state.base_url)?;
            let value =
                serde_json::to_value(&item).map_err(|e| ApiError::Internal(e.to_string()))?;
            validate_stac_doc(
                &state,
                &item_validation_doc(&state, &value, &collection_id, None),
            )
            .await?;
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
        (status = 204, description = "No content"),
        (status = 404, description = "Not found"),
        (status = 409, description = "Resource id already exists or write conflict"),
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
            require_if_match(&state, &headers, &etag_of(&state, ITEMS_INDEX, &version))?;
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
            require_if_match(&state, &headers, &etag_of(&state, ITEMS_INDEX, &version))?;
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
            // Schema-validate the merged result in read shape — stored
            // docs strip inferred links and the item schema keys
            // `collection` on a `rel=collection` link.
            validate_stac_doc(
                &state,
                &item_validation_doc(&state, &doc, &collection_id, None),
            )
            .await?;
            let key = item_key(&item_id, &collection_id);
            // Upstream `patch_item` forces refresh=True — a patched item
            // must be searchable immediately regardless of DATABASE_REFRESH.
            let applied = if headers.contains_key(header::IF_MATCH) {
                state
                    .store
                    .index_document_versioned_with(
                        ITEMS_INDEX,
                        &key,
                        &doc,
                        version,
                        opensearch::params::Refresh::True,
                    )
                    .await?
            } else {
                state
                    .store
                    .replace_document_with(
                        ITEMS_INDEX,
                        &key,
                        &doc,
                        opensearch::params::Refresh::True,
                    )
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
        (status = 409, description = "Resource id already exists or write conflict")
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
    if upsert && state.require_if_match {
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
    // Deserialize + identity-check per item first (cheap, sequential),
    // then schema-validate the survivors in parallel across the
    // validator pool, and finally one `_bulk` request — a single
    // round-trip and one refresh for the whole batch.
    let mut candidates: Vec<(&serde_json::Value, Item)> = Vec::new();
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
        candidates.push((raw, item));
    }
    // `buffered` keeps POOL_SIZE validations in flight, results in
    // order. Schema failures are per-item errors like the other
    // malformed admissions; a fetch failure is environmental — abort.
    let docs: Vec<serde_json::Value> = candidates
        .iter()
        .map(|(raw, _)| item_validation_doc(&state, raw, &collection_id, None))
        .collect();
    let validations = futures_util::stream::iter(docs)
        .map(|doc| validate_member_doc(&state, doc))
        .buffered(VALIDATOR_POOL_SIZE)
        .collect::<Vec<_>>()
        .await;
    let mut prepared: Vec<(String, String, serde_json::Value)> = Vec::new();
    for ((raw, item), result) in candidates.into_iter().zip(validations) {
        match result {
            Ok(()) => {
                let doc =
                    serde_json::to_value(&item).map_err(|e| ApiError::Internal(e.to_string()))?;
                prepared.push((item_key(&item.id, &collection_id), item.id.clone(), doc));
            }
            Err(ApiError::BadRequest(msg)) => {
                errors.push(json!({"id": raw["id"], "msg": msg}));
            }
            Err(e) => return Err(e),
        }
    }
    let ops: Vec<(String, serde_json::Value, bool)> = prepared
        .iter()
        .map(|(key, _, doc)| (key.clone(), doc.clone(), !upsert))
        .collect();
    let results = state.store.bulk_write(ITEMS_INDEX, &ops).await?;
    for ((_, id, _), res) in prepared.iter().zip(&results) {
        match res.status {
            s if (200..300).contains(&s) => success += 1,
            409 => {
                skipped += 1;
                if raise_on_error {
                    return Err(ApiError::Conflict(format!(
                        "Item '{id}' already exists in collection '{collection_id}'"
                    )));
                }
                errors.push(json!({"id": id, "msg": "item already exists or write conflict"}));
            }
            _ => {
                skipped += 1;
                errors.push(json!({
                    "id": id,
                    "msg": format!("bulk write failed: {}", res.error.as_deref().unwrap_or("unknown error"))
                }));
            }
        }
    }
    Ok(Json(json!({
        "received": received,
        "success": success,
        "skipped": skipped,
        "errors": errors,
    })))
}
