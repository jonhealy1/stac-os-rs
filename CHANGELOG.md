# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `ENABLE_TOPOLOGY_VALIDATION` — geometry checks beyond the item JSON
  schema: WGS84 bounds on every coordinate (lon ±180, lat ±90),
  Polygon/MultiPolygon ring minimum of 4 vertices, the
  `MAX_TOPOLOGY_VERTICES` ceiling (default 5000), and antimeridian
  detection (>180° longitude jumps between consecutive ring vertices).
  Failures return `400` with `Invalid item geometry: ...`
  (`Topology Error: ...` inside batch `errors[]`). Mirrors upstream
  `validate_item_topology_lightweight`
- `RAISE_ON_BULK_ERROR` — strict batch writes (upstream parity). `true`:
  a FeatureCollection POST or `bulk_items` insert with any malformed
  member, schema failure, or conflict rejects the whole batch with
  `400`/`409`. `false` (default): partial semantics — valid members are
  written, failures are reported per-item
- `MAX_BATCH_ERROR_SIZE` — validation-error ceiling for batch members:
  once exceeded, the whole batch is rejected with `400` regardless of
  `RAISE_ON_BULK_ERROR` (`Validation error threshold exceeded`)
- `VALIDATOR_POOL_SIZE` — number of `stac-validate` instances kept warm
  for concurrent validation (default 4)
- `MAX_TOPOLOGY_VERTICES` — maximum vertices per polygon ring under
  `ENABLE_TOPOLOGY_VALIDATION` (default 5000)
- `make test-upstream-validated` — spins up its own API instance with
  `ENABLE_STAC_VALIDATOR=true ENABLE_TOPOLOGY_VALIDATION=true
  REQUIRE_IF_MATCH=true` and runs the validation-gated vendored
  upstream tests (`test_api_stac_validator.py` + the `require_if_match`
  tests) with `UPSTREAM_VALIDATED=1`

### Changed

- FeatureCollection batch POST now uses upstream partial semantics:
  non-strict mode writes valid members and reports failures in the
  response (`errors[]` per member, `conflict_errors` mapping id →
  message, processed-count `message`) — was all-or-nothing 400/409.
  A batch where every member fails returns 409 (all conflicts) or 400
- `bulk_items` and FeatureCollection member validation now validates
  the raw member document (serde normalization previously healed some
  schema violations before the validator saw them) and reports upstream
  message formats: `STAC validation failed for '{id}': ...`,
  `Invalid item datetime: ...`, `Topology Error: ...`
- Item write validation errors now use upstream message formats:
  `Invalid item: STAC validation failed for '{id}': ...`,
  `Invalid item datetime: ...`, `Invalid item geometry: ...` — and the
  bulk-upsert-under-`REQUIRE_IF_MATCH` 428 detail now reads
  `use method 'insert'` like upstream
- `ENABLE_STAC_VALIDATOR` also enables upstream `validate_datetime_range`
  checks: `start_datetime`/`end_datetime` must be supplied together,
  `start <= end`, and `datetime` must fall inside the bounds when they
  exist (`Invalid item datetime: ...`)

## [0.6.0] - 2026-11-08

### Added

- Permissive CORS middleware (`tower_http::cors::CorsLayer::permissive()`)
  applied outermost in `build_app` — OPTIONS preflights and cross-origin
  requests from browser SPAs (e.g. STAC Browser) now succeed; matches
  upstream sfeos's default `CORSMiddleware` configuration
- `tests/cors.rs` — OpenSearch-free smoke test asserting preflight
  `Access-Control-Allow-*` headers
- `ENABLE_STAC_VALIDATOR` — write bodies validated against STAC JSON
  schemas via `stac-validate` before reaching OpenSearch: item,
  collection, and catalog POST/PUT bodies, every member of a
  FeatureCollection or `bulk_items` batch, and the merged result of
  PATCH. Items are validated in read shape (server-injected
  `collection`/`assets`/generated links — the stored doc alone never
  satisfies the schema's `collection` ⇔ `rel=collection` link rule).
  Off by default (upstream parity); invalid docs return 400 with the
  `STAC validation failed:` detail, unknown `stac_version` values are
  rejected up front, and schema fetch failures/timeouts return 502

### Changed

- `stac` 0.14 → 0.17.7 (`geojson` 1.0, `cql2` 0.5.5 along with it). The
  `stac-api` crate is replaced by an in-tree `api_types` module vendoring
  the types we used (`Search`/`GetSearch`, `Items`/`GetItems`,
  `Filter`, `Fields`, `Sortby`, `ItemCollection`/`Context`) — `stac-api`
  0.8.2 is unmaintained upstream and pins `stac` 0.14, which blocked the
  upgrade and `stac-validate` adoption

## [0.5.0] - 2026-11-04

### Added

- `DATABASE_REFRESH` env var controls the write refresh policy
  (`true`/`false`/`wait_for`), replacing the hardcoded `wait_for` that
  parked every write until the index's next refresh tick. Default is
  `false`, matching upstream sfeos — hierarchy link/unlink and PATCH
  writes force `true` like upstream's `patch_item`; `compose.yml` sets
  `true` mirroring sfeos's own compose override
- FeatureCollection batch POST on the catalog-scoped items route
  (`POST /catalogs/{id}/collections/{col}/items`), matching the root
  route's existing behavior

### Changed

- `/search` default page size now honors `STAC_DEFAULT_ITEM_LIMIT`
  (default 10) instead of a hardcoded 100 — the env var already
  documented as covering `/search` and used by the items listing
- Batch item ingest (`POST .../items` with a FeatureCollection body, and
  `POST /collections/{id}/bulk_items`) now issues a single OpenSearch
  `_bulk` request plus one `mget` existence preflight — was N sequential
  `get`+`create` round-trips each waiting for an index refresh

## [0.4.0] - 2026-11-03

### Added

- `/docs` — FastAPI-style Swagger UI endpoint (same page as `/api.html`)
- `STAC_PORT` env var sets the API bind port (was hardcoded `3000`)
- `GET /catalogs/{catalog_id}/queryables` — catalog-scoped queryables
  JSON Schema (404 for missing catalogs), matching the upstream
  catalogs-extension route surface
- Invariant validation on `/catalogs/{catalog_id}/children` responses:
  response links must carry `root`/`parent`/`self` and every child must
  have a `self` link (violations surface as 500, matching the upstream
  `Children` model contract)

### Changed

- `src/handlers.rs` (~5,200 lines) split into `src/handlers/` modules by
  resource domain — `catalogs.rs`, `collections.rs`, `items.rs`,
  `search.rs`, `metadata.rs`, plus `common.rs` for the shared plumbing
  (state, query structs, link/etag helpers, patch utilities, `ApiError`).
  No behavior change
- The `/api` OpenAPI document is now generated at runtime from
  `#[utoipa::path]` annotations on the handlers (`src/docs.rs`),
  replacing the hand-maintained `src/openapi.json`. The generated spec
  is more accurate than the static file ever was: `info.version` tracks
  `Cargo.toml`, previously-missing routes (`/collections-search`,
  `/collections-sortables`, `/api`, `/_mgmt/ping`) are documented, path
  params and `If-Match` headers are declared everywhere they apply,
  `token` replaces the stale `offset` param on GET search, and listing
  routes declare only the params they actually consume

## [0.3.0] - 2026-11-02

### Added

- `/api.html` — Swagger UI service documentation, linked from the
  landing page as `service-doc`
- `/_mgmt/ping` health endpoint returning `{"message":"PONG"}`
- `filter`/`filter-lang` query params declared in the OpenAPI document
  for the catalog/collection listing routes that accept them
- `sortby`, `fields`, and `q` on `GET /catalogs/{id}/collections` —
  same semantics as the collections-search binding, restricted to
  catalog members; sortable fields come from the collections index
  mapping (`sortby=description` → 400, excluded fields → 400
  "not sortable")
- Catalog write responses emit strong `ETag`s; `If-Match` is honored on
  catalog PUT/DELETE (412 on mismatch, 428 when `REQUIRE_IF_MATCH` is
  set and the header is absent)
- Upstream `sfeos` test coverage: `tests/extensions/test_catalogs*`,
  `test_bulk_transactions`, `test_catalog_parent_cleanup`, and the
  `tests/resources/` batch (item, collection, conformance, mgmt) run
  against the live server via an HTTP-backed `txn_client`/`ctx` adapter
  in `tests/upstream/conftest.py`; `test_aggregation*` is vendored but
  ignored via `collect_ignore` until the `/aggregate` extension lands

### Changed

- `filter`/`filter-lang` are now *applied* (previously only validated)
  on `/catalogs`, `/catalogs/{id}/children`, `/catalogs/{id}/catalogs`,
  `/catalogs/{id}/collections`, and `/collections` — listings run a
  scoped OpenSearch query so filtered results paginate correctly
- `GET /catalogs` is now a flat list of every catalog in the registry
  (upstream `get_catalogs`); nested catalogs carry their `parent` links
- Pagination `next` links on listing endpoints now preserve every
  original query parameter (upstream `_next_query`), not only
  `filter`/`filter-lang`
- Generated link titles and rels match upstream
  `generate_poly_hierarchy_links`: `parent` links are titled
  `Root Catalog` or the parent id, global collection reads expose
  catalog memberships as `related`/`duplicate` links with a single
  `parent` to the landing page, and `rel=child` hrefs are scoped under
  the listing catalog (`/catalogs/{id}/catalogs/{child}`)
- JSON-Patch ops addressing `/parent_ids` (including `from` and root
  replacement) are rejected with 400 before any document lookup —
  memberships only change via the catalog link/unlink routes
- The collections index maps `description` as plain `text` (no keyword
  sub-field), matching upstream: it is full-text searchable via `q`
  but not sortable
- Link persistence matches upstream `resolve_links`/`db_to_stac`
  semantics: inferred rels (`self`, `item`, `parent`, `collection`,
  `root`, `items`) are stripped on write and user links are stored;
  on read, stored user links merge with generated structural links
  while API-owned rels (`queryables`, `aggregate(s)`, `canonical`,
  `duplicate`, `sortables`) and in-API `related` hrefs are filtered —
  a GET→PUT round-trip no longer duplicates generated links
- Explicit `POST /search {"collections": []}` now matches zero
  features; `GET /search?collections=` remains unconstrained, matching
  upstream's str2list semantics
- Search request validation (malformed bbox/datetime/filter) runs
  before catalog-scope resolution, so bad params on a missing catalog
  return 400 rather than 404

### Fixed

- Item listing `next` pagination links dropped `filter`/`query` params
- Empty datetime intervals (`""`, `/`, `../..`) and out-of-range bbox
  latitude/longitude now return 400 instead of 200/500
- Scoped search on a nonexistent catalog returns 404 (was empty 200)
- cql2-text filters on listing routes were serialized to JSON before
  translation, losing text-mode semantics — `IN ('x')` single-element
  lists and LIKE escaping now behave correctly; translation errors
  surface as `Error with cql2 filter: …`

## [0.2.0] - 2026-10-31

### Added

- `STAC_BASE_URL` env var sets the externally advertised link host,
  separate from the bind address
- `/collections-sortables` endpoint (mounted when either
  collections-search flag is on) plus the matching link on
  `/collections` and the OGC sortables conformance URI
- `queryables` links (Filter extension) on the landing page and on
  collection resources
- `CONTRIBUTING.md` — dev setup, test workflow, PR requirements, and
  upstream-compat conventions for new contributors
- Upstream `sfeos` test coverage: `tests/api/test_datetime_range.py`
  plus the `tests/extensions/` batch (CQL2 text/JSON-literals, filter,
  queryables-root, sortables) — 105p/208s/0F on the extensions suite

### Changed

- Root `/queryables` now returns upstream's `DEFAULT_QUERYABLES`
  verbatim; per-collection queryables merge defaults with the items
  index mapping and populate `platform.enum` via a terms aggregation
- CQL2 `LIKE` now honors escape sequences (`\%`, `\_`, `\\` literal;
  `%`/`_` → `*`/`?` wildcards), targets the analyzed field, and runs
  `case_insensitive`, matching upstream `cql2_like_to_es`
- CQL2 `in` with a non-list rhs returns upstream's exact 400
  (`Error with cql2 filter: Arg {v} is not a list`); cql2-text
  `IN ('x')` counts as a 1-element list
- `created`/`updated` CQL2 terms on collections no longer target a
  nonexistent `.keyword` sub-field

### Fixed

- GET search `next` pagination links dropped the original query
  parameters; POST `next` links hardcoded `/catalogs/search` —
  both now preserve the request URL/params and method
- Default item sort placed undated (`start_datetime`/`end_datetime`
  range) items mid-list instead of last; explicit `sortby` now gets a
  deterministic `_id` tiebreaker
- CQL2 `date`/`timestamp`/`interval` literals are validated as RFC 3339
  on search and listing routes (incl. `TIMESTAMP()`/`INTERVAL()` args
  scanned from cql2-text) — previously invalid literals could return
  200/empty instead of 400
- cql2-text `x [NOT] BETWEEN a AND b AND <rest>` silently dropped the
  trailing predicate — cql2-rs 0.4 parser bug, worked around by
  rewriting to the `BETWEEN(x, a, b)` function form pre-parse
- Negative `BETWEEN` bounds (`BETWEEN -37.3 AND -37.2`) returned 400 —
  the parser encodes unary minus as `*(-1, x)`; now folded back

## [0.1.1] - 2026-10-30

### Added

- Published multi-arch image (`ghcr.io/jonhealy1/stac-os-rs`) on `v*.*.*`
  tags via `.github/workflows/publish.yml`; `compose.release.yml` runs the
  released image without building from source

### Fixed

- Published image crashed on start (`GLIBC_2.38 not found`) — the
  cross-compiled binary needed a newer glibc than the `debian:bookworm-slim`
  runtime provided; CI image now runs on `ubuntu:24.04` matching the pinned
  build runner

## [0.1.0] - 2026-10-29

Initial release — a STAC API server in Rust (axum) backed by OpenSearch,
implementing the core spec plus the multi-tenant-catalogs extension.

### Added

**Core STAC API**
- Landing page, `/conformance`, `/api` (hand-maintained OpenAPI),
  `/sortables`, `/queryables` (root + per-collection)
- `/collections` + `/collections/{id}` and `/collections/{id}/items` +
  item reads, with generated dynamic links and `limit`/`token` paging
- Item search — `GET`/`POST /search`, `/catalogs/search`, and
  `/catalogs/{id}/search`: `collections`, `ids`, `bbox` (incl.
  antimeridian crossing via ±180° MultiPolygon split), `intersects`,
  `datetime`, `sortby`, `limit`, `fields`, `query`, `filter`, and
  opaque-token pagination

**STAC API extensions**
- Fields — include/exclude with nested paths and `*` wildcards
- Query — STACQL `eq`/`ne`/`gt`/`gte`/`lt`/`lte`/`in`/`contains`
- Filter — CQL2 JSON + text (basic-cql2, advanced comparisons, basic
  spatial predicates)
- Sort — `sortby` on search, item listings, and collections; Sortables
  endpoints (root + per-collection)
- Queryables — JSON Schema generated from the items index mapping
- Collections-search — `GET`/`POST /collections-search` and search
  params on `/collections`: `sortby`, `fields`, free-text `q`, CQL2
  `filter`, `query`, `datetime` (temporal-extent overlap), and `bbox`
  (geo_shape on the write-time-derived `bbox_shape` field)

**Multi-tenant catalogs**
- Poly-hierarchy DAG persisted in a dedicated `stac-hierarchy` index —
  `{kind, parents}` per node; link/unlink is a single-document write,
  orphans are auto-adopted under `root`
- Scoped catalog/collection/item read routes under `/catalogs/{id}`
  with contextual links (`parent` per parent, alternates as
  `related`/`duplicate`)
- Scoped search intersected with a catalog's descendant collections;
  explicitly-named out-of-scope collections return 403

**Transactions**
- Full create/update/PATCH/delete on catalogs, collections, and items
  on both the canonical `/collections` surface and the `/catalogs` scope
- PATCH supports JSON Merge Patch and RFC 6902 JSON Patch (copy/move
  deduplication, immutable `id`/`collection` guards)
- `FeatureCollection` batch POST to `.../items` (all-or-nothing) and
  `/collections/{id}/bulk_items` (`{received, success, skipped, errors}`)
- All mutating routes gated behind `ENABLE_TRANSACTIONS_EXTENSIONS`

**Optimistic concurrency**
- Strong ETags (`"<primary_term>-<seq_no>-<indexhash>"`, per-coding
  suffix for compressed bodies) on single-resource reads and writes
- `If-Match` on PUT/PATCH/DELETE — `*` and tag lists honored, checked
  version conditions the write itself (atomic, RFC 9110)

**Configuration** (env vars, read at startup)
- `OPENSEARCH_URL`, `ENABLE_TRANSACTIONS_EXTENSIONS`,
  `CATALOGS_HIDE_ALTERNATE_PARENTS`, `ENABLE_COLLECTIONS_SEARCH`,
  `ENABLE_COLLECTIONS_SEARCH_ROUTE`, `STAC_DEFAULT_*`/`STAC_GLOBAL_*`
  limit knobs, `EXCLUDED_FROM_ITEMS`, `EXCLUDED_FROM_QUERYABLES`,
  `USE_DATETIME` — see the README Configuration Reference

**Library surface**
- `read_router()` / `transaction_router()` / `build_app(state)` for
  composable axum routing; public `AppState` fields so extension
  handlers share store/link-engine/config

**Ops & testing**
- Dockerfile, `compose.yml` (API + OpenSearch + Dashboards), Makefile,
  `sample_data/` + ingest script
- CI: fmt + clippy + tests, advisory `stac-api-validator` job
- ~150 Rust tests (incl. a port of upstream's `test_catalogs.py`) and a
  vendored sfeos API suite under `tests/upstream/` running over HTTP

[Unreleased]: https://github.com/jonhealy1/stac-os-rs/compare/v0.6.0...main
[0.6.0]: https://github.com/jonhealy1/stac-os-rs/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/jonhealy1/stac-os-rs/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/jonhealy1/stac-os-rs/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/jonhealy1/stac-os-rs/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/jonhealy1/stac-os-rs/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/jonhealy1/stac-os-rs/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/jonhealy1/stac-os-rs/releases/tag/v0.1.0
