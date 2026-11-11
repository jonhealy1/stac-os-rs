# STAC API Conformance Matrix

Status of `stac-os-rs` against the official STAC API specification and
extension conformance classes, audited from:

- [radiantearth/stac-api-spec](https://github.com/radiantearth/stac-api-spec)
  (`core`, `ogcapi-features`, `item-search`, `fragments/itemcollection`)
- [stac-api-extensions](https://github.com/stac-api-extensions)
  (`fields`, `query`, `filter`, `sort`, `context`, `freetext-search`,
  `children`, `browseable`, `transaction`, `collection-search`)
- [StacLabs/multi-tenant-catalogs](https://github.com/StacLabs/multi-tenant-catalogs)

Legend:

| Mark | Meaning |
|---|---|
| ✅ | Implemented and verified against the spec text |
| ⚠️ | Implemented, but deviates from the spec in a documented way |
| ❌ | Required by a conformance class we advertise, but missing |
| — | Not advertised / intentionally unsupported |

## STAC API — Core (`v1.0.0/core`)

| Requirement | Status | Notes |
|---|---|---|
| `GET /` landing page returns a valid STAC Catalog with `conformsTo` | ✅ | `root_landing_page`, `metadata.rs` |
| `conformance` link (`/conformance`) | ✅ | |
| `service-desc` link → OpenAPI doc | ✅ | `/api`, `application/vnd.oai.openapi+json;version=3.0` |
| `service-doc` link → human-readable docs | ✅ | `/api.html` (also aliased at `/docs`) |
| `data` link (`/collections`) | ✅ | |
| `search` link (`/search`, `application/geo+json`) | ✅ | |
| `GET /conformance` returns `{conformsTo: [...]}` | ✅ | Same class list as landing page |
| `child` links on the landing page | ✅ | Derived from the hierarchy DAG at read time |
| `rel=children` link + `GET /children` endpoint | ✅ | Always mounted; collections-only when `ENABLE_CATALOGS_ROUTE` is off |

## STAC API — Features / Collections (`v1.0.0/ogcapi-features`)

| Requirement | Status | Notes |
|---|---|---|
| `GET /collections` returns `{collections, links}` | ✅ | |
| `GET /collections/{id}` | ✅ | `self`/`root`/`parent`/`canonical`/`items`/`queryables`/sortables links |
| `items` link on Collection, `application/geo+json` | ✅ | |
| `GET /collections/{id}/items` | ✅ | `bbox`, `datetime`, `limit` params |
| `GET /collections/{id}/items/{itemId}` | ✅ | |
| OAFeat `conf/core` + `conf/geojson` | ✅ | Advertised |
| OAFeat `conf/oas30` | ✅ | OpenAPI 3.0 served at `service-desc` |
| `https://api.stacspec.org/v1.0.0/collections` (subset class) | ✅ | |

## STAC API — Item Search (`v1.0.0/item-search`)

| Requirement | Status | Notes |
|---|---|---|
| `GET /search` (required) | ✅ | |
| `POST /search` (recommended) | ✅ | |
| Params: `limit`, `bbox`, `datetime`, `intersects`, `ids`, `collections` | ✅ | |
| `bbox` + `intersects` together → 400 | ✅ | `Search::valid()` mutual exclusion |
| Antimeridian-crossing `bbox` | ✅ | Rewritten to a 2-piece `intersects` MultiPolygon |
| Default limit 10 (`STAC_DEFAULT_ITEM_LIMIT`), cap `MAX_SEARCH_LIMIT` | ✅ | |
| Response is an ItemCollection with `numberMatched`/`numberReturned` | ✅ | Top-level fields are the current (non-deprecated) form |
| `next`/`prev` pagination links (GET href-merge; POST body+token) | ✅ | Opaque `token` cursor |
| `q` free-text parameter | ✅ | Comma-separated terms OR'd via `simple_query_string` over `id`/`collection`/`properties.*`; `v1.0.0-rc.1/item-search#free-text` advertised |

## Filter / CQL2 (`item-search#filter`, `features-3` + `cql2` classes)

| Class | Status | Notes |
|---|---|---|
| `item-search#filter` | ✅ | Both the stable `v1.0.0` and legacy `v1.0.0-rc.2` URIs advertised |
| `ogcapi-features-3/conf/filter`, `features-filter` | ✅ | `filter` + `filter-lang` on GET and POST, `/search` and scoped search |
| `cql2-text`, `cql2-json` | ✅ | Both input languages parse |
| `basic-cql2` (`=`, `<>`, `<`, `<=`, `>`, `>=`, `IS NULL`, `AND`/`OR`/`NOT`) | ✅ | |
| `advanced-comparison-operators` (`LIKE`, `BETWEEN`, `IN`) | ✅ | `BETWEEN` text rewritten to a range clause |
| `basic-spatial-functions` (`S_INTERSECTS` et al.) | ✅ | |
| `features-3/conf/queryables` | ✅ | `/queryables` + `/collections/{id}/queryables` (mapping-driven, `additionalProperties: false`) |
| `case-insensitive-comparison` (`CASEI`) | — | Not advertised |
| `accent-insensitive-comparison` (`ACCENTI`) | — | Not advertised |
| `property-property` comparisons | — | Not advertised |
| `temporal-functions` (`T_*`) | — | 400 today; not advertised |
| `spatial-functions` (full set) | — | Not advertised |
| `array-functions` | — | Not advertised |
| `arithmetic`, `functions` | — | Not advertised |
| Queryables link relation | ⚠️ | We emit `rel: "queryables"`; the OGC canonical rel URI is `http://www.opengis.net/def/rel/ogc/1.0/queryables` |

## Fields (`#fields`), Query (`#query`), Sort (`#sort`, `#sortables`)

| Class | Status | Notes |
|---|---|---|
| `item-search#fields`, `ogcapi-features#fields` | ✅ | `include`/`exclude` incl. nested paths; GET `-`-prefix syntax |
| `item-search#query`, `ogcapi-features#query` | ✅ | STACQL operators `eq`/`neq`/`lt`/`lte`/`gt`/`gte`/`startsWith`/`endsWith`/`contains`/`in` |
| `v1.1.0/item-search#sort`, `v1.1.0/ogcapi-features#sort` | ✅ | `sortby` GET `+`/`-` and POST `{field, direction}` |
| `v1.1.0/item-search#sortables` + `GET /sortables` | ✅ | Landing-page sortables link uses the canonical OGC rel URI |
| `ogcapi-features-5/conf/sortables` + `/collections/{id}/sortables` | ✅ | |

## Children (`v1.0.0/children`) & Browseable (`v1.0.0-rc.3/browseable`)

| Requirement | Status | Notes |
|---|---|---|
| `GET /catalogs/{id}/children` → `{children, links}` | ✅ | `root`/`parent`/`self` links; each child carries `self` |
| `?type=Catalog\|Collection` (`children#type-filter`) | ✅ | Scoped endpoint only |
| `children` link on catalogs with `child` links | ✅ | `catalog_links` |
| `children` link + `GET /children` on the landing page | ✅ | |
| Browseable conformance URI | ✅ | Both `v1.0.0` and the official `v1.0.0-rc.3` URIs advertised |
| Every item reachable via `child`/`item` traversal | ✅ | Hierarchy DAG exposes all nodes |

## Collection Search (`v1.0.0/collection-search`, opt-in flags)

| Class | Status | Notes |
|---|---|---|
| `v1.0.0/collection-search` + `ogcapi-common-2/conf/simple-query` | ⚠️ | `bbox`/`datetime`/`limit` implemented on `GET /collections` (gated by `ENABLE_COLLECTIONS_SEARCH`) — **not advertised** |
| `collection-search#sort` (v1.1.0) | ⚠️ | Implemented, not advertised |
| `collection-search#fields` (v1.0.0) | ⚠️ | Implemented, not advertised |
| `collection-search#filter` (v1.0.0) | ⚠️ | CQL2 over collection fields implemented (`COLLECTION_FIELDS` mapping), not advertised |
| `collection-search#free-text` (rc.1 basic) | ⚠️ | `q` implemented (comma-split OR via `multi_match` over `id`/`title`/`description`/`keywords`) — not advertised |
| `collection-search#sortables` (v1.1.0) + `/collections-sortables` | ✅ | Advertised only when a collections-search flag is on |
| `GET /collection-queryables` (proposed path) | — | Not implemented; collection queryables live at `/collections/{id}/queryables` (per-item) |

## Multi-Tenant Catalogs (`v1.0.0/multi-tenant-catalogs`, StacLabs)

| Requirement | Status | Notes |
|---|---|---|
| `GET /catalogs`, `GET /catalogs/{id}` (landing page per catalog) | ✅ | |
| `GET /catalogs/{id}/conformance`, `/queryables`, `/children`, `/catalogs`, `/collections`, items | ✅ | |
| Scoped search `GET/POST /catalogs/{id}/search` (`…/search` class) | ✅ | Recursive descendant scope; out-of-scope `collections` → 403 |
| `/catalogs/search` registry-wide | ✅ | Root-scope equivalent |
| Poly-hierarchy: one `parent` per scoped read, `related` for alternates | ✅ | `CATALOGS_HIDE_ALTERNATE_PARENTS` opt-out |
| Transaction surface (`…/transaction` class, `ENABLE_TRANSACTIONS_EXTENSIONS`) | ✅ | Link/unlink is non-destructive; orphans adopt to root |
| `conformance` link on scoped catalog docs | ✅ | |
| `POST /catalogs/{id}/bulk` | — | Planned; currently 404/405 (distinct from `_bulk` item ingest) |

## Transaction (`ogcapi-features/extensions/transaction`)

| Requirement | Status | Notes |
|---|---|---|
| `POST /collections/{id}/items` → 201 + `Location`, populate `collection` from URI, 409 on dup | ✅ | |
| `POST` partial ItemCollection → 201, partial-failure semantics | ✅ | |
| `PUT`/`PATCH` → 200/204; `id`/`collection` URI mismatch → 400; 404 on missing | ✅ | |
| `DELETE` → 200/204 (+404 acceptable) | ✅ | |
| ETag / `If-Match` optimistic concurrency | ✅ | 412 on mismatch; `REQUIRE_IF_MATCH` 428 reserved |
| `POST /collections/{id}/bulk_items` | ✅ | `_bulk` ingest (implementation-specific, beyond spec) |

## Deliberately unimplemented

- **Context extension** — *deprecated* upstream. Top-level
  `numberMatched`/`numberReturned` is the current required form.
- **Aggregation extension** — not implemented, not advertised.
- **Redis queue / async writes** — out of scope; `202` async path unused.
- **Advanced free-text** (`#advanced-free-text`) — only basic semantics
  are targeted.

## Findings, ordered by effort

**Resolved** (this branch): conformance URIs, item-search `q`, `items`
link media type, `Location` on create, collections `q` comma-split,
`GET /children` + landing-page link.

**Open:**

Nothing currently claimed-but-missing. Remaining honest gaps are all
unclaimed: CQL2 classes we don't advertise (temporal functions,
property-property, CASEI/ACCENTI, array ops, arithmetic),
`POST /catalogs/{id}/bulk`, deep paging beyond 10k, and a
`/collection-queryables` endpoint (the spec's proposed path — ours is
per-collection instead).
