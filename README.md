# stac-os-rs

![StacLabs](https://github.com/StacLabs/.github/raw/main/profile/staclabs-orange-banner.png)

A STAC API Opensearch server built with Rust

## Contents

- [What is this?](#what-is-this)
- [API Routes](#api-routes)
  - [Read surface — always mounted](#read-surface--always-mounted)
  - [Transactions](#transactions--require-enable_transactions_extensions)
  - [Planned](#planned--currently-404405)
- [Getting Started (coming from Python?)](#getting-started-coming-from-python)
- [Extending (custom routes)](#extending-custom-routes)
- [Sample Data](#sample-data)
- [TODO](#todo)

## What is this?

A **hybrid** of the STAC API core spec and the [multi-tenant-catalogs extension](https://github.com/StacLabs/multi-tenant-catalogs): catalogs form a poly-hierarchy (a DAG — a catalog or collection can live under multiple parents), and every STAC capability is exposed *scoped* under `/catalogs/{catalog_id}` rather than only at the API root. See [API Routes](#api-routes) for the full surface.

The catalog DAG is stored in OpenSearch as one `{kind, parents}` document per node (`stac-hierarchy` index) — children are derived via `term` queries on `parents`, so linking a shared resource is a single-document write, and orphans are automatically adopted under `root`.

**Beyond the spec:** this project also exposes full STAC *transaction* capabilities inside the `/catalogs` scope (create/update/delete catalogs, collections, and items). That goes beyond what the multi-tenant-catalogs extension currently specifies — we're prototyping it here with the intent of feeding it back into the spec. All write endpoints are gated behind a flag:

```bash
ENABLE_TRANSACTIONS_EXTENSIONS=true   # unsets → every mutating route returns 405
```

When unset, only the read surface is mounted and the landing page `conformsTo` omits the transaction conformance URIs.

## API Routes

### Read surface — always mounted

**Core STAC routes**

| Method | Path | Supports |
|---|---|---|
| GET | `/` | Landing page (`conformsTo`, `child` links per root node, `search`/`data`/`conformance`/`service-desc` links) |
| GET | `/conformance` | OGC conformance class URIs |
| GET | `/api` | OpenAPI service description (`service-desc` link target) |
| GET | `/sortables` | OGC Sortables schema for `sortby` discovery |
| GET/POST | `/search` | Registry-wide item search (same handler as `/catalogs/search`). Filters: `collections`, `ids`, `bbox`, `intersects`, `datetime`, `sortby`, `limit`, `offset`, `fields` → `next`/`prev` links |
| GET | `/collections` | `?limit=&token=` paging — every collection in the registry |
| GET | `/collections/{collection_id}` | Canonical links (`self`, `parent` per parent, `duplicate`) |
| GET | `/collections/{collection_id}/sortables` | Per-collection Sortables schema (`ogcapi-features-5` binding) |
| GET | `/collections/{collection_id}/items` | `?limit=&token=` paging; `?sortby=±field`; `?fields=` filtering; `application/geo+json`, item `self`/`parent`/`collection`/`root` links |
| GET | `/collections/{collection_id}/items/{item_id}` | `application/geo+json` |

**Multi-tenant catalogs routes**

| Method | Path | Supports |
|---|---|---|
| GET | `/catalogs` | `?limit=&token=` paging (default 10) |
| GET | `/catalogs/{catalog_id}` | Dynamic links (`parent` per parent, `children`, `data`, `search`) |
| GET | `/catalogs/{catalog_id}/catalogs` | `?limit=&token=` paging |
| GET | `/catalogs/{catalog_id}/conformance` | Scoped `conformsTo` classes |
| GET | `/catalogs/{catalog_id}/children` | `?type=Catalog\|Collection` filter, `?limit=&token=` paging |
| GET | `/catalogs/{catalog_id}/collections` | `?limit=&token=` paging |
| GET | `/catalogs/{catalog_id}/collections/{collection_id}` | Contextual `self`/`parent`, alt parents as `related`/`duplicate` |
| GET | `/catalogs/{catalog_id}/collections/{collection_id}/items` | `?limit=&token=` paging; `?sortby=±field`; `?fields=` filtering; `application/geo+json`, scoped item links |
| GET | `/catalogs/{catalog_id}/collections/{collection_id}/items/{item_id}` | `application/geo+json` |
| GET/POST | `/catalogs/search` | Whole-registry scope. Same filters as `/search` |
| GET/POST | `/catalogs/{catalog_id}/search` | Same filters, intersected with the catalog's descendant collections |

GET search takes `bbox`, `datetime`, `ids`, `collections`, `limit`, `sortby` (`+field`/`-field` shorthand), `fields` (comma-separated, `-` prefix excludes) as query params; POST takes the full `Search` body (`intersects` and `fields: {include, exclude}` object included). A bbox crossing the antimeridian (`xmin > xmax`) is supported — internally rewritten as a ±180°-split `MultiPolygon` intersects query. `query` and CQL2 `filter` are parsed but not yet applied.

`fields` implements the [Fields extension](https://github.com/stac-api-extensions/fields): include/exclude with nested paths (`properties.datetime`) and `*` wildcards (`properties.eo:*`); when an include selection matches nothing, items fall back to `{id, collection}` per spec.

### Transactions — require `ENABLE_TRANSACTIONS_EXTENSIONS`

**Core transactions** — canonical `/collections` surface (STAC Transaction extension / OGC API Features Part-4 style)

| Method | Path | Supports |
|---|---|---|
| POST | `/collections` | Create under root; `409` on id collision |
| PUT | `/collections/{collection_id}` | Update; `400` id mismatch, `404` missing; DAG memberships preserved |
| DELETE | `/collections/{collection_id}` | Delete collection **and its items**; `404` if missing |
| POST | `/collections/{collection_id}/items` | Create; `404` missing collection, `409` repost, `400` collection mismatch |
| PUT | `/collections/{collection_id}/items/{item_id}` | Update; `404` missing item/collection, `400` id mismatch |
| DELETE | `/collections/{collection_id}/items/{item_id}` | Delete; `404` unless item is in this collection |

**Catalog transactions** — Multi-Tenant Catalogs extension (scoped, poly-hierarchy)

| Method | Path | Supports |
|---|---|---|
| POST | `/catalogs` | Create a root-level catalog; `409` on id collision |
| PUT | `/catalogs/{catalog_id}` | Update; body `id` must match path |
| DELETE | `/catalogs/{catalog_id}` | Disband — direct children adopted by `root`, never cascaded |
| POST | `/catalogs/{catalog_id}/catalogs` | Mode A full-body create (`201`) or Mode B link `{"id": ...}` (`200`); `404` link target missing, `409` repost |
| DELETE | `/catalogs/{catalog_id}/catalogs/{sub_id}` | Unlink edge only; `404` if not a child |
| POST | `/catalogs/{catalog_id}/collections` | Mode A create / Mode B link — same codes as sub-catalogs |
| PUT | `/catalogs/{catalog_id}/collections/{collection_id}` | Update in place — all DAG memberships preserved |
| DELETE | `/catalogs/{catalog_id}/collections/{collection_id}` | Unlink edge only; `404` if not a child (does **not** delete the collection) |
| POST | `/catalogs/{catalog_id}/collections/{collection_id}/items` | Create; `400` if body `collection` contradicts path |
| PUT | `/catalogs/{catalog_id}/collections/{collection_id}/items/{item_id}` | Update; same collection check |
| DELETE | `/catalogs/{catalog_id}/collections/{collection_id}/items/{item_id}` | Delete; `404` unless in this collection |

Scoped reads 404 when the target collection isn't inside the catalog's DAG. Item payloads whose `collection` field contradicts the path get a 400.

Status codes: `201` create (Mode A), `200` link (Mode B `{"id": ...}`) / update, `204` delete/unlink, `400` bad request (`limit=0`, id mismatch, invalid search), `404` missing resource or edge, `409` id collision / full-body repost.

### Planned — currently 404/405

| Method | Path | Status |
|---|---|---|
| GET | `/collections/{id}/queryables` | 404 |
| POST | `/catalogs/{id}/bulk` | 404 — bulk transactions extension |

## Getting Started (coming from Python?)

Rust projects don't use `pip` or virtualenvs — dependencies live in `Cargo.toml` (like `pyproject.toml`) and are fetched automatically by `cargo`, the Rust build/package tool.

**1. Install the Rust toolchain** (gives you `cargo`, think "pip + interpreter in one"):

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source ~/.cargo/env
cargo --version   # verify install
```

**2. Start the stack** — `compose.yml` runs the API, a single-node OpenSearch dev cluster, and Dashboards:

```bash
docker compose up -d          # API :3000, OpenSearch :9200, Dashboards :5601
```

**3. Or run the API locally** — iterate on Rust code while OpenSearch stays in Docker:

```bash
docker compose up -d opensearch
cargo run     # API on http://localhost:3000
```

Config via env vars: `OPENSEARCH_URL` (default `http://localhost:9200`), `ENABLE_TRANSACTIONS_EXTENSIONS` (enables all write endpoints; set in `compose.yml` by default), `CATALOGS_HIDE_ALTERNATE_PARENTS` (suppresses `related`/`duplicate` links and extra `parent` links on poly-hierarchy resources).

Other handy commands: `cargo check` (fast type-check, no binary), `cargo test` (unit + integration tests — integration tests need OpenSearch running, and skip automatically if it's not), `cargo add <crate>` (add a dependency).

Or use the **Makefile**, which handles the Docker dependency for you (`make test` / `make test-integration` start OpenSearch and wait for it to be healthy before running tests): `make up`, `make test`, `make test-integration`, `make test-upstream` (vendored upstream API suite against `localhost:3000`), `make ingest`, `make reset` (wipes data volumes), `make down`.

**Spec conformance:** `make validate` runs [`stac-api-validator`](https://github.com/stac-utils/stac-api-validator) (`pip install stac-api-validator` first) — needs the API running with `make up` and data via `make ingest`. Checks core, item-search, features, and browseable classes.

## Extending (custom routes)

`stac-os-rs` is usable as a library — the router is composable axum, so consumers merge their own extension routes sharing the same `AppState` (store, link engine, config). Three seams:

- `stac_os_rs::read_router()` / `stac_os_rs::transaction_router()` — mount selectively (e.g. transactions behind auth middleware), or `build_app(state)` for the whole surface
- `handlers::AppState` fields are public — your handlers can take `State<Arc<AppState>>` and call `state.store.*` directly
- Standard axum composition: `.merge()` extra routes, `.layer()` middleware

```rust
use axum::{extract::State, routing::get, Json, Router};
use serde_json::json;
use stac_os_rs::{handlers::AppState, read_router, store::ROOT_CATALOG_ID};
use std::sync::Arc;

async fn stats(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let children = state.store.get_children(ROOT_CATALOG_ID).await.unwrap_or_default();
    Json(json!({ "root_children": children.len() }))
}

let state = Arc::new(AppState { /* store, links, base_url, flags */ });
let app = read_router()
    .merge(Router::new().route("/stats", get(stats)))
    .with_state(state);
```

Anything middleware-shaped (auth, CORS, rate-limiting) goes through `.layer()` — no fork needed.

## Sample Data

`sample_data/` contains a demo hierarchy — folder structure mirrors the DAG:

```
earth-observation/          # root catalog
├── landsat/                # sub-catalog
│   └── landsat-c2-l2/      # collection + 2 items
└── sentinel/
    └── sentinel-2-l2a/     # collection + 2 items
climate-research/           # root catalog
└── era5/
    └── era5-daily/         # collection + 2 items
```

Load it (requires `ENABLE_TRANSACTIONS_EXTENSIONS`, on by default in `compose.yml`):

```bash
python3 scripts/ingest_sample_data.py        # STAC_API_URL to override :3000
```

Then try scoped search: `POST /catalogs/earth-observation/search` returns its 4 satellite items; `POST /catalogs/search` searches the whole registry.

## TODO

This is a prototype — the endpoints exist but several are shallow. Known gaps:

**Search**
- `collections`, `ids`, `bbox` (incl. antimeridian crossing), `intersects`, `datetime`, `sortby`, `limit`, and `fields` are applied — `query` and CQL2 `filter` are parsed but ignored
- `sortby` works on `/search` + `/catalogs/*/search` (item-search binding) and on items listings `?sortby=±field` (features binding); sortables at `/sortables` + `/collections/{id}/sortables`
- `datetime` bounds must be full RFC3339 datetimes — date-only inputs (`2023-06-01`) are rejected at validation
- No CQL2 / filter extension support
- Offset pagination exists: `{"offset": n}` in the POST body or `?offset=` on GET + `next`/`prev` links (`method: POST`, `body`) in search responses; items listings use `?limit=&token=` with `method: GET` `next` links. All paged responses include `numberMatched`/`numberReturned`. Deep paging (>10k) needs `search_after`/cursor — not implemented

**Core STAC routes**
- `GET /queryables` endpoints (root + per-collection) — not implemented
- `service-doc` (human-readable API docs page) — OpenAPI JSON exists at `/api`; no HTML variant
- `src/openapi.json` is hand-maintained and drifts as features land — generate it from the route types instead (`utoipa` or `aide`)

**Hierarchy & data**
- Children/descendants capped at 10k per level — real pagination needed on the DAG itself
- Reserved IDs: a catalog named `search` collides with the static route — should 400 on create
- Orphaned docs: a hierarchy node whose document is missing is silently skipped in children listings and `rel: child` links — needs a consistency check
- User-provided `links` in POST/PUT bodies are replaced by generated links on read (upstream merges non-dynamic user links — a deliberate divergence for now)
- No request-body STAC schema validation yet (`stac-validate` crate planned)

**Ops**
- OpenSearch runs single-node with the security plugin disabled — dev only, harden before anything else
- No auth or per-tenant authorization — catalog scope is organizational only, not a security boundary

**Concurrency** — all writes use optimistic concurrency via OpenSearch `_seq_no`/`_primary_term`: document PUTs re-stamp and retry up to 5 times (`Store::replace_document`/`put_document`), hierarchy link/unlink replay the edge mutation on a fresh read (`Store::mutate_node`), and POSTs use `op_type=create` for atomic repost-409s. Exhausted retries surface as HTTP 409.

**Tests** — `tests/catalogs.rs` ports `stac-fastapi-elasticsearch-opensearch`'s `test_catalogs.py` (118 passing, 2 `#[ignore]`d pending: `stac-validate`, user-link merge semantics). `tests/upstream/` vendors upstream's Python API suite verbatim — `make test-upstream` runs it over HTTP against `localhost:3000` (needs the API up with `ENABLE_TRANSACTIONS_EXTENSIONS`; adapter conftest skips tests needing in-process sfeos internals). CI also runs [`stac-api-validator`](https://github.com/stac-utils/stac-api-validator) (core, item-search, features, browseable — all passing) as an advisory job. Each test gets isolated `it-*` indices (unique `Store` index prefix), so tests never touch dev data — `make test*` sweeps `it-*` afterwards, or `make test-clean` manually.
