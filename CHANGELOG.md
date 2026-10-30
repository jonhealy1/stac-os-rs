# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Published multi-arch image (`ghcr.io/jonhealy1/stac-os-rs`) on `v*.*.*`
  tags via `.github/workflows/publish.yml`; `compose.release.yml` runs the
  released image without building from source

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
