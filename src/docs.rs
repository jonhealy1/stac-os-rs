//! OpenAPI document — generated from `#[utoipa::path]` annotations on the
//! handlers, replacing the hand-maintained `openapi.json`. Route params can
//! no longer drift out of the doc: an endpoint exists in the spec iff it is
//! annotated here.

use utoipa::OpenApi;

use crate::handlers;

#[derive(OpenApi)]
#[openapi(
    info(
        title = "stac-os-rs — STAC API with Multi-Tenant Catalogs",
        description = "STAC API implementing the Multi-Tenant Catalogs extension, backed by OpenSearch. Transaction routes exist only when ENABLE_TRANSACTIONS_EXTENSIONS is set.",
        version = "0.3.0",
        license(name = "MIT")
    ),
    paths(
        handlers::root_landing_page,
        handlers::conformance,
        handlers::api,
        handlers::mgmt_ping,
        handlers::sortables,
        handlers::get_queryables,
        handlers::get_collection_queryables,
        handlers::collection_sortables,
        handlers::collections_sortables,
        handlers::catalog_conformance,
        handlers::list_collections,
        handlers::create_root_collection,
        handlers::get_collection,
        handlers::update_root_collection,
        handlers::delete_root_collection,
        handlers::patch_root_collection,
        handlers::collections_search_get,
        handlers::collections_search_post,
        handlers::list_collection_items,
        handlers::create_root_item,
        handlers::get_collection_item,
        handlers::update_root_item,
        handlers::delete_root_item,
        handlers::patch_root_item,
        handlers::bulk_create_items,
        handlers::list_catalogs,
        handlers::create_root_catalog,
        handlers::get_catalog,
        handlers::update_catalog,
        handlers::disband_catalog,
        handlers::get_catalog_children,
        handlers::list_sub_catalogs,
        handlers::link_or_create_sub_catalog,
        handlers::unlink_sub_catalog,
        handlers::list_scoped_collections,
        handlers::link_or_create_scoped_collection,
        handlers::get_scoped_collection,
        handlers::update_scoped_collection,
        handlers::unlink_scoped_collection,
        handlers::list_scoped_items,
        handlers::create_scoped_item,
        handlers::get_scoped_item,
        handlers::update_scoped_item,
        handlers::delete_scoped_item,
        handlers::catalogs_search_get,
        handlers::catalogs_search_post,
        handlers::search_get,
        handlers::search_post,
        handlers::scoped_search_get,
        handlers::scoped_search_post,
    ),
    tags(
        (name = "metadata", description = "Service metadata"),
        (name = "schemas", description = "Queryables/sortables JSON Schemas"),
        (name = "collections", description = "Collection CRUD, listing, and search"),
        (name = "items", description = "Item CRUD and listing"),
        (name = "catalogs", description = "Multi-tenant catalog registry and DAG edges"),
        (name = "search", description = "Item search"),
    )
)]
pub struct ApiDoc;

/// The rendered OpenAPI document for a given deployment configuration,
/// with `info.version` synced from `CARGO_PKG_VERSION` (the derive macro
/// can't read env vars). Paths are pruned to match `build_app`: non-GET
/// operations go away when transactions are off, and the
/// collections-search routes follow their flags — the spec never
/// advertises endpoints that aren't mounted.
pub fn openapi(
    transactions_enabled: bool,
    collections_search_route: bool,
    collections_search: bool,
) -> utoipa::openapi::OpenApi {
    let mut doc = ApiDoc::openapi();
    doc.info.version = env!("CARGO_PKG_VERSION").to_string();
    if !transactions_enabled {
        for item in doc.paths.paths.values_mut() {
            item.put = None;
            item.post = None;
            item.delete = None;
            item.patch = None;
            item.options = None;
            item.head = None;
            item.trace = None;
            item.query = None;
            item.additional_operations.clear();
        }
        doc.paths.paths.retain(|_, item| item.get.is_some());
    }
    if !collections_search_route {
        doc.paths.paths.remove("/collections-search");
    }
    if !collections_search && !collections_search_route {
        doc.paths.paths.remove("/collections-sortables");
    }
    doc
}
