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
        handlers::metadata::root_landing_page,
        handlers::metadata::conformance,
        handlers::metadata::api,
        handlers::metadata::mgmt_ping,
        handlers::metadata::sortables,
        handlers::metadata::get_queryables,
        handlers::metadata::get_collection_queryables,
        handlers::metadata::collection_sortables,
        handlers::metadata::collections_sortables,
        handlers::metadata::catalog_conformance,
        handlers::metadata::get_catalog_queryables,
        handlers::collections::list_collections,
        handlers::collections::create_root_collection,
        handlers::collections::get_collection,
        handlers::collections::update_root_collection,
        handlers::collections::delete_root_collection,
        handlers::collections::patch_root_collection,
        handlers::collections::collections_search_get,
        handlers::collections::collections_search_post,
        handlers::items::list_collection_items,
        handlers::items::create_root_item,
        handlers::items::get_collection_item,
        handlers::items::update_root_item,
        handlers::items::delete_root_item,
        handlers::items::patch_root_item,
        handlers::items::bulk_create_items,
        handlers::catalogs::list_catalogs,
        handlers::catalogs::create_root_catalog,
        handlers::catalogs::get_catalog,
        handlers::catalogs::update_catalog,
        handlers::catalogs::disband_catalog,
        handlers::catalogs::get_catalog_children,
        handlers::catalogs::list_sub_catalogs,
        handlers::catalogs::link_or_create_sub_catalog,
        handlers::catalogs::unlink_sub_catalog,
        handlers::collections::list_scoped_collections,
        handlers::collections::link_or_create_scoped_collection,
        handlers::collections::get_scoped_collection,
        handlers::collections::update_scoped_collection,
        handlers::collections::unlink_scoped_collection,
        handlers::items::list_scoped_items,
        handlers::items::create_scoped_item,
        handlers::items::get_scoped_item,
        handlers::items::update_scoped_item,
        handlers::items::delete_scoped_item,
        handlers::search::catalogs_search_get,
        handlers::search::catalogs_search_post,
        handlers::search::search_get,
        handlers::search::search_post,
        handlers::search::scoped_search_get,
        handlers::search::scoped_search_post,
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
