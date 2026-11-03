// src/handlers/search.rs
use super::common::*;
use crate::store::{
    search_offset, CATALOGS_INDEX, DEFAULT_SEARCH_LIMIT, MAX_SEARCH_LIMIT, ROOT_CATALOG_ID,
};
use axum::{
    extract::{Path, Query, State},
    response::IntoResponse,
};
use serde_json::json;
use stac::{Bbox, Link};
use stac_api::{GetSearch, ItemCollection, Search};
use std::collections::HashMap;
use std::sync::Arc;

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
