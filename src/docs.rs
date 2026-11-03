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

/// The rendered OpenAPI document, with `info.version` synced from
/// `CARGO_PKG_VERSION` (the derive macro can't read env vars).
pub fn openapi() -> utoipa::openapi::OpenApi {
    static DOC: std::sync::OnceLock<utoipa::openapi::OpenApi> = std::sync::OnceLock::new();
    DOC.get_or_init(|| {
        let mut doc = ApiDoc::openapi();
        doc.info.version = env!("CARGO_PKG_VERSION").to_string();
        doc
    })
    .clone()
}
