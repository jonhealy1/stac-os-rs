// src/handlers/metadata.rs
use super::common::*;
use crate::store::{NodeKind, CATALOGS_INDEX, COLLECTIONS_INDEX, ITEMS_INDEX, ROOT_CATALOG_ID};
use axum::{
    extract::{Path, State},
    http::header,
    response::IntoResponse,
    Json,
};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;

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
pub(crate) fn all_queryables() -> serde_json::Map<String, serde_json::Value> {
    let mut q = default_queryables();
    q.insert(
        "platform".to_string(),
        json!({"$enum": true, "description": "Satellite platform identifier"}),
    );
    q
}

/// JSON Schema document served by the queryables endpoints.
pub(crate) fn queryables_schema(
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

pub(crate) fn json_schema(body: serde_json::Value) -> impl IntoResponse {
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
pub async fn api(State(state): State<Arc<AppState>>) -> Result<impl IntoResponse, ApiError> {
    let doc = crate::docs::openapi(
        state.enable_transactions,
        state.enable_collections_search_route,
        state.enable_collections_search,
    );
    let body = doc
        .to_json()
        .map_err(|e| ApiError::Internal(format!("OpenAPI serialization failed: {e}")))?;
    Ok((
        [(
            header::CONTENT_TYPE,
            "application/vnd.oai.openapi+json;version=3.0",
        )],
        body,
    ))
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
        "title": "stac-os-rs — STAC API with Multi-Tenant Catalogs",
        "description": "Multi-tenant STAC API backed by OpenSearch. Root catalog of the registry.",
        "conformsTo": conformance_classes(&state),
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
pub(crate) async fn index_mappings(
    state: &AppState,
    logical: &str,
) -> Result<serde_json::Value, ApiError> {
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
pub(crate) async fn items_sortable_fields(
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

/// GET /catalogs/{catalog_id}/queryables — upstream's catalog-scoped
/// queryables. Catalog scope shares the single items index, so the
/// schema matches the root queryables base (no per-collection enum
/// fill — catalog scope can't narrow the terms aggregation).
#[utoipa::path(
    get,
    path = "/catalogs/{catalog_id}/queryables",
    tag = "schemas",
    summary = "Catalog queryables schema",
    params(("catalog_id" = String, Path, description = "Catalog id")),
    responses(
        (status = 200, description = "Successful response"),
        (status = 400, description = "Invalid request"),
        (status = 404, description = "Not found")
    )
)]
pub async fn get_catalog_queryables(
    Path(catalog_id): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, ApiError> {
    require_catalog(&state, &catalog_id).await?;
    Ok(json_schema(queryables_schema(
        &state,
        format!("/catalogs/{catalog_id}/queryables"),
        "Catalog Queryables",
        all_queryables(),
    )))
}
