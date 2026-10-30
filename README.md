# stac-os-rs
<img src="assets/glass-osiris.png" alt="STAC-OSIRIS" width="400">
<!-- ![StacLabs](https://github.com/StacLabs/.github/raw/main/profile/staclabs-orange-banner.png) -->

A STAC API Opensearch server built with Rust

## Contents

- [What is this?](#what-is-this)
- [API Routes](#api-routes)
  - [Read surface — always mounted](#read-surface--always-mounted)
  - [Transactions](#transactions--require-enable_transactions_extensions)
  - [Planned](#planned--currently-404405)
- [Getting Started (coming from Python?)](#getting-started-coming-from-python)
  - [Configuration Reference](#configuration-reference)
- [Extending (custom routes)](#extending-custom-routes)
- [Sample Data](#sample-data)
- [TODO](#todo)
- [Changelog](CHANGELOG.md)

## What is this?

A **Rust port of [sfeos](https://github.com/stac-utils/stac-fastapi-elasticsearch-opensearch)** — the STAC API implementation backed by OpenSearch — with the same extensions surface (Fields, Query, Filter/CQL2, Sort, Queryables, Sortables, transactions, collections-search). The upstream Python test suite is vendored under `tests/upstream/` and runs against this server over HTTP, so behavior stays compatible.

sfeos also supports the [multi-tenant-catalogs extension](https://github.com/StacLabs/multi-tenant-catalogs), which this project implements too: catalogs form a poly-hierarchy (a DAG — a catalog or collection can live under multiple parents), and every STAC capability is exposed *scoped* under `/catalogs/{catalog_id}` rather than only at the API root. It's an optional extension — the catalogs surface is currently always mounted, with an opt-out flag (`ENABLE_CATALOGS_ROUTE`, like upstream's) coming soon. See [API Routes](#api-routes) for the full surface.

The catalog DAG is stored in OpenSearch as one `{kind, parents}` document per node (`stac-hierarchy` index) — children are derived via `term` queries on `parents`, so linking a shared resource is a single-document write, and orphans are automatically adopted under `root`.

The multi-tenant-catalogs extension includes a **transaction spec**, which this project implements: full create/update/delete of catalogs, collections, and items inside the `/catalogs` scope (`multi-tenant-catalogs/transaction` conformance class). All write endpoints are gated behind a flag:

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
| GET | `/queryables` | OGC Queryables JSON Schema — core item fields (`application/schema+json`) |
| GET/POST | `/search` | Registry-wide item search (same handler as `/catalogs/search`). Filters: `collections`, `ids`, `bbox`, `intersects`, `datetime`, `sortby`, `limit`, `offset`, `fields`, `query`, `filter` → `next`/`prev` links |
| GET | `/collections` | `?limit=&token=` paging — every collection in the registry. Collections-search extensions: `sortby`, `fields`, `q` (free text), `filter` + `filter-lang`, `query`, `datetime`, `bbox` |
| GET/POST | `/collections-search` | Requires `ENABLE_COLLECTIONS_SEARCH_ROUTE`. Same parameter surface as extended `/collections`; POST takes `{sortby:[{field,direction}], fields:{include,exclude}, q, filter, query, datetime, bbox, limit, token}` — `next` links carry `body.token` |
| GET | `/collections-sortables` | Sortables schema for collections-search (mounted when either collections-search flag is on) |
| GET | `/collections/{collection_id}` | Canonical links (`self`, `parent` per parent, `duplicate`, `queryables`) |
| GET | `/collections/{collection_id}/sortables` | Per-collection Sortables schema (`ogcapi-features-5` binding) |
| GET | `/collections/{collection_id}/queryables` | Per-collection Queryables — core fields merged with the items index mapping; `platform.enum` populated from indexed values |
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

GET search takes `bbox`, `datetime`, `ids`, `collections`, `limit`, `sortby` (`+field`/`-field` shorthand), `fields` (comma-separated, `-` prefix excludes), `filter` (cql2-text) as query params; POST takes the full `Search` body (`intersects`, `fields: {include, exclude}`, `query`, `filter` + `filter-lang` — defaults to `cql2-json`). A bbox crossing the antimeridian (`xmin > xmax`) is supported — internally rewritten as a ±180°-split `MultiPolygon` intersects query.

`fields` implements the [Fields extension](https://github.com/stac-api-extensions/fields): include/exclude with nested paths (`properties.datetime`) and `*` wildcards (`properties.eo:*`); when an include selection matches nothing, items fall back to `{id, collection}` per spec.

`query` implements the [Query extension](https://github.com/stac-api-extensions/query) — `{"eo:cloud_cover": {"lt": 10}}` with `eq`/`ne`/`neq`/`gt`/`gte`/`lt`/`lte`/`in`/`contains`; bare property names resolve under `properties.*` (top-level fields like `id`/`collection` pass through).

`filter` implements the [Filter extension](https://github.com/stac-api-extensions/filter) — CQL2 in both `cql2-json` (POST) and `cql2-text` (GET) forms. Coverage: basic-cql2 (comparisons, `and`/`or`/`not`, `IS NULL`), advanced comparisons (`LIKE`, `BETWEEN`, `IN`), and basic spatial predicates (`s_intersects`/`s_within`/`s_contains`/`s_disjoint`). Temporal functions and arithmetic are not yet supported — unsupported operators return 400.

### Transactions — require `ENABLE_TRANSACTIONS_EXTENSIONS`

**Core transactions** — canonical `/collections` surface (STAC Transaction extension / OGC API Features Part-4 style)

| Method | Path | Supports |
|---|---|---|
| POST | `/collections` | Create under root; `409` on id collision |
| PUT | `/collections/{collection_id}` | Update; `400` id mismatch, `404` missing; DAG memberships preserved |
| PATCH | `/collections/{collection_id}` | JSON Merge Patch (`application/merge-patch+json`) or RFC 6902 JSON Patch (`application/json-patch+json`); `400` immutable `id` change |
| DELETE | `/collections/{collection_id}` | Delete collection **and its items**; `404` if missing |
| POST | `/collections/{collection_id}/items` | Create; `404` missing collection, `409` repost, `400` collection mismatch. `FeatureCollection` body = batch create (all-or-nothing `400` if any feature mismatches) |
| POST | `/collections/{collection_id}/bulk_items` | Bulk insert/update `{"items": {...}}`; reports `{received, success, skipped, errors[]}` |
| PUT | `/collections/{collection_id}/items/{item_id}` | Update; `404` missing item/collection, `400` id mismatch |
| PATCH | `/collections/{collection_id}/items/{item_id}` | Merge Patch / JSON Patch; `400` immutable `id`/`collection` change |
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

Status codes: `201` create (Mode A), `200` link (Mode B `{"id": ...}`) / update, `204` delete/unlink, `400` bad request (`limit=0`, id mismatch, invalid search), `404` missing resource or edge, `409` id collision / full-body repost, `412` failed `If-Match` precondition.

**Optimistic concurrency (ETag/`If-Match`):** single-resource GET and write responses carry a strong `ETag` (`"<primary_term>-<seq_no>-<indexhash>"`, suffixed `-gzip`/`-br` when the body is compressed — RFC 9110 §8.8.1). `PUT`/`PATCH`/`DELETE` honor `If-Match` (`*`, tag lists, weak/malformed tags never match); the checked version conditions the write itself, so check-and-write is atomic (`409`→`412` on a lost race).

### Planned — currently 404/405

| Method | Path | Status |
|---|---|---|
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

> **Just want to run it, not hack on it?** `compose.release.yml` pulls the published
> image (`ghcr.io/jonhealy1/stac-os-rs`) instead of building from source — no
> Rust toolchain or repo checkout needed:
>
> ```bash
> docker compose -f compose.release.yml up -d   # API :3000, OpenSearch :9200
> ```

**3. Or run the API locally** — iterate on Rust code while OpenSearch stays in Docker:

```bash
docker compose up -d opensearch
cargo run     # API on http://localhost:3000
```

### Configuration Reference

All variables are read once at startup — changing them requires a restart.

**Backend Connection**

| Variable | Description | Default | Required |
|----------|-------------|---------|----------|
| `OPENSEARCH_URL` | OpenSearch endpoint the store connects to. | `http://localhost:9200` | Optional |
| `STAC_BASE_URL` | External base URL used for advertised links (`self`, `root`, `next`, queryables/sortables `$id`s) — set this behind a reverse proxy so links point at the public origin rather than the bind address. | `http://localhost:{port}` | Optional |

**Feature Flags**

| Variable | Description | Default | Required |
|----------|-------------|---------|----------|
| `ENABLE_TRANSACTIONS_EXTENSIONS` | Mounts all mutating routes — POST/PUT/PATCH/DELETE on collections and items, `bulk_items`, and the `/catalogs` transaction surface. The read surface is always mounted. Set to `true` in `compose.yml`. | `false` | Optional |
| `CATALOGS_HIDE_ALTERNATE_PARENTS` | Suppresses `related`/`duplicate` links and extra `parent` links for alternate parents in poly-hierarchy — only the contextual `parent` is advertised. Useful for multi-tenant deployments. (Upstream calls this `HIDE_ALTERNATE_PARENTS`.) | `false` | Optional |
| `ENABLE_COLLECTIONS_SEARCH` | Enables the collection search extensions (`sortby`, `fields`, `q`, `filter`, `query`, `datetime`, `bbox`) on `GET /collections`. `false` = plain `limit`/`token` listing, params ignored. | `true` | Optional |
| `ENABLE_COLLECTIONS_SEARCH_ROUTE` | Mounts the `GET`/`POST /collections-search` route. Set to `true` in `compose.yml` (upstream default is off). | `false` | Optional |

**Limits & Performance**

| Variable | Description | Default | Required |
|----------|-------------|---------|----------|
| `STAC_DEFAULT_COLLECTION_LIMIT` | Default page size for `/collections` and `/collections-search` when `limit` isn't given. | `10` | Optional |
| `STAC_GLOBAL_COLLECTION_MAX_LIMIT` | Hard cap on collection page size — larger `limit` values clamp to this. | none | Optional |
| `STAC_DEFAULT_ITEM_LIMIT` | Default page size for item listings and `/search` when `limit` isn't given. | `10` | Optional |
| `STAC_GLOBAL_ITEM_MAX_LIMIT` | Hard cap on item page size — larger `limit` values clamp to this. | none | Optional |

**Filtering & Exclusions**

| Variable | Description | Default | Required |
|----------|-------------|---------|----------|
| `EXCLUDED_FROM_ITEMS` | Comma-separated field paths stripped from every item response (e.g. `private_data,properties.confidential`). Applied before the request's own `fields` filter — privacy wins over includes. | none | Optional |
| `EXCLUDED_FROM_QUERYABLES` | Comma-separated field paths excluded from `/queryables` output; nested children are excluded with them. | none | Optional |

**Database Indexing & Behavior**

| Variable | Description | Default | Required |
|----------|-------------|---------|----------|
| `USE_DATETIME` | `false` = items carrying `start_datetime`/`end_datetime` are datetime-filtered on the range alone, ignoring `properties.datetime` (matches upstream `USE_DATETIME=false` query semantics). | `true` | Optional |

Upstream sfeos env vars **not yet wired** — planned but currently inert if set: `REQUIRE_IF_MATCH` (428 enforcement), `ENABLE_STAC_VALIDATOR`, `ENABLE_REDIS_QUEUE`/`VALIDATE_BEFORE_QUEUE` (async writes), `FREE_TEXT_FIELDS`, `ENABLE_CATALOGS_ROUTE` (our `/catalogs` surface is core, always on), `ENABLE_TOPOLOGY_VALIDATION`, `DATABASE_REFRESH`, custom-mapping/sort-remap vars, and the `ES_*` connection family (we take a single `OPENSEARCH_URL`).

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
- `collections`, `ids`, `bbox` (incl. antimeridian crossing), `intersects`, `datetime`, `sortby`, `limit`, `fields`, `query` (STACQL), and `filter` (CQL2 json + text) are applied
- `filter` coverage is basic-cql2 + advanced comparisons + basic spatial predicates — temporal functions (`t_*`), arithmetic, and array ops are not yet supported (400)
- `sortby` works on `/search` + `/catalogs/*/search` (item-search binding), on items listings `?sortby=±field` (features binding), and on `/collections` + `/collections-search`; sortables at `/sortables` + `/collections/{id}/sortables` + `/collections-sortables`
- Collections-search extension is on: `/collections` and `/collections-search` (GET+POST) support `sortby`, `fields`, `q` free text, CQL2 `filter`, STACQL `query`, `datetime` (temporal-extent overlap), and `bbox` (geo_shape on the derived `bbox_shape` field — injected at write time, stripped on read)
- `datetime` bounds require full RFC3339 date-times — date-only inputs (`2023-06-01`) 400, matching the spec and upstream. Accepting them as whole-day intervals (OGC Features' leniency) would be a client-friendliness enhancement: normalize bounds to `T00:00:00Z`/`T23:59:59.999Z` before `valid()` runs — OpenSearch's `date` mapping already accepts date-only
- Paging is opaque-token based: `next` links carry a base64 `token` (in the href for GET, in `body` for POST); `{offset: n}` in a POST body is still accepted. Item listings use `?limit=&token=`; collection cursors are lenient (unknown values restart at page 1, matching upstream), item tokens 400. All paged responses include `numberMatched`/`numberReturned`. Deep paging (>10k) needs `search_after`/cursor — not implemented

**Core STAC routes**
- `GET /queryables` (root) serves upstream's core queryables set; per-collection queryables merge that with the items index mapping (`platform.enum` comes from a terms aggregation). True per-collection narrowing would need per-collection indices
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
