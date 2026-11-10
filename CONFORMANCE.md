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
| `rel=children` link + `GET /children` endpoint | ❌ | Spec: any entity emitting `child` links must carry a `children` link to a `**/children` endpoint. Catalogs have both; the landing page has neither |

## STAC API — Features / Collections (`v1.0.0/ogcapi-features`)

| Requirement | Status | Notes |
|---|---|---|
| `GET /collections` returns `{collections, links}` | ✅ | |
| `GET /collections/{id}` | ✅ | `self`/`root`/`parent`/`canonical`/`items`/`queryables`/sortables links |
| `items` link on Collection, `application/geo+json` | ⚠️ | Link emitted with `type: application/json`; spec table specifies `application/geo+json` |
| `GET /collections/{id}/items` | ✅ | `bbox`, `datetime`, `limit` params |
| `GET /collections/{id}/items/{itemId}` | ✅ | |
| OAFeat `conf/core` + `conf/geojson` | ✅ | Advertised |
| OAFeat `conf/oas30` | — | We serve OpenAPI 3.0, so this *may* be advertised; currently not |
| `https://api.stacspec.org/v1.0.0/collections` (subset class) | — | Satisfied behaviorally; not advertised |

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
| `q` free-text parameter | ❌ | Advertised in generated OpenAPI on all search GETs but **silently ignored** — `GetSearch` never reads it. Implement or de-document |

## Filter / CQL2 (`item-search#filter`, `features-3` + `cql2` classes)

| Class | Status | Notes |
|---|---|---|
| `item-search#filter` | ⚠️ | We advertise the **`v1.0.0-rc.2`** URI; the stable `v1.0.0/item-search#filter` URI should be advertised (keeping rc.2 for back-compat) |
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
| `children` link + `GET /children` on the landing page | ❌ | Same gap as Core above |
| Browseable conformance URI | ⚠️ | We advertise `v1.0.0/browseable`; the official class URI is **`v1.0.0-rc.3/browseable`** (still an RC in the spec) |
| Every item reachable via `child`/`item` traversal | ✅ | Hierarchy DAG exposes all nodes |

## Collection Search (`v1.0.0/collection-search`, opt-in flags)

| Class | Status | Notes |
|---|---|---|
| `v1.0.0/collection-search` + `ogcapi-common-2/conf/simple-query` | ⚠️ | `bbox`/`datetime`/`limit` implemented on `GET /collections` (gated by `ENABLE_COLLECTIONS_SEARCH`) — **not advertised** |
| `collection-search#sort` (v1.1.0) | ⚠️ | Implemented, not advertised |
| `collection-search#fields` (v1.0.0) | ⚠️ | Implemented, not advertised |
| `collection-search#filter` (v1.0.0) | ⚠️ | CQL2 over collection fields implemented (`COLLECTION_FIELDS` mapping), not advertised |
| `collection-search#free-text` (rc.1 basic) | ⚠️ | `q` implemented via `multi_match` over `id`/`title`/`description`/`keywords`, but **comma-separated terms are not split** (spec: commas are OR separators), and not advertised |
| `collection-search#sortables` (v1.1.0) + `/collections-sortables` | ⚠️ | Endpoint + `/collections` link implemented, but the class is advertised **unconditionally** — even when collections-search is disabled and the route is unmounted |
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
| `conformance` link on scoped catalog docs | ⚠️ | `/catalogs/{id}/conformance` exists but the doc doesn't link to it |
| `POST /catalogs/{id}/bulk` | — | Planned; currently 404/405 (distinct from `_bulk` item ingest) |

## Transaction (`ogcapi-features/extensions/transaction`)

| Requirement | Status | Notes |
|---|---|---|
| `POST /collections/{id}/items` → 201, populate `collection` from URI, 409 on dup | ⚠️ | 201 + 409 implemented; **`Location` header missing** (spec: MUST) |
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

1. **Filter + Browseable conformance URIs** — advertise
   `v1.0.0/item-search#filter` (stable, keep rc.2) and switch
   `v1.0.0/browseable` → `v1.0.0-rc.3/browseable`. One-line change each
   in `conformance_classes()`.
2. **`q` on item search** — either wire `multi_match` against `id` +
   `properties.*` text fields (pattern already exists for collections)
   and advertise `v1.0.0-rc.1/item-search#free-text`, or strip the
   `q` param from the three utoipa annotations. Currently a documented
   lie.
3. **`items` link `type` → `application/geo+json`** — one-line fix.
4. **`Location` header on item create** — spec MUST; small response
   change on both create paths.
5. **Collections `q` comma-split** — split `q` on `,` before joining, to
   match basic free-text OR semantics.
6. **`GET /children` + `children` link on landing page** — required by
   the `v1.0.0/children` class we advertise; reuse
   `get_catalog_children` with `ROOT_CATALOG_ID`.
7. **Gate `collection-search#sortables`** on the collections-search
   flags; advertise the implemented-but-unclaimed classes
   (`collection-search`, `simple-query`, `#sort`, `#fields`, `#filter`,
   `#free-text`) only when the flags are on.
8. **Optional advertisement additions** — `v1.0.0/collections`,
   OAFeat `conf/oas30`, `rel=conformance` on scoped catalog docs.
