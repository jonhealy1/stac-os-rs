// src/handlers/catalogs.rs
use super::common::*;
use crate::dto::CreateOrLinkPayload;
use crate::store::{NodeKind, CATALOGS_INDEX, COLLECTIONS_INDEX, ROOT_CATALOG_ID};
use axum::{
    extract::{Path, Query, RawQuery, State},
    http::{header, StatusCode},
    response::IntoResponse,
    Json,
};
use serde_json::json;
use stac::Catalog;
use std::collections::HashMap;
use std::sync::Arc;

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
        (status = 201, description = "Created"),
        (status = 400, description = "Invalid request"),
        (status = 404, description = "Not found"),
        (status = 409, description = "Resource id already exists or write conflict")
    )
)]
pub async fn create_root_catalog(
    State(state): State<Arc<AppState>>,
    StacJson(mut catalog): StacJson<Catalog>,
) -> Result<impl IntoResponse, ApiError> {
    validate_stac_doc(&state, &catalog).await?;
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
        (status = 409, description = "Resource id already exists or write conflict"),
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
    require_if_match(&state, &headers, &etag_of(&state, CATALOGS_INDEX, &version))?;
    validate_stac_doc(&state, &catalog).await?;
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

    validate_children_response(&children, &links)?;

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
        (status = 201, description = "Created"),
        (status = 400, description = "Invalid request"),
        (status = 404, description = "Not found"),
        (status = 409, description = "Resource id already exists or write conflict")
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
            validate_stac_doc(&state, &new_catalog).await?;
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
        (status = 204, description = "No content"),
        (status = 404, description = "Not found"),
        (status = 409, description = "Resource id already exists or write conflict")
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
    delete,
    path = "/catalogs/{catalog_id}",
    tag = "catalogs",
    summary = "Disband a catalog (unlink children, keep resources)",
    params(("catalog_id" = String, Path, description = "Catalog id"), ("If-Match" = Option<String>, Header, description = "Conditional write: apply only if the resource ETag matches")),
    responses(
        (status = 204, description = "No content"),
        (status = 404, description = "Not found"),
        (status = 409, description = "Resource id already exists or write conflict"),
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
        require_if_match(&state, &headers, &etag_of(&state, CATALOGS_INDEX, version))?;
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
