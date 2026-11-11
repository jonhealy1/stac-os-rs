// Shared helpers for integration tests against a real OpenSearch.
#![allow(dead_code)]

use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
    Router,
};
use serde_json::{json, Value};
use stac_os_rs::{
    build_app,
    handlers::AppState,
    links::LinkEngine,
    store::{NodeKind, Store, COLLECTIONS_INDEX, ITEMS_INDEX, ROOT_CATALOG_ID},
};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn uniq(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{prefix}-{nanos}")
}

/// None if OpenSearch is unreachable — caller should skip the test.
pub async fn test_state(enable_transactions: bool) -> Option<Arc<AppState>> {
    test_state_full(enable_transactions, false).await
}

/// Full control over feature flags (transactions, hide_alternate_parents).
///
/// Each call gets a unique `it-<nanos>` index prefix — every test runs
/// against isolated, empty indices. `make test*` sweeps `it-*` after the
/// run (`curl -XDELETE "$OPENSEARCH_URL/it-*"` for manual cleanup).
/// Feature-flag bundle for `test_state_opts` — mirrors the AppState
/// env knobs so tests can compose app variants without env mutation
/// (env reads are racy under parallel test runs).
#[derive(Clone, Copy, Default)]
pub struct TestFlags {
    /// `ENABLE_CATALOGS_ROUTE=false` — catalogs surface unmounted.
    /// Named for the action (disable) so the bool default keeps the
    /// flag's on-by-default production behavior.
    pub disable_catalogs_route: bool,
    pub enable_validator: bool,
    pub require_if_match: bool,
    pub raise_on_bulk_error: bool,
    pub enable_topology_validation: bool,
    pub max_topology_vertices: usize,
    pub max_batch_error_size: Option<usize>,
}

pub async fn test_state_full(
    enable_transactions: bool,
    hide_alternate_parents: bool,
) -> Option<Arc<AppState>> {
    test_state_opts(
        enable_transactions,
        hide_alternate_parents,
        TestFlags::default(),
    )
    .await
}

/// `ENABLE_STAC_VALIDATOR=true` variant — write bodies are schema-validated.
pub async fn test_state_validated() -> Option<Arc<AppState>> {
    test_state_opts(
        true,
        false,
        TestFlags {
            enable_validator: true,
            ..Default::default()
        },
    )
    .await
}

/// `ENABLE_STAC_VALIDATOR` + `REQUIRE_IF_MATCH` variant.
pub async fn test_app_validated_if_match() -> Option<(Router, Arc<AppState>)> {
    let state = test_state_opts(
        true,
        false,
        TestFlags {
            enable_validator: true,
            require_if_match: true,
            ..Default::default()
        },
    )
    .await?;
    Some((build_app(state.clone()), state))
}

/// Build an app from explicit flags — for the validation/batch knobs
/// that don't warrant their own helper.
pub async fn test_app_flags(flags: TestFlags) -> Option<(Router, Arc<AppState>)> {
    let state = test_state_opts(true, false, flags).await?;
    Some((build_app(state.clone()), state))
}

async fn test_state_opts(
    enable_transactions: bool,
    hide_alternate_parents: bool,
    flags: TestFlags,
) -> Option<Arc<AppState>> {
    let url =
        std::env::var("OPENSEARCH_URL").unwrap_or_else(|_| "http://localhost:9200".to_string());
    // Tests write-then-search constantly — pin inline refresh so reads
    // never race the index's refresh interval (production default is
    // `DATABASE_REFRESH=false`, which this deliberately overrides).
    let store = Store::connect(&url)
        .ok()?
        .with_index_prefix(uniq("it"))
        .with_refresh(opensearch::params::Refresh::True);
    // OpenSearch answered but index creation failed — the environment is
    // broken (e.g. a cluster `create_index` block), not absent. Fail loudly
    // rather than letting every test silently pass as "skipped".
    store
        .ensure_indices()
        .await
        .expect("ensure_indices failed — check OpenSearch cluster blocks/settings");
    Some(Arc::new(AppState {
        base_url: "http://test".to_string(),
        store,
        links: LinkEngine::new("http://test"),
        enable_transactions,
        hide_alternate_parents,
        enable_collections_search: true,
        enable_collections_search_route: true,
        enable_catalogs_route: !flags.disable_catalogs_route,
        default_collection_limit: 10,
        max_collection_limit: None,
        default_item_limit: 10,
        max_item_limit: None,
        excluded_item_fields: Vec::new(),
        excluded_queryables: Vec::new(),
        validator: if flags.enable_validator {
            Some(
                stac_validate::Validator::with_client_builder(
                    reqwest::Client::builder().timeout(stac_os_rs::handlers::VALIDATION_TIMEOUT),
                )
                .await
                .expect("validator init failed"),
            )
        } else {
            None
        },
        validation_concurrency: stac_os_rs::handlers::VALIDATOR_POOL_SIZE,
        require_if_match: flags.require_if_match,
        raise_on_bulk_error: flags.raise_on_bulk_error,
        enable_topology_validation: flags.enable_topology_validation,
        max_topology_vertices: if flags.max_topology_vertices == 0 {
            5000
        } else {
            flags.max_topology_vertices
        },
        max_batch_error_size: flags.max_batch_error_size,
    }))
}

pub async fn test_app(enable_transactions: bool) -> Option<(Router, Arc<AppState>)> {
    let state = test_state(enable_transactions).await?;
    Some((build_app(state.clone()), state))
}

pub async fn test_app_validated() -> Option<(Router, Arc<AppState>)> {
    let state = test_state_validated().await?;
    Some((build_app(state.clone()), state))
}

pub async fn test_app_hide_alt() -> Option<(Router, Arc<AppState>)> {
    let state = test_state_full(true, true).await?;
    Some((build_app(state.clone()), state))
}

pub async fn call(
    app: &Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let (status, _, json) = call_headers(app, method, uri, body).await;
    (status, json)
}

/// `call` variant that also returns response headers — for content-type
/// assertions (e.g. `application/schema+json` on queryables).
pub async fn call_headers(
    app: &Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, axum::http::HeaderMap, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.map(|b| b.to_string()).unwrap_or_default()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, headers, json)
}

/// Owned-URI variant of `call` — use for futures held across `join!`s
/// (borrowed `&format!` temporaries can't outlive the joined future).
pub async fn call_owned(
    app: &Router,
    method: &str,
    uri: String,
    body: Option<Value>,
) -> (StatusCode, Value) {
    call(app, method, &uri, body).await
}

use tower::ServiceExt;

/// Equivalent of their `test_catalog.json` fixture (user links included —
/// tests assert user links persist while dynamic links are regenerated).
pub fn catalog(id: &str) -> Value {
    json!({
        "id": id,
        "type": "Catalog",
        "stac_version": "1.0.0",
        "title": "Test Catalog",
        "description": "A test catalog for STAC API testing",
        "links": [
            {"rel": "self", "href": "http://test-server/catalogs/x", "type": "application/json"},
            {"rel": "root", "href": "http://test-server/catalogs", "type": "application/json"},
            {"rel": "child", "href": "http://test-server/collections/placeholder-collection", "type": "application/json", "title": "Placeholder Collection"}
        ]
    })
}

/// Equivalent of their `test_collection.json` fixture (slimmed).
pub fn collection(id: &str) -> Value {
    json!({
        "id": id,
        "type": "Collection",
        "stac_version": "1.0.0",
        "title": "Test Collection",
        "description": "A test collection",
        "license": "PDDL-1.0",
        "extent": {
            "spatial": {"bbox": [[-180.0, -90.0, 180.0, 90.0]]},
            "temporal": {"interval": [["2013-06-01T00:00:00Z", null]]}
        }
    })
}

/// Equivalent of their `test_item.json` fixture (slimmed).
pub fn item(id: &str, collection_id: &str) -> Value {
    json!({
        "type": "Feature",
        "id": id,
        "stac_version": "1.0.0",
        "geometry": {"type": "Point", "coordinates": [150.0, -33.0]},
        "bbox": [149.0, -34.0, 152.0, -32.0],
        "properties": {"datetime": "2020-02-12T12:30:22Z"},
        "collection": collection_id
    })
}

/// The `ctx` fixture equivalent: a collection + item that exist at root
/// level without belonging to any test catalog. Seeded directly via the
/// store (we have no root-level POST /collections route by design).
pub struct Ctx {
    pub collection_id: String,
    pub item_id: String,
}

pub async fn ctx(state: &AppState) -> Ctx {
    let collection_id = uniq("ctx-col");
    let item_id = uniq("ctx-item");
    state
        .store
        .index_document(
            COLLECTIONS_INDEX,
            &collection_id,
            collection(&collection_id),
        )
        .await
        .unwrap();
    state
        .store
        .set_parents(
            &collection_id,
            vec![ROOT_CATALOG_ID.to_string()],
            NodeKind::Collection,
        )
        .await
        .unwrap();
    state
        .store
        .index_document(
            ITEMS_INDEX,
            &format!("{item_id}|{collection_id}"),
            item(&item_id, &collection_id),
        )
        .await
        .unwrap();
    Ctx {
        collection_id,
        item_id,
    }
}

/// Convenience: extract rel -> href list from a doc's links array.
pub fn link_rels(doc: &Value) -> Vec<String> {
    doc["links"]
        .as_array()
        .map(|links| {
            links
                .iter()
                .filter_map(|l| l["rel"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Convenience: all link objects with a given rel.
pub fn links_by_rel(doc: &Value, rel: &str) -> Vec<Value> {
    doc["links"]
        .as_array()
        .map(|links| links.iter().filter(|l| l["rel"] == rel).cloned().collect())
        .unwrap_or_default()
}

/// Assert no href has double slashes outside the scheme.
pub fn assert_no_double_slashes(doc: &Value) {
    for link in doc["links"].as_array().into_iter().flatten() {
        if let Some(href) = link["href"].as_str() {
            let rest = href.replace("http://", "").replace("https://", "");
            assert!(!rest.contains("//"), "URL has double slashes: {href}");
        }
    }
}
