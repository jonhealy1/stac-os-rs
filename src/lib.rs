// src/lib.rs
pub mod dto;
pub mod handlers;
pub mod links;
pub mod store;

use axum::{
    routing::{delete, get, post, put},
    Router,
};
use handlers::*;
use std::sync::Arc;

/// Read-only router — the full STAC read surface (landing page, core
/// collections routes, catalogs registry, all search endpoints).
/// Returned unbound; call `.with_state(state)` after merging in your
/// own extension routes.
pub fn read_router() -> Router<Arc<AppState>> {
    Router::new()
        // --- Global Landing Page ---
        .route("/", get(root_landing_page))
        .route("/conformance", get(conformance))
        .route("/api", get(api))
        .route("/sortables", get(sortables))
        // --- Core STAC item search (registry-wide; same as /catalogs/search) ---
        .route(
            "/search",
            get(catalogs_search_get).post(catalogs_search_post),
        )
        // --- Core STAC collections routes (global namespace) ---
        .route("/collections", get(list_collections))
        .route("/collections/{collection_id}", get(get_collection))
        .route(
            "/collections/{collection_id}/sortables",
            get(collection_sortables),
        )
        .route(
            "/collections/{collection_id}/items",
            get(list_collection_items),
        )
        .route(
            "/collections/{collection_id}/items/{item_id}",
            get(get_collection_item),
        )
        // --- Registry & Management Plane ---
        .route("/catalogs", get(list_catalogs))
        .route("/catalogs/{catalog_id}", get(get_catalog))
        .route("/catalogs/{catalog_id}/catalogs", get(list_sub_catalogs))
        .route(
            "/catalogs/{catalog_id}/conformance",
            get(catalog_conformance),
        )
        // --- Children & Sub-Resources ---
        .route("/catalogs/{catalog_id}/children", get(get_catalog_children))
        // --- Scoped Collections & Items ---
        .route(
            "/catalogs/{catalog_id}/collections",
            get(list_scoped_collections),
        )
        .route(
            "/catalogs/{catalog_id}/collections/{collection_id}",
            get(get_scoped_collection),
        )
        .route(
            "/catalogs/{catalog_id}/collections/{collection_id}/items",
            get(list_scoped_items),
        )
        .route(
            "/catalogs/{catalog_id}/collections/{collection_id}/items/{item_id}",
            get(get_scoped_item),
        )
        // --- Scoped Search Engine ---
        // /catalogs/search = whole-registry scope (everything under root)
        .route(
            "/catalogs/search",
            get(catalogs_search_get).post(catalogs_search_post),
        )
        .route(
            "/catalogs/{catalog_id}/search",
            get(scoped_search_get).post(scoped_search_post),
        )
}

/// Transaction router — mutating routes. `build_app` merges this only
/// when `state.enable_transactions` is set; embedders may merge it
/// selectively themselves (e.g. behind auth middleware).
pub fn transaction_router() -> Router<Arc<AppState>> {
    Router::new()
        // Core transaction routes (STAC Transaction extension /
        // OGC API Features Part-4 style) on the canonical surface
        .route("/collections", post(create_root_collection))
        .route(
            "/collections/{collection_id}",
            put(update_root_collection)
                .delete(delete_root_collection)
                .patch(patch_root_collection),
        )
        .route("/collections/{collection_id}/items", post(create_root_item))
        .route(
            "/collections/{collection_id}/items/{item_id}",
            put(update_root_item)
                .delete(delete_root_item)
                .patch(patch_root_item),
        )
        // Catalog-extension transaction routes (scoped surface)
        .route("/catalogs", post(create_root_catalog))
        .route(
            "/catalogs/{catalog_id}",
            put(update_catalog).delete(disband_catalog),
        )
        .route(
            "/catalogs/{catalog_id}/catalogs",
            post(link_or_create_sub_catalog),
        )
        .route(
            "/catalogs/{catalog_id}/catalogs/{sub_id}",
            delete(unlink_sub_catalog),
        )
        .route(
            "/catalogs/{catalog_id}/collections",
            post(link_or_create_scoped_collection),
        )
        .route(
            "/catalogs/{catalog_id}/collections/{collection_id}",
            put(update_scoped_collection).delete(unlink_scoped_collection),
        )
        .route(
            "/catalogs/{catalog_id}/collections/{collection_id}/items",
            post(create_scoped_item),
        )
        .route(
            "/catalogs/{catalog_id}/collections/{collection_id}/items/{item_id}",
            put(update_scoped_item).delete(delete_scoped_item),
        )
}

/// Build the API router. Mutating routes are only mounted when
/// `state.enable_transactions` is set (ENABLE_TRANSACTIONS_EXTENSIONS).
pub fn build_app(state: Arc<AppState>) -> Router {
    let mut app = read_router();
    if state.enable_transactions {
        app = app.merge(transaction_router());
    }
    app.with_state(state)
}
