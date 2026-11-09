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
use futures_util::StreamExt;
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

/// Upstream `validate_datetime_range` (sfeos `validate.py`), gated like
/// upstream on the schema-validation flag: when `properties.datetime`
/// is null both start/end are required, they come in pairs, and
/// `start <= datetime <= end` must hold. Returns the rule text.
fn validate_item_datetime(doc: &serde_json::Value) -> Result<(), String> {
    let props = doc.get("properties");
    let raw = |k: &str| -> Option<String> {
        props
            .and_then(|p| p.get(k))
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    let parse = |k: &str, s: &str| -> Result<chrono::DateTime<chrono::FixedOffset>, String> {
        chrono::DateTime::parse_from_rfc3339(s)
            .map_err(|_| format!("Invalid ISO 8601 format for '{k}': {s}"))
    };
    let opt = |k: &str| -> Result<Option<chrono::DateTime<chrono::FixedOffset>>, String> {
        raw(k).map(|s| parse(k, &s)).transpose()
    };
    let (start, dt, end) = (
        opt("start_datetime")?,
        opt("datetime")?,
        opt("end_datetime")?,
    );
    if dt.is_none() && (start.is_none() || end.is_none()) {
        return Err(
            "If 'datetime' is null, both 'start_datetime' and 'end_datetime' must be set".into(),
        );
    }
    if start.is_some() != end.is_some() {
        return Err(
            "Both 'start_datetime' and 'end_datetime' must be set together, or both must be null"
                .into(),
        );
    }
    if let (Some(s), Some(e)) = (start, end) {
        if s > e {
            return Err(format!(
                "'start_datetime' ({}) must be <= 'end_datetime' ({})",
                raw("start_datetime").unwrap_or_default(),
                raw("end_datetime").unwrap_or_default()
            ));
        }
    }
    if let Some(d) = dt {
        if let Some(s) = start {
            if d < s {
                return Err(format!(
                    "'datetime' ({}) must be >= 'start_datetime' ({})",
                    raw("datetime").unwrap_or_default(),
                    raw("start_datetime").unwrap_or_default()
                ));
            }
        }
        if let Some(e) = end {
            if d > e {
                return Err(format!(
                    "'datetime' ({}) must be <= 'end_datetime' ({})",
                    raw("datetime").unwrap_or_default(),
                    raw("end_datetime").unwrap_or_default()
                ));
            }
        }
    }
    Ok(())
}

/// Upstream `validate_item_topology_lightweight`, gated on
/// `ENABLE_TOPOLOGY_VALIDATION`: WGS84 bounds on every coordinate, then
/// for Polygon/MultiPolygon ring minimum size, the
/// `MAX_TOPOLOGY_VERTICES` ceiling, and antimeridian longitude jumps.
fn validate_item_topology(state: &AppState, doc: &serde_json::Value) -> Result<(), String> {
    if !state.enable_topology_validation {
        return Ok(());
    }
    let Some(geometry) = doc.get("geometry") else {
        return Ok(());
    };
    if geometry.is_null() {
        return Ok(());
    }
    let Some(coords) = geometry.get("coordinates") else {
        return Ok(());
    };
    if coords.is_null() {
        return Ok(());
    }
    validate_geometry_bounds(coords)?;
    match geometry.get("type").and_then(|t| t.as_str()) {
        Some("Polygon") => check_antimeridian_rings(state, coords),
        Some("MultiPolygon") => {
            for polygon in coords.as_array().into_iter().flatten() {
                check_antimeridian_rings(state, polygon)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Recursive WGS84 bounds check — mirrors upstream: any nested list
/// whose first element is numeric is treated as a [lon, lat] pair.
fn validate_geometry_bounds(coords: &serde_json::Value) -> Result<(), String> {
    let Some(arr) = coords.as_array() else {
        return Ok(());
    };
    if arr.is_empty() {
        return Ok(());
    }
    if arr[0].is_number() {
        if arr.len() < 2 {
            return Err("Coordinate pair must have at least 2 elements [lon, lat]".into());
        }
        let lon = arr[0].as_f64().unwrap_or(f64::NAN);
        let lat = arr[1].as_f64().unwrap_or(f64::NAN);
        if !(-180.0..=180.0).contains(&lon) || !(-90.0..=90.0).contains(&lat) {
            return Err(format!(
                "Coordinates out of global WGS84 bounds: [{lon}, {lat}]"
            ));
        }
        return Ok(());
    }
    for sub in arr {
        validate_geometry_bounds(sub)?;
    }
    Ok(())
}

/// Polygon/MultiPolygon ring checks — `rings` is the `coordinates`
/// array of a Polygon (or one polygon inside a MultiPolygon).
fn check_antimeridian_rings(state: &AppState, rings: &serde_json::Value) -> Result<(), String> {
    let Some(rings) = rings.as_array() else {
        return Ok(());
    };
    for ring in rings {
        let Some(ring) = ring.as_array() else {
            continue;
        };
        if ring.len() < 4 {
            return Err("Polygon ring must have at least 4 coordinates (closed loop).".into());
        }
        if ring.len() > state.max_topology_vertices {
            return Err(format!(
                "Geometry has too many vertices ({}). Maximum allowed is {}.",
                ring.len(),
                state.max_topology_vertices
            ));
        }
        let lon_at = |p: &serde_json::Value| {
            p.as_array()
                .and_then(|c| c.first())
                .and_then(|v| v.as_f64())
        };
        for pair in ring.windows(2) {
            if let (Some(lon1), Some(lon2)) = (lon_at(&pair[0]), lon_at(&pair[1])) {
                if (lon1 - lon2).abs() > 180.0 {
                    return Err(format!(
                        "Geometry crosses the antimeridian without proper truncation/splitting between longitude {lon1} and {lon2}."
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Single-item write validation — mirrors upstream `_validate_single_item`:
/// schema → `Invalid item: STAC validation failed for '{id}': {e}`, then
/// datetime range → `Invalid item datetime: {e}` (gated on the validator
/// like upstream), then topology → `Invalid item geometry: {e}`.
async fn validate_item_write(
    state: &AppState,
    doc: &serde_json::Value,
    item_id: &str,
) -> Result<(), ApiError> {
    validate_stac_doc(state, doc).await.map_err(|e| match e {
        ApiError::BadRequest(msg) => ApiError::BadRequest(format!(
            "Invalid item: STAC validation failed for '{item_id}': {}",
            msg.strip_prefix("STAC validation failed: ").unwrap_or(&msg)
        )),
        other => other,
    })?;
    if !state.validators.is_empty() {
        if let Err(msg) = validate_item_datetime(doc) {
            return Err(ApiError::BadRequest(format!(
                "Invalid item datetime: {msg}"
            )));
        }
    }
    if let Err(msg) = validate_item_topology(state, doc) {
        return Err(ApiError::BadRequest(format!(
            "Invalid item geometry: {msg}"
        )));
    }
    Ok(())
}

/// Batch-member validation — the same checks as `validate_item_write`
/// but in upstream's per-item batch formats (they land in `errors[]`
/// entries and `conflict_errors`): schema → `STAC validation failed for
/// '{id}': {e}`, datetime → the bare rule text, topology → `Topology
/// Error: {e}`. The named-fn boundary also lets `buffered` hold owned
/// futures (an `async` block in a closure can't satisfy `FnOnce`).
async fn validate_member_doc(state: &AppState, doc: serde_json::Value) -> Result<(), ApiError> {
    let id = doc["id"].as_str().unwrap_or_default().to_string();
    validate_stac_doc(state, &doc).await.map_err(|e| match e {
        ApiError::BadRequest(msg) => ApiError::BadRequest(format!(
            "STAC validation failed for '{id}': {}",
            msg.strip_prefix("STAC validation failed: ").unwrap_or(&msg)
        )),
        other => other,
    })?;
    if !state.validators.is_empty() {
        if let Err(msg) = validate_item_datetime(&doc) {
            return Err(ApiError::BadRequest(msg));
        }
    }
    if let Err(msg) = validate_item_topology(state, &doc) {
        return Err(ApiError::BadRequest(format!("Topology Error: {msg}")));
    }
    Ok(())
}

/// Upstream `build_bulk_summary` telemetry, embedded in the `detail`
/// dict of strict bulk rejections ("Bulk insertion rejected." /
/// "Batch rejected." / "No items were added to the database.").
fn bulk_summary(
    input_count: usize,
    processed_count: usize,
    valid_count: usize,
    validation_error_count: usize,
    conflict_count: usize,
    database_error_count: usize,
) -> serde_json::Value {
    json!({
        "input_count": input_count,
        "processed_count": processed_count,
        "valid_count": valid_count,
        "skipped_total": input_count.saturating_sub(processed_count)
            + validation_error_count
            + conflict_count,
        "validation_error_count": validation_error_count,
        "conflict_count": conflict_count,
        "database_error_count": database_error_count,
    })
}

/// `{msg: [ids]}` grouping of `{"id","msg"}` error entries — upstream's
/// `validation_errors` dict shape inside rejection `detail`s.
fn errors_grouped_by_msg(errors: &[serde_json::Value]) -> serde_json::Value {
    let mut grouped = serde_json::Map::new();
    for e in errors {
        let msg = e["msg"].as_str().unwrap_or("unknown").to_string();
        grouped
            .entry(msg)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .expect("grouping array")
            .push(e["id"].clone());
    }
    serde_json::Value::Object(grouped)
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
    let item_id = body["id"].as_str().unwrap_or_default().to_string();
    validate_item_write(
        &state,
        &item_validation_doc(&state, &body, &collection_id, Some(&catalog_id)),
        &item_id,
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
    StacJson(body): StacJson<serde_json::Value>,
) -> Result<impl IntoResponse, ApiError> {
    require_scoped_collection(&state, &catalog_id, &collection_id).await?;
    // Validate the raw document like the create path — serde
    // normalization heals some schema violations (e.g. a missing
    // `type` re-serializes as "Feature") before the schema sees them.
    let body_id = body["id"].as_str().unwrap_or_default().to_string();
    validate_item_write(
        &state,
        &item_validation_doc(&state, &body, &collection_id, Some(&catalog_id)),
        &body_id,
    )
    .await?;
    let mut item: Item = serde_json::from_value(body)
        .map_err(|e| ApiError::BadRequest(format!("invalid item: {e}")))?;
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
    let item_id = body["id"].as_str().unwrap_or_default().to_string();
    validate_item_write(
        &state,
        &item_validation_doc(&state, &body, &collection_id, None),
        &item_id,
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

/// FeatureCollection batch POST — upstream `RAISE_ON_BULK_ERROR`
/// semantics. When false (upstream default), failed members are
/// skipped and reported: malformed/collection-mismatch members and
/// schema failures land in `errors[]`, existing-id conflicts in
/// `conflict_errors`; valid members still insert. When true, the
/// first failure class aborts the whole batch — `400 Batch rejected.
/// N items are malformed.` / `N items failed validation.`, `409` on
/// conflict. `MAX_BATCH_ERROR_SIZE` is an unconditional fail-fast
/// cutoff either way. In-batch duplicate ids collapse (last wins).
///
/// Storage: one mget existence preflight + one `_bulk` create — a
/// single refresh for the whole batch.
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
    let raise = state.raise_on_bulk_error;
    let input_count = features.len();

    // Phase 1 — admission: deserialize + collection identity, dedupe
    // last-wins in first-seen order.
    let mut order: Vec<String> = Vec::new();
    // Parsed item for storage + the raw member for validation —
    // serde normalization heals some schema violations (e.g. a missing
    // `type` re-serializes as "Feature"), so the schema must see the
    // original document, like the `bulk_items` path does.
    let mut by_id: std::collections::HashMap<String, (Item, serde_json::Value)> =
        std::collections::HashMap::new();
    let mut member_errors: Vec<(String, String)> = Vec::new();
    let mut input_dupes = 0usize;
    for f in features {
        let id = f["id"].as_str().unwrap_or_default().to_string();
        let mut item: Item = match serde_json::from_value(f.clone()) {
            Ok(i) => i,
            Err(e) => {
                member_errors.push((id, format!("invalid feature: {e}")));
                continue;
            }
        };
        if let Err(e) = validate_item_collection(&mut item, collection_id, &state.base_url) {
            let msg = match e {
                ApiError::BadRequest(m) => m,
                _ => "invalid collection identity".to_string(),
            };
            member_errors.push((item.id, msg));
            continue;
        }
        if by_id.contains_key(&item.id) {
            input_dupes += 1;
        } else {
            order.push(item.id.clone());
        }
        // Rewrite `collection` on the raw member like `bulk_items`
        // admission does — `validate_item_collection` accepts absent/
        // null/"" and the path id is authoritative for what we store.
        let mut raw = f.clone();
        raw["collection"] = json!(collection_id);
        by_id.insert(item.id.clone(), (item, raw));
    }
    if raise && !member_errors.is_empty() {
        return Err(ApiError::JsonDetail(
            StatusCode::BAD_REQUEST,
            json!({
                "message": format!("Batch rejected. {} items are malformed.", member_errors.len()),
                "errors": member_errors
                    .iter()
                    .map(|(id, msg)| json!({"id": id, "msg": msg}))
                    .collect::<Vec<_>>(),
            }),
        ));
    }
    // Phase-1 failures are upstream's "preprocessing skipped" bucket.
    let admission_error_count = member_errors.len();

    // Phase 2 — member validation, buffered across the validator pool.
    // Upstream validates before insert, so an id that both fails schema
    // and already exists reports the validation error, not a conflict.
    let docs: Vec<serde_json::Value> = order
        .iter()
        .map(|id| item_validation_doc(state, &by_id[id].1, collection_id, scope))
        .collect();
    let validations = futures_util::stream::iter(docs)
        .map(|doc| validate_member_doc(state, doc))
        .buffered(state.validators.len().max(1))
        .collect::<Vec<_>>()
        .await;
    let mut valid: Vec<String> = Vec::new();
    let mut validation_error_count = 0usize;
    let mut validation_grouped = serde_json::Map::new();
    for (id, result) in order.iter().zip(validations) {
        match result {
            Ok(()) => valid.push(id.clone()),
            Err(ApiError::BadRequest(msg)) => {
                validation_error_count += 1;
                validation_grouped
                    .entry(msg.clone())
                    .or_insert_with(|| json!([]))
                    .as_array_mut()
                    .expect("grouping array")
                    .push(json!(id));
                member_errors.push((id.clone(), msg));
            }
            Err(e) => return Err(e),
        }
    }
    if let Some(max) = state.max_batch_error_size {
        if validation_error_count > max {
            return Err(ApiError::JsonDetail(
                StatusCode::BAD_REQUEST,
                json!({
                    "message": format!("Batch rejected. Validation error threshold exceeded ({validation_error_count} errors found). Halting validation loop."),
                    "summary": bulk_summary(input_count, order.len(), valid.len(), validation_error_count, 0, 0),
                    "errors": validation_grouped,
                }),
            ));
        }
    }
    if raise && validation_error_count > 0 {
        return Err(ApiError::JsonDetail(
            StatusCode::BAD_REQUEST,
            json!({
                "message": format!("Batch rejected. {validation_error_count} items failed validation."),
                "summary": bulk_summary(input_count, order.len(), valid.len(), validation_error_count, 0, 0),
                "errors": validation_grouped,
            }),
        ));
    }

    // Phase 3 — existence preflight on the validated members →
    // conflicts (skipped or fatal), then one `_bulk` create.
    let keys: Vec<String> = valid.iter().map(|id| item_key(id, collection_id)).collect();
    let existing = state.store.existing_ids(ITEMS_INDEX, &keys).await?;
    let mut conflict_errors = serde_json::Map::new();
    let mut candidates: Vec<String> = Vec::new();
    for id in &valid {
        if existing.contains(&item_key(id, collection_id)) {
            if raise {
                return Err(ApiError::Conflict(id.clone()));
            }
            conflict_errors.insert(
                id.clone(),
                json!(format!(
                    "Item '{id}' already exists in collection '{collection_id}'"
                )),
            );
        } else {
            candidates.push(id.clone());
        }
    }

    // Phase 4 — one `_bulk` create for the surviving members.
    let ops: Vec<(String, serde_json::Value, bool)> = candidates
        .iter()
        .map(|id| {
            serde_json::to_value(&by_id[id].0).map(|doc| (item_key(id, collection_id), doc, true))
        })
        .collect::<Result<_, _>>()
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let results = state.store.bulk_write(ITEMS_INDEX, &ops).await?;
    let mut added: Vec<String> = Vec::new();
    let mut database_error_count = 0usize;
    for (id, res) in candidates.iter().zip(&results) {
        match res.status {
            s if (200..300).contains(&s) => added.push(id.clone()),
            409 => {
                if raise {
                    return Err(ApiError::Conflict(id.clone()));
                }
                conflict_errors.insert(
                    id.clone(),
                    json!(format!(
                        "Item '{id}' already exists in collection '{collection_id}'"
                    )),
                );
            }
            _ => {
                if raise {
                    return Err(ApiError::Internal(format!(
                        "bulk create failed for '{id}': {}",
                        res.error.as_deref().unwrap_or("unknown error")
                    )));
                }
                database_error_count += 1;
                member_errors.push((
                    id.clone(),
                    format!(
                        "bulk create failed: {}",
                        res.error.as_deref().unwrap_or("unknown error")
                    ),
                ));
            }
        }
    }
    // Upstream: nothing inserted at all → 409 only when every validated
    // member was a conflict and nothing else went wrong (upstream's
    // `all_conflicts` classification), 400 otherwise — `detail` is a
    // dict either way.
    if added.is_empty() && input_count > 0 {
        let all_conflicts = !conflict_errors.is_empty()
            && conflict_errors.len() == valid.len()
            && member_errors.is_empty()
            && input_dupes == 0;
        return Err(ApiError::JsonDetail(
            if all_conflicts {
                StatusCode::CONFLICT
            } else {
                StatusCode::BAD_REQUEST
            },
            json!({
                "message": "No items were added to the database.",
                "summary": bulk_summary(
                    input_count,
                    order.len(),
                    valid.len(),
                    validation_error_count,
                    conflict_errors.len(),
                    database_error_count,
                ),
                "validation_errors": validation_grouped,
                "conflict_errors": conflict_errors,
            }),
        ));
    }
    let docs: Vec<serde_json::Value> = added
        .iter()
        .map(|id| item_doc(state, &by_id[id].0, scope))
        .collect();
    let mut parts = vec![format!(
        "Processed {input_count} items: {} added",
        added.len()
    )];
    if input_dupes > 0 {
        parts.push(format!("{input_dupes} input duplicates"));
    }
    if validation_error_count > 0 {
        parts.push(format!("{validation_error_count} validation errors"));
    }
    if admission_error_count > 0 {
        parts.push(format!("{admission_error_count} preprocessing skipped"));
    }
    if !conflict_errors.is_empty() {
        parts.push(format!("{} conflicts", conflict_errors.len()));
    }
    if database_error_count > 0 {
        parts.push(format!("{database_error_count} database errors"));
    }
    Ok((
        StatusCode::CREATED,
        geo_json(serde_json::json!({
            "type": "FeatureCollection",
            "features": docs,
            "conflict_errors": conflict_errors,
            "errors": member_errors
                .iter()
                .map(|(id, msg)| json!({"id": id, "msg": msg}))
                .collect::<Vec<_>>(),
            "message": parts.join(" | "),
        })),
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
    StacJson(body): StacJson<serde_json::Value>,
) -> Result<impl IntoResponse, ApiError> {
    if body["id"].as_str() != Some(item_id.as_str()) {
        return Err(ApiError::BadRequest(format!(
            "body id '{}' does not match path '{item_id}'",
            body["id"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| body["id"].to_string())
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
            // Validate the raw document like the create path — serde
            // normalization heals some schema violations before the
            // schema sees them.
            validate_item_write(
                &state,
                &item_validation_doc(&state, &body, &collection_id, None),
                &item_id,
            )
            .await?;
            let mut item: Item = serde_json::from_value(body)
                .map_err(|e| ApiError::BadRequest(format!("invalid item: {e}")))?;
            validate_item_collection(&mut item, &collection_id, &state.base_url)?;
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
            validate_item_write(
                &state,
                &item_validation_doc(&state, &doc, &collection_id, None),
                &item_id,
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
        return Err(ApiError::PreconditionRequired(
            "A bulk upsert cannot carry If-Match, which REQUIRE_IF_MATCH requires for updates; use method 'insert', or PUT each item with If-Match.".to_string(),
        ));
    }
    let items = body
        .get("items")
        .and_then(serde_json::Value::as_object)
        .cloned()
        .ok_or_else(|| ApiError::BadRequest("body missing 'items' object".into()))?;
    let raise_on_error = state.raise_on_bulk_error;
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
        return Err(ApiError::JsonDetail(
            StatusCode::BAD_REQUEST,
            json!({
                "message": format!("Bulk insertion rejected. {} items are malformed.", errors.len()),
                "errors": errors,
            }),
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
    let processed_count = unique.len();
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
    if raise_on_error && !errors.is_empty() {
        return Err(ApiError::JsonDetail(
            StatusCode::BAD_REQUEST,
            json!({
                "message": format!("Bulk insertion rejected. {} items are malformed.", errors.len()),
                "errors": errors,
            }),
        ));
    }
    // `buffered` keeps `validators.len()` validations in flight,
    // results in order. Schema failures are per-item errors like the
    // other malformed admissions; a fetch failure is environmental —
    // abort.
    let docs: Vec<serde_json::Value> = candidates
        .iter()
        .map(|(raw, _)| item_validation_doc(&state, raw, &collection_id, None))
        .collect();
    let validations = futures_util::stream::iter(docs)
        .map(|doc| validate_member_doc(&state, doc))
        .buffered(state.validators.len().max(1))
        .collect::<Vec<_>>()
        .await;
    let mut prepared: Vec<(String, String, serde_json::Value)> = Vec::new();
    let mut validation_error_count = 0usize;
    for ((raw, item), result) in candidates.into_iter().zip(validations) {
        match result {
            Ok(()) => {
                let doc =
                    serde_json::to_value(&item).map_err(|e| ApiError::Internal(e.to_string()))?;
                prepared.push((item_key(&item.id, &collection_id), item.id.clone(), doc));
            }
            Err(ApiError::BadRequest(msg)) => {
                validation_error_count += 1;
                errors.push(json!({"id": raw["id"], "msg": msg}));
            }
            Err(e) => return Err(e),
        }
    }
    // `MAX_BATCH_ERROR_SIZE` — unconditional fast-fail cutoff (upstream:
    // a payload this toxic stops the loop even when errors would
    // otherwise be skipped).
    if let Some(max) = state.max_batch_error_size {
        if validation_error_count > max {
            return Err(ApiError::JsonDetail(
                StatusCode::BAD_REQUEST,
                json!({
                    "message": format!("Batch rejected. Validation error threshold exceeded ({validation_error_count} errors found). Halting validation loop."),
                    "summary": bulk_summary(
                        admitted.len(),
                        processed_count,
                        prepared.len(),
                        validation_error_count,
                        0,
                        0,
                    ),
                    "errors": errors_grouped_by_msg(&errors),
                }),
            ));
        }
    }
    if raise_on_error && validation_error_count > 0 {
        return Err(ApiError::JsonDetail(
            StatusCode::BAD_REQUEST,
            json!({
                "message": format!("Bulk insertion rejected. {validation_error_count} items failed validation."),
                "summary": bulk_summary(
                    admitted.len(),
                    processed_count,
                    prepared.len(),
                    validation_error_count,
                    0,
                    0,
                ),
                "errors": errors_grouped_by_msg(&errors),
            }),
        ));
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
                    return Err(ApiError::JsonDetail(
                        StatusCode::CONFLICT,
                        json!(format!(
                            "Item '{id}' already exists in collection '{collection_id}'"
                        )),
                    ));
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
