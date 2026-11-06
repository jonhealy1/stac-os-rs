// CORS smoke test — no OpenSearch needed: CorsLayer answers preflights
// before routing, so the store is never touched. The preflight is what
// a cross-origin browser SPA (STAC Browser) sends before any request.
use axum::{
    body::Body,
    http::{header, Request, StatusCode},
};
use stac_os_rs::{build_app, handlers::AppState, links::LinkEngine, store::Store};
use std::sync::Arc;
use tower::ServiceExt;

fn state() -> Arc<AppState> {
    Arc::new(AppState {
        base_url: "http://test".to_string(),
        store: Store::connect("http://localhost:9200").unwrap(),
        links: LinkEngine::new("http://test"),
        enable_transactions: false,
        hide_alternate_parents: false,
        enable_collections_search: true,
        enable_collections_search_route: false,
        default_collection_limit: 10,
        max_collection_limit: None,
        default_item_limit: 10,
        max_item_limit: None,
        excluded_item_fields: Vec::new(),
        excluded_queryables: Vec::new(),
    })
}

#[tokio::test]
async fn preflight_allows_cross_origin_spa() {
    let app = build_app(state());
    let req = Request::builder()
        .method("OPTIONS")
        .uri("/collections")
        .header(header::ORIGIN, "http://localhost:8080")
        .header(header::ACCESS_CONTROL_REQUEST_METHOD, "GET")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN], "*");
    let methods = resp.headers()[header::ACCESS_CONTROL_ALLOW_METHODS]
        .to_str()
        .unwrap();
    assert!(methods == "*" || methods.contains("GET"));
}
