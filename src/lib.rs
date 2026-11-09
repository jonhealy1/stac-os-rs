// src/lib.rs
pub mod api_types;
pub mod docs;
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

/// Read-only router — the core STAC read surface (landing page,
/// collections routes, item search), without the catalogs registry.
/// `build_app` merges `catalogs_read_router` when
/// `state.enable_catalogs_route` is set (`ENABLE_CATALOGS_ROUTE`,
/// default on). Returned unbound; call `.with_state(state)` after
/// merging in your own extension routes.
pub fn read_router() -> Router<Arc<AppState>> {
    Router::new()
        // --- Global Landing Page ---
        .route("/", get(root_landing_page))
        .route("/conformance", get(conformance))
        .route("/api", get(api))
        .route("/api.html", get(api_docs_html))
        .route("/docs", get(api_docs_html))
        .route("/_mgmt/ping", get(mgmt_ping))
        .route("/sortables", get(sortables))
        .route("/queryables", get(get_queryables))
        // --- Core STAC item search (registry-wide; same as /catalogs/search) ---
        .route("/search", get(search_get).post(search_post))
        // --- Core STAC collections routes (global namespace) ---
        .route("/collections", get(list_collections))
        .route("/collections/{collection_id}", get(get_collection))
        .route(
            "/collections/{collection_id}/sortables",
            get(collection_sortables),
        )
        .route(
            "/collections/{collection_id}/queryables",
            get(get_collection_queryables),
        )
        .route(
            "/collections/{collection_id}/items",
            get(list_collection_items),
        )
        .route(
            "/collections/{collection_id}/items/{item_id}",
            get(get_collection_item),
        )
}

/// Multi-Tenant Catalogs read surface — merged by `build_app` only
/// when `state.enable_catalogs_route` is set. Upstream gates the same
/// surface behind its opt-in `ENABLE_CATALOGS_ROUTE` (default off
/// there; this port defaults it on).
pub fn catalogs_read_router() -> Router<Arc<AppState>> {
    Router::new()
        // --- Registry & Management Plane ---
        .route("/catalogs", get(list_catalogs))
        .route("/catalogs/{catalog_id}", get(get_catalog))
        .route("/catalogs/{catalog_id}/catalogs", get(list_sub_catalogs))
        .route(
            "/catalogs/{catalog_id}/conformance",
            get(catalog_conformance),
        )
        .route(
            "/catalogs/{catalog_id}/queryables",
            get(get_catalog_queryables),
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

/// Transaction router — mutating routes on the canonical surface.
/// `build_app` merges this only when `state.enable_transactions` is
/// set; catalogs-surface writes live in `catalogs_transaction_router`.
/// Embedders may merge it selectively themselves (e.g. behind auth
/// middleware).
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
            "/collections/{collection_id}/bulk_items",
            post(bulk_create_items),
        )
        .route(
            "/collections/{collection_id}/items/{item_id}",
            put(update_root_item)
                .delete(delete_root_item)
                .patch(patch_root_item),
        )
}

/// Catalog-extension transaction routes (scoped surface). `build_app`
/// merges this only when both `enable_transactions` and
/// `enable_catalogs_route` are set.
pub fn catalogs_transaction_router() -> Router<Arc<AppState>> {
    Router::new()
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
    // Custom collections-search route — opt-in like upstream
    // (`ENABLE_COLLECTIONS_SEARCH_ROUTE`, default off). The search
    // params on GET /collections are gated inside list_collections
    // by `enable_collections_search`.
    if state.enable_collections_search_route {
        app = app.route(
            "/collections-search",
            get(collections_search_get).post(collections_search_post),
        );
    }
    // Collections-search sortables — registered when either collections-
    // search flag is on (upstream gates it on the same condition).
    if state.enable_collections_search || state.enable_collections_search_route {
        app = app.route("/collections-sortables", get(collections_sortables));
    }
    // Multi-Tenant Catalogs extension — opt-out (default on), matching
    // upstream's `ENABLE_CATALOGS_ROUTE` knob but inverted default.
    if state.enable_catalogs_route {
        app = app.merge(catalogs_read_router());
    }
    if state.enable_transactions {
        app = app.merge(transaction_router());
        if state.enable_catalogs_route {
            app = app.merge(catalogs_transaction_router());
        }
    }
    app.with_state(state)
        // Compression first so it runs innermost on the way out; the
        // etag layer is outermost and sees the final Content-Encoding.
        // Upstream's BrotliMiddleware only compresses bodies >= 400
        // bytes (`minimum_size`) — small single-resource responses stay
        // uncompressed and keep their plain strong ETag.
        .layer(
            tower_http::compression::CompressionLayer::new().compress_when({
                use tower_http::compression::predicate::{DefaultPredicate, Predicate, SizeAbove};
                DefaultPredicate::new().and(SizeAbove::new(400))
            }),
        )
        .layer(axum::middleware::from_fn(etag_content_coding))
        // Permissive CORS — matches upstream's CORSMiddleware defaults and
        // lets browser SPAs (STAC Browser) hit the API cross-origin.
        // Outermost layer so it answers OPTIONS preflights itself.
        .layer(tower_http::cors::CorsLayer::permissive())
}

/// Response-side ETag finishing: when the body is content-encoded
/// (gzip/br), append `-<coding>` inside the ETag quotes — a tag shared
/// by differently-encoded bodies would be weak (RFC 9110 §8.8.1).
async fn etag_content_coding(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let mut resp = next.run(req).await;
    let coding = resp
        .headers()
        .get(axum::http::header::CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    if !coding.is_empty() && coding != "identity" {
        if let Some(value) = resp.headers().get(axum::http::header::ETAG) {
            if let Ok(tag) = value.to_str() {
                if let Some(inner) = tag.strip_suffix('"') {
                    let tagged = format!("{inner}-{coding}\"");
                    if let Ok(v) = axum::http::HeaderValue::from_str(&tagged) {
                        resp.headers_mut().insert(axum::http::header::ETAG, v);
                    }
                }
            }
        }
    }
    resp
}
