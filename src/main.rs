// src/main.rs
use stac_os_rs::{build_app, handlers::AppState, links::LinkEngine, store::Store};
use std::sync::Arc;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let base_url =
        std::env::var("STAC_BASE_URL").unwrap_or_else(|_| "http://localhost:3000".to_string());
    let opensearch_url =
        std::env::var("OPENSEARCH_URL").unwrap_or_else(|_| "http://localhost:9200".to_string());
    let store = Store::connect(&opensearch_url).expect("Failed to connect to OpenSearch");
    store
        .ensure_indices()
        .await
        .expect("Failed to create STAC indices");

    let env_flag = |name: &str| {
        std::env::var(name)
            .map(|v| matches!(v.to_lowercase().as_str(), "true" | "1" | "yes"))
            .unwrap_or(false)
    };
    let enable_transactions = env_flag("ENABLE_TRANSACTIONS_EXTENSIONS");
    let hide_alternate_parents = env_flag("CATALOGS_HIDE_ALTERNATE_PARENTS");
    // Upstream defaults: collections-search on, its custom route off.
    let enable_collections_search = std::env::var("ENABLE_COLLECTIONS_SEARCH")
        .map(|v| !matches!(v.to_lowercase().as_str(), "false" | "0" | "no"))
        .unwrap_or(true);
    let enable_collections_search_route = env_flag("ENABLE_COLLECTIONS_SEARCH_ROUTE");
    let env_u64 = |name: &str| std::env::var(name).ok().and_then(|v| v.parse().ok());

    // `USE_DATETIME` (default true): false = ranged items filter on
    // start/end alone, ignoring `properties.datetime`.
    let use_datetime = std::env::var("USE_DATETIME")
        .map(|v| !matches!(v.to_lowercase().as_str(), "false" | "0" | "no"))
        .unwrap_or(true);
    // `DATABASE_REFRESH` (default `false`, matching upstream sfeos):
    // `false` accepts ~refresh_interval search lag on writes (GET-by-id
    // stays real-time) — the fast ingest setting. `true` refreshes
    // inline before responding; `wait_for` parks until the next tick.
    let refresh = match std::env::var("DATABASE_REFRESH")
        .map(|v| v.to_lowercase())
        .as_deref()
    {
        Ok("true") | Ok("1") | Ok("yes") => opensearch::params::Refresh::True,
        Ok("wait_for") => opensearch::params::Refresh::WaitFor,
        _ => opensearch::params::Refresh::False,
    };
    let store = store.with_use_datetime(use_datetime).with_refresh(refresh);

    let state = Arc::new(AppState {
        base_url: base_url.clone(),
        store,
        links: LinkEngine::new(&base_url),
        enable_transactions,
        hide_alternate_parents,
        enable_collections_search,
        enable_collections_search_route,
        default_collection_limit: env_u64("STAC_DEFAULT_COLLECTION_LIMIT").unwrap_or(10),
        max_collection_limit: env_u64("STAC_GLOBAL_COLLECTION_MAX_LIMIT"),
        default_item_limit: env_u64("STAC_DEFAULT_ITEM_LIMIT").unwrap_or(10),
        max_item_limit: env_u64("STAC_GLOBAL_ITEM_MAX_LIMIT"),
        excluded_item_fields: std::env::var("EXCLUDED_FROM_ITEMS")
            .map(|v| {
                v.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        excluded_queryables: std::env::var("EXCLUDED_FROM_QUERYABLES")
            .map(|v| {
                v.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        validators: match env_flag("ENABLE_STAC_VALIDATOR") {
            true => {
                let mut pool = Vec::with_capacity(stac_os_rs::handlers::VALIDATOR_POOL_SIZE);
                for _ in 0..pool.capacity() {
                    pool.push(tokio::sync::Mutex::new(
                        stac_validate::Validator::new().await.expect(
                            "ENABLE_STAC_VALIDATOR is set but the validator failed to initialize",
                        ),
                    ));
                }
                pool
            }
            false => Vec::new(),
        },
        require_if_match: env_flag("REQUIRE_IF_MATCH"),
    });
    let app = build_app(state);

    let port: u16 = std::env::var("STAC_PORT")
        .ok()
        .map(|v| v.parse().expect("STAC_PORT must be a valid TCP port"))
        .unwrap_or(3000);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .unwrap();
    println!("stac-os-rs API running on http://localhost:{port}");
    if enable_transactions {
        println!("Transaction extension ENABLED (ENABLE_TRANSACTIONS_EXTENSIONS)");
    }

    axum::serve(listener, app).await.unwrap();
}
