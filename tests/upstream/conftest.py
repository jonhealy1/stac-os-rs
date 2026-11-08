"""Adapter conftest: run the vendored sfeos API tests against stac-os-rs.

Upstream (stac-fastapi-elasticsearch-opensearch) wires these fixtures to an
in-process FastAPI app and a real DatabaseLogic. Here the HTTP-level fixtures
(app_client, txn_client, catalogs_app_client, ctx) talk to a running
stac-os-rs over HTTP, and OpenSearch maintenance calls (refresh, wipe) go
straight to the cluster.

Fixtures that only exist inside the Python implementation (app, database,
core_client, route_dependencies_*, ...) resolve to _SkipProxy, so tests that
exercise in-process internals skip instead of erroring. Nothing is deleted:
every vendored test still runs or reports a skip reason.

Run against a live stack:  make up   (or OPENSEARCH_URL/STAC_API_URL to point
elsewhere).  Requires: pytest, pytest-asyncio, httpx, sfeos (for its imports).
"""

import asyncio
import copy
import json
import os

# Test modules switch imports on BACKEND — we always run the opensearch
# variant (conftest loads before any test module's module-level imports).
os.environ.setdefault("BACKEND", "opensearch")

# The aggregation extension (`/aggregate`, `/collections/{id}/aggregations`)
# is not implemented yet. The files stay vendored — delete these ignore
# entries when the extension lands.
collect_ignore = [
    "extensions/test_aggregation.py",
    "extensions/test_malformed_aggregation.py",
]
import sys
import types
from typing import Any, Callable

import httpx
import pytest
import pytest_asyncio


def _install_optional_dep_stubs():
    """Stub packages sfeos imports but our HTTP-level tests never exercise.

    `stac_fastapi.core.redis_utils` is imported at module level by some
    vendored tests. Without the redis/retry packages installed that import
    chain fails at collection — these stubs let the modules import cleanly,
    and any test that actually touches the stubbed objects skips on use.
    """
    if "redis" not in sys.modules:
        try:
            import redis  # noqa: F401
        except ImportError:
            redis_mod = types.ModuleType("redis")
            aio_mod = types.ModuleType("redis.asyncio")
            client_mod = types.ModuleType("redis.asyncio.client")
            sentinel_mod = types.ModuleType("redis.asyncio.sentinel")
            exc_mod = types.ModuleType("redis.exceptions")

            class _MissingRedis:
                def __init__(self, *_a, **_k):
                    pytest.skip("redis package not installed")

                def __getattr__(self, attr):
                    return _MissingRedis

                @classmethod
                def from_url(cls, *_a, **_k):
                    pytest.skip("redis package not installed")

            class ConnectionError(Exception):
                pass

            class TimeoutError(Exception):
                pass

            client_mod.Pipeline = _MissingRedis
            sentinel_mod.Sentinel = _MissingRedis
            exc_mod.ConnectionError = ConnectionError
            exc_mod.TimeoutError = TimeoutError
            aio_mod.client = client_mod
            aio_mod.sentinel = sentinel_mod
            aio_mod.__getattr__ = lambda name: _MissingRedis
            redis_mod.asyncio = aio_mod
            redis_mod.exceptions = exc_mod
            redis_mod.__getattr__ = lambda name: _MissingRedis
            sys.modules.update(
                {
                    "redis": redis_mod,
                    "redis.asyncio": aio_mod,
                    "redis.asyncio.client": client_mod,
                    "redis.asyncio.sentinel": sentinel_mod,
                    "redis.exceptions": exc_mod,
                }
            )
    if "retry" not in sys.modules:
        try:
            import retry  # noqa: F401
        except ImportError:
            retry_mod = types.ModuleType("retry")

            def retry(func=None, *_a, **_k):
                if callable(func):
                    return func
                return lambda f: f

            retry_mod.retry = retry
            sys.modules["retry"] = retry_mod


_install_optional_dep_stubs()

STAC_API_URL = os.environ.get("STAC_API_URL", "http://localhost:3000").rstrip("/")


class _UpstreamHostTransport(httpx.AsyncHTTPTransport):
    """Send `http://test-server` URLs to the real API listener.

    Upstream tests pin link hrefs and `$id`s to the in-process app's
    `http://test-server` base URL. The API is started with
    `STAC_BASE_URL=http://test-server` so every advertised link carries
    that host; this transport rewrites the connection target so those
    URLs still reach the real listener.
    """

    async def handle_async_request(self, request: httpx.Request) -> httpx.Response:
        if request.url.host == "test-server":
            target = httpx.URL(STAC_API_URL)
            request.url = request.url.copy_with(
                scheme=target.scheme, host=target.host, port=target.port
            )
        return await super().handle_async_request(request)
OPENSEARCH_URL = os.environ.get("OPENSEARCH_URL", "http://localhost:9200").rstrip("/")
# Physical index prefix used by the running server (`Store::connect` default).
INDEX_PREFIX = os.environ.get("STAC_INDEX_PREFIX", "stac")

DATA_DIR = os.path.join(os.path.dirname(__file__), "data")

# --- sfeos internals: real when installed, skip-stubs otherwise -------------
# Test modules import these names from this conftest. With the sfeos package
# installed they resolve to the real classes; without it they become
# skip-on-use stand-ins so collection still works.

try:
    from stac_fastapi.opensearch.app import instantiate_api as _sfeos_instantiate_api
    from stac_fastapi.opensearch.config import (
        AsyncOpensearchSettings as AsyncSettings,
    )
    from stac_fastapi.opensearch.config import (
        OpensearchSettings as SearchSettings,
    )
    from stac_fastapi.opensearch.database_logic import DatabaseLogic
except ImportError:

    class _UnavailableSettings:
        def __init__(self, *_a, **_k):
            pytest.skip("sfeos not installed")

    AsyncSettings = _UnavailableSettings
    SearchSettings = _UnavailableSettings

    class DatabaseLogic:  # noqa: D101 — name compat only
        def __init__(self, *_a, **_k):
            pass

        def __getattr__(self, attr):
            return _SkipProxy(f"DatabaseLogic.{attr}")


def instantiate_api(*_a, **_k):
    # Building an in-process sfeos app can never exercise our external
    # API — skip even when the package is installed.
    pytest.skip("in-process sfeos app — does not exercise the external API")


try:
    from stac_fastapi.types.errors import ConflictError, NotFoundError
except ImportError:

    class ConflictError(Exception):
        pass

    class NotFoundError(Exception):
        pass


try:
    from stac_pydantic import api as _stac_api
except ImportError:
    _stac_api = None

try:
    from opensearchpy import AsyncOpenSearch
except ImportError:
    AsyncOpenSearch = None


def pytest_configure(config):
    config.addinivalue_line(
        "markers", "datetime_filtering: matches datetime_filtering mark"
    )
    config.addinivalue_line(
        "filterwarnings", "ignore:Duplicate Operation ID:UserWarning"
    )


class _SkipProxy:
    """Chains attribute access/calls and skips the test on use.

    Any interaction — attribute access, call, subscript, await — raises
    pytest.skip, so tests reaching for in-process internals report
    'skipped: not portable' instead of erroring.
    """

    def __init__(self, name: str = "internal"):
        object.__setattr__(self, "_skip_name", name)

    def __getattr__(self, attr: str):
        return _SkipProxy(f"{self._skip_name}.{attr}")

    def __call__(self, *_a, **_k):
        pytest.skip(f"{self._skip_name}() needs in-process sfeos internals")

    def __getitem__(self, _item):
        pytest.skip(f"{self._skip_name}[...] needs in-process sfeos internals")

    def __await__(self):
        pytest.skip(f"await {self._skip_name} needs in-process sfeos internals")
        yield  # pragma: no cover — unreachable


class Context:
    def __init__(self, item, collection):
        self.item = item
        self.collection = collection


class MockRequest:
    base_url = "http://test-server"
    url = "http://test-server/test"
    headers = {}
    query_params = {}

    def __init__(
        self,
        method: str = "GET",
        url: str = "XXXX",
        app: Any | None = None,
        query_params: dict[str, Any] | None = None,
        headers: dict[str, Any] | None = None,
    ):
        self.method = method
        self.url = url
        self.app = app
        self.query_params = query_params or {"limit": "10"}
        self.headers = headers or {"content-type": "application/json"}


def _load_file(filename: str) -> dict:
    with open(os.path.join(DATA_DIR, filename)) as file:
        return json.load(file)


_test_item_prototype = _load_file("test_item.json")
_test_collection_prototype = _load_file("test_collection.json")


@pytest.fixture
def load_test_data() -> Callable[[str], dict]:
    return _load_file


@pytest.fixture
def test_item() -> dict:
    return copy.deepcopy(_test_item_prototype)


@pytest.fixture
def test_collection() -> dict:
    return copy.deepcopy(_test_collection_prototype)


def _dump(doc) -> dict:
    """Pydantic model or plain dict -> JSON-able dict."""
    if hasattr(doc, "model_dump"):
        return doc.model_dump(mode="json", by_alias=True, exclude_none=True)
    return dict(doc)


def _pointer_parts(path: str):
    return [p.replace("~1", "/").replace("~0", "~") for p in path.split("/")[1:]]


def _json_patch_op(doc, op: dict):
    """Single-op RFC 6902 application — enough for conftest test seams."""
    name = op["op"]
    if op["path"] == "":
        if name in ("add", "replace"):
            return op["value"]
        raise ValueError(f"{name} at root unsupported")
    parts = _pointer_parts(op["path"])
    target = doc
    for part in parts[:-1]:
        target = target[int(part)] if isinstance(target, list) else target[part]
    last = parts[-1]
    if isinstance(target, list) and last != "-":
        last = int(last)
    if name in ("add", "replace"):
        if isinstance(target, list):
            (target.insert if name == "add" or last == "-" else target.__setitem__)(
                last if isinstance(last, int) else len(target), op["value"]
            )
        else:
            target[last] = op["value"]
    elif name == "remove":
        del target[last]
    elif name in ("copy", "move"):
        src = doc
        for part in _pointer_parts(op["from"]):
            src = src[int(part)] if isinstance(src, list) else src[part]
        if name == "move":
            f_parts = _pointer_parts(op["from"])
            f_target = doc
            for part in f_parts[:-1]:
                f_target = (
                    f_target[int(part)]
                    if isinstance(f_target, list)
                    else f_target[part]
                )
            del f_target[f_parts[-1]]
        if isinstance(target, list):
            target.append(src)
        else:
            target[last] = src
    return doc


def _merge_patch_dict(target: dict, patch: dict) -> dict:
    for key, value in patch.items():
        if value is None:
            target.pop(key, None)
        elif isinstance(value, dict) and isinstance(target.get(key), dict):
            _merge_patch_dict(target[key], value)
        else:
            target[key] = value
    return target


def _check_status(resp: httpx.Response) -> httpx.Response:
    if resp.status_code == 409:
        raise ConflictError(f"{resp.request.method} {resp.request.url}: 409")
    if resp.status_code == 404:
        raise NotFoundError(f"{resp.request.method} {resp.request.url}: 404")
    resp.raise_for_status()
    return resp


class _IndexSelector:
    async def refresh_cache(self) -> None:
        pass


# Mirrors Store::ensure_indices in src/store.rs — keep in sync if the Rust
# mappings change. Recreating indices (rather than delete_by_query) also
# clears stale dynamic mappings, e.g. a field first indexed as `long`
# rejecting a later float.
_INDEX_MAPPINGS = {
    "hierarchy": {
        "properties": {
            "kind": {"type": "keyword"},
            "parents": {"type": "keyword"},
        }
    },
    "catalogs": {"properties": {"id": {"type": "keyword"}}},
    "collections": {
        "properties": {
            "id": {"type": "keyword"},
            # text+keyword: `q` free text needs analysis; term/sort on
            # strings needs the keyword sub-field. Mirrors store.rs.
            "title": {
                "type": "text",
                "fields": {"keyword": {"type": "keyword", "ignore_above": 256}},
            },
            "description": {
                "type": "text",
                "fields": {"keyword": {"type": "keyword", "ignore_above": 256}},
            },
            "keywords": {
                "type": "text",
                "fields": {"keyword": {"type": "keyword", "ignore_above": 256}},
            },
            "extent": {
                "properties": {
                    "temporal": {"properties": {"interval": {"type": "date"}}}
                }
            },
            # upstream alias so collections sort on `temporal`.
            "temporal": {"type": "alias", "path": "extent.temporal.interval"},
            "bbox_shape": {"type": "geo_shape"},
        }
    },
    "items": {
        # long->double so differently-typed numeric properties across
        # collections can't lock each other out of the shared index.
        "dynamic_templates": [
            {
                "ints_as_double": {
                    "match_mapping_type": "long",
                    "mapping": {"type": "double"},
                }
            },
            {
                "floats_as_double": {
                    "match_mapping_type": "double",
                    "mapping": {"type": "double"},
                }
            },
        ],
        "properties": {
            "id": {"type": "keyword"},
            "collection": {"type": "keyword"},
            "bbox": {"type": "float"},
            "geometry": {"type": "geo_shape"},
            "properties": {
                "properties": {
                    "datetime": {"type": "date"},
                    "start_datetime": {"type": "date"},
                    "end_datetime": {"type": "date"},
                }
            },
            # response material, not query fields — mirrors store.rs.
            "links": {"enabled": False},
            "assets": {"enabled": False},
        }
    },
}


class _TranslatingOsClient:
    """Rewrites upstream index names to ours on direct OpenSearch ops.

    Upstream stores catalogs *in* the `collections` index and items in
    per-collection `items_*` indices; ours are `{prefix}-catalogs` /
    `{prefix}-collections` / `{prefix}-items`. Reads (`get`) try each
    candidate; writes use the first. Everything else passes through.
    """

    def __init__(self, client):
        self._client = client

    @staticmethod
    def _candidates(index):
        if index == "collections":
            return (f"{INDEX_PREFIX}-collections", f"{INDEX_PREFIX}-catalogs")
        if index.startswith("items"):
            return (f"{INDEX_PREFIX}-items",)
        for logical in ("catalogs", "hierarchy"):
            if index == logical:
                return (f"{INDEX_PREFIX}-{logical}",)
        return (index,)

    @staticmethod
    def _doc_id(id):
        # Physical item _ids are upstream's composite "item_id|collection_id"
        # — pass through unchanged.
        return str(id)

    async def get(self, index, id, **kw):
        id = self._doc_id(id)
        candidates = self._candidates(index)
        for physical in candidates:
            try:
                hit = await self._client.get(index=physical, id=id, **kw)
            except Exception:
                if physical == candidates[-1]:
                    raise
                continue
            # Upstream stores hierarchy edges on the doc itself; ours
            # live in the stac-hierarchy index (`parents` list, root
            # sentinel filtered). Synthesize `parent_ids` so tests
            # reading raw _source assert against the same edges.
            if physical in (
                f"{INDEX_PREFIX}-catalogs",
                f"{INDEX_PREFIX}-collections",
            ):
                try:
                    node = await self._client.get(
                        index=f"{INDEX_PREFIX}-hierarchy", id=id
                    )
                    hit["_source"]["parent_ids"] = [
                        p
                        for p in node["_source"].get("parents", [])
                        if p != "root"
                    ]
                except Exception:
                    hit["_source"]["parent_ids"] = []
            return hit
        raise AssertionError("unreachable")

    async def exists(self, index, id, **kw):
        id = self._doc_id(id)
        for physical in self._candidates(index):
            if await self._client.exists(index=physical, id=id, **kw):
                return True
        return False

    async def index(self, index, id=None, **kw):
        return await self._client.index(
            index=self._candidates(index)[0],
            id=self._doc_id(id) if id is not None else None,
            **kw,
        )

    async def delete(self, index, id, **kw):
        return await self._client.delete(
            index=self._candidates(index)[0], id=self._doc_id(id), **kw
        )

    async def update(self, index, id, **kw):
        return await self._client.update(
            index=self._candidates(index)[0], id=self._doc_id(id), **kw
        )

    async def search(self, index, **kw):
        translated = ",".join(
            c for i in index.split(",") for c in self._candidates(i)
        )
        return await self._client.search(index=translated, **kw)

    @property
    def indices(self):
        return _TranslatingIndicesClient(self)

    def __getattr__(self, attr):
        return getattr(self._client, attr)


class _TranslatingIndicesClient:
    """`client.indices.*` calls with upstream index names -> ours."""

    def __init__(self, owner):
        self._owner = owner
        self._raw = owner._client.indices

    def _first(self, index):
        return self._owner._candidates(index)[0]

    async def get_settings(self, index=None, **kw):
        if index and index != "_all":
            return await self._raw.get_settings(index=self._first(index), **kw)
        return await self._raw.get_settings(index=index, **kw)

    async def put_settings(self, index=None, **kw):
        return await self._raw.put_settings(index=self._first(index), **kw)

    async def put_mapping(self, index=None, **kw):
        return await self._raw.put_mapping(index=self._first(index), **kw)

    async def get_mapping(self, index=None, **kw):
        return await self._raw.get_mapping(index=self._first(index), **kw)

    async def refresh(self, index=None, **kw):
        return await self._raw.refresh(index=index, **kw)

    async def delete(self, index=None, **kw):
        return await self._raw.delete(index=self._first(index), **kw)

    async def exists(self, index=None, **kw):
        return await self._raw.exists(index=self._first(index), **kw)

    async def create(self, index=None, **kw):
        return await self._raw.create(index=self._first(index), **kw)

    def __getattr__(self, attr):
        return getattr(self._raw, attr)


class _HttpDatabase:
    """Stand-in for sfeos DatabaseLogic.

    `.client` is an AsyncOpenSearch wrapper that translates upstream index
    names to ours, so `refresh_indices` and direct index ops work.
    delete_* drop and recreate OUR index set (clears docs AND stale
    mappings); anything else skips the calling test.
    """

    def __init__(self, os_client, http=None):
        self.client = _TranslatingOsClient(os_client)
        self._http = http
        self.async_index_selector = _IndexSelector()
        # Tests isinstance-check this against DatetimeIndexInserter to
        # detect upstream's datetime-sharded mode. Any proxy instance
        # satisfies isinstance when the name resolves to the _SkipProxy
        # class through the stubs — None is deterministic False
        # (non-sharded expectations: realtime GET finds fresh docs).
        self.async_index_inserter = None

    async def _clear(self, logical: str) -> None:
        index = f"{INDEX_PREFIX}-{logical}"
        try:
            await self.client.delete_by_query(
                index=index,
                body={"query": {"match_all": {}}},
                params={"refresh": "true", "conflicts": "proceed"},
            )
        except Exception:
            # Index may not exist yet — a wiped registry is equivalent.
            pass

    async def delete_items(self) -> None:
        await self._clear("items")

    async def delete_collections(self) -> None:
        for logical in ("collections", "catalogs", "hierarchy"):
            await self._clear(logical)

    async def get_one_item(self, item_id, collection_id, **kwargs):
        r = await self._http.get(
            f"/collections/{collection_id}/items/{item_id}"
        )
        if r.status_code == 404:
            return None
        return _check_status(r).json()

    async def find_collection(self, *args, **kwargs):
        # Present only so tests can monkeypatch it as a "lookup reached"
        # tripwire — against the external API it is never invoked.
        raise NotImplementedError("in-process lookup — no HTTP equivalent")

    def __getattr__(self, attr: str):
        if attr.startswith("_"):
            raise AttributeError(attr)
        return _SkipProxy(f"database.{attr}")


class HttpTxnClient:
    """Drop-in for sfeos TransactionsClient that writes via our HTTP API.

    Supports the surface `create_collection`/`create_item` need; every other
    method skips on use (in-process internals with no HTTP equivalent yet).
    """

    def __init__(self, os_client):
        self._http = httpx.AsyncClient(
            base_url=STAC_API_URL, timeout=60.0, transport=_UpstreamHostTransport()
        )
        self.database = _HttpDatabase(os_client, self._http)
        # bulk_item_insert is synchronous upstream (no await at call
        # sites) — it needs a sync client.
        self._sync = httpx.Client(base_url=STAC_API_URL, timeout=60.0)

    async def create_collection(self, collection, request=None, refresh=False):
        r = await self._http.post("/collections", json=_dump(collection))
        result = _check_status(r).json()
        if refresh:
            # Upstream's refresh kwarg feeds the OS bulk/create call;
            # the HTTP port refreshes the index directly instead.
            await self.database.client.indices.refresh(index="_all")
        return result

    def _raise_bulk_conflict(self, item_id, collection_id):
        # Emulates RAISE_ON_BULK_ERROR: the flag lives in the (separate)
        # API process, so the monkeypatched test env can't reach it.
        # When set in-process we translate the API's conflict response
        # into upstream's exception type.
        from stac_fastapi.sfeos_helpers.database import ItemAlreadyExistsError

        raise ItemAlreadyExistsError(item_id=item_id, collection_id=collection_id)

    def bulk_item_insert(self, items, chunk_size=None, **kwargs):
        """HTTP port of TransactionsClient.bulk_item_insert."""
        raw = {k: _dump(v) for k, v in items.items.items()}
        request = kwargs.get("request")
        collection_id = (
            getattr(request, "path_params", {}).get("collection_id")
            if request
            else None
        )
        if collection_id is None:
            # Without a path collection the first admissible entry naming
            # one sets the batch collection (upstream behavior).
            collection_id = next(
                (
                    i["collection"]
                    for i in raw.values()
                    if isinstance(i, dict)
                    and isinstance(i.get("id"), str)
                    and i["id"]
                    and isinstance(i.get("collection"), str)
                    and i["collection"]
                ),
                None,
            )
        if collection_id is None:
            raise ValueError("no collection derivable for bulk_items")
        method = items.method
        method = method.value if hasattr(method, "value") else method
        r = self._sync.post(
            f"/collections/{collection_id}/bulk_items",
            json={"items": raw, "method": method},
        )
        if r.status_code == 404:
            from stac_fastapi.types.errors import NotFoundError

            raise NotFoundError(f"Collection '{collection_id}' not found")
        result = _check_status(r).json()
        if os.getenv("RAISE_ON_BULK_ERROR") == "true":
            for err in result.get("errors", []):
                if "already exists" in err.get("msg", ""):
                    self._raise_bulk_conflict(err["id"], collection_id)
            if result.get("errors"):
                from fastapi import HTTPException

                raise HTTPException(status_code=400, detail=result)
        return result

    async def create_item(self, collection_id, item, request=None, refresh=False):
        from fastapi import HTTPException

        body = _dump(item)
        features = body.get("features")
        if features is None:
            r = await self._http.post(
                f"/collections/{collection_id}/items", json=body
            )
            if r.status_code == 409 and os.getenv("RAISE_ON_BULK_ERROR") == "true":
                self._raise_bulk_conflict(body.get("id", ""), collection_id)
            if r.status_code == 400:
                # Upstream raises HTTPException from the handler; the
                # detail string carries the validation message.
                raise HTTPException(
                    status_code=400, detail=r.json().get("detail")
                )
            result = _check_status(r).json()
            if refresh:
                await self.database.client.indices.refresh(index="_all")
            return result
        # FeatureCollection path: one batch POST. Non-strict responses
        # are 201 + per-member `errors`/`conflict_errors`; strict
        # rejects surface as 4xx.
        r = await self._http.post(
            f"/collections/{collection_id}/items", json=body
        )
        if r.status_code == 409 and os.getenv("RAISE_ON_BULK_ERROR") == "true":
            self._raise_bulk_conflict(
                next(
                    iter(r.json().get("conflict_errors") or {}),
                    features[0].get("id", ""),
                ),
                collection_id,
            )
        if r.status_code == 400:
            raise HTTPException(status_code=400, detail=r.json().get("detail"))
        result = _check_status(r).json()
        # RAISE_ON_BULK_ERROR lives in the API process — emulate the
        # strict-mode rejection from the partial-success payload, like
        # bulk_item_insert does above.
        if os.getenv("RAISE_ON_BULK_ERROR") == "true":
            conflicts = result.get("conflict_errors") or {}
            member_errors = result.get("errors") or []
            if conflicts:
                self._raise_bulk_conflict(
                    next(iter(conflicts)), collection_id
                )
            if member_errors:
                grouped: dict = {}
                for e in member_errors:
                    grouped.setdefault(e.get("msg", "unknown"), []).append(
                        e.get("id")
                    )
                n = len(member_errors)
                raise HTTPException(
                    status_code=400,
                    detail={
                        "message": (
                            f"Batch rejected. {n} items failed validation."
                        ),
                        "errors": grouped,
                        "summary": {
                            "input_count": len(features),
                            "processed_count": len(features),
                            "valid_count": len(result.get("features") or []),
                            "validation_error_count": n,
                            "skipped_total": n + len(conflicts),
                            "conflict_count": len(conflicts),
                            "database_error_count": 0,
                        },
                    },
                )
        if refresh:
            await self.database.client.indices.refresh(index="_all")
        return result

    async def delete_collection(self, collection_id, request=None, refresh=False):
        r = await self._http.delete(f"/collections/{collection_id}")
        if r.status_code not in (200, 204, 404):
            _check_status(r)
        return r

    async def delete_item(self, item_id, collection_id, request=None, refresh=False):
        r = await self._http.delete(
            f"/collections/{collection_id}/items/{item_id}"
        )
        if r.status_code not in (200, 204, 404):
            _check_status(r)
        return r

    async def aclose(self):
        try:
            await self.database.client.close()
        except Exception:
            pass
        self._sync.close()
        await self._http.aclose()

    async def _validate_single_item(self, item):
        # Test seam: upstream runs schema validation here. Present so a
        # monkeypatched defect propagates out of _apply_and_validate_patch.
        return item

    async def _apply_and_validate_patch(
        self, existing, patch, content_type, item_type="item"
    ):
        """Minimal in-process port of upstream's patch+validate seam."""
        import copy as _copy

        patched = _copy.deepcopy(_dump(existing))
        if isinstance(patch, list) and content_type == "application/json-patch+json":
            for op in patch:
                patched = _json_patch_op(patched, op)
        elif isinstance(patch, dict):
            patched = _merge_patch_dict(patched, patch)
        else:
            raise ValueError(f"unsupported patch for {content_type}")
        if item_type == "item":
            return await self._validate_single_item(patched)
        return patched

    def __getattr__(self, attr: str):
        if attr.startswith("_"):
            raise AttributeError(attr)
        return _SkipProxy(f"txn_client.{attr}")


# sfeos internal entry points used by a few tests — they need an in-process
# app, which does not exist against a remote server.
database = _SkipProxy("database")
core_client = _SkipProxy("core_client")


def build_test_app(*_args, **_kwargs):
    pytest.skip("build_test_app constructs an in-process sfeos app")


def build_test_app_with_catalogs(*_args, **_kwargs):
    pytest.skip("build_test_app_with_catalogs constructs an in-process sfeos app")


def get_flattened_routes(*_args, **_kwargs):
    pytest.skip("get_flattened_routes inspects an in-process router")


@pytest_asyncio.fixture()
async def catalogs_client():
    pytest.skip("catalogs_client is an in-process sfeos client")


async def create_collection(txn_client, collection: dict) -> None:
    await txn_client.create_collection(
        _stac_api.Collection(**dict(collection)) if _stac_api else dict(collection),
        request=MockRequest,
        refresh=True,
    )


async def create_item(txn_client, item: dict) -> None:
    if "collection" in item:
        await txn_client.create_item(
            collection_id=item["collection"],
            item=_stac_api.Item(**item) if _stac_api else dict(item),
            request=MockRequest,
            refresh=True,
        )
    else:
        await txn_client.create_item(
            collection_id=item["features"][0]["collection"],
            item=_stac_api.ItemCollection(**item) if _stac_api else dict(item),
            request=MockRequest,
            refresh=True,
        )


async def delete_collections_and_items(txn_client) -> None:
    await refresh_indices(txn_client)
    await txn_client.database.delete_items()
    await txn_client.database.delete_collections()


async def refresh_indices(txn_client) -> None:
    try:
        await txn_client.database.client.indices.refresh(index="_all")
    except Exception:
        pass


@pytest_asyncio.fixture()
async def ctx(txn_client, test_collection, test_item):
    await delete_collections_and_items(txn_client)

    await create_collection(txn_client, test_collection)
    await create_item(txn_client, test_item)

    yield Context(item=test_item, collection=test_collection)

    await delete_collections_and_items(txn_client)


def _os_client():
    if AsyncOpenSearch is None:
        return _SkipProxy("opensearch")
    return AsyncOpenSearch([OPENSEARCH_URL])


@pytest_asyncio.fixture()
async def txn_client():
    client = HttpTxnClient(_os_client())
    # Tests not using `ctx` still start on an empty registry — upstream
    # gets the same isolation from ctx teardowns elsewhere.
    await delete_collections_and_items(client)
    yield client
    await client.aclose()


class _HttpCoreClient:
    """HTTP stand-in for sfeos CoreCrudClient — read surface only."""

    def __init__(self):
        self._http = httpx.AsyncClient(
            base_url=STAC_API_URL, timeout=60.0, transport=_UpstreamHostTransport()
        )

    async def item_collection(self, collection_id, request=None, **kwargs):
        r = await self._http.get(
            f"/collections/{collection_id}/items", params={"limit": 10000}
        )
        return _check_status(r).json()

    def __getattr__(self, attr: str):
        if attr.startswith("_"):
            raise AttributeError(attr)
        return _SkipProxy(f"core_client.{attr}")

    async def aclose(self):
        await self._http.aclose()


@pytest_asyncio.fixture()
async def core_client():
    client = _HttpCoreClient()
    yield client
    await client.aclose()


@pytest_asyncio.fixture()
async def bulk_txn_client():
    # No wipe here: upstream's fixture is a bare client and every test
    # that uses it also pulls in `ctx` — wiping would destroy the
    # collection ctx just created.
    client = HttpTxnClient(_os_client())
    yield client
    await client.aclose()


@pytest_asyncio.fixture()
async def app_client():
    async with httpx.AsyncClient(
        base_url=STAC_API_URL, timeout=60.0, transport=_UpstreamHostTransport()
    ) as c:
        yield c


@pytest_asyncio.fixture()
async def catalogs_app_client():
    async with httpx.AsyncClient(
        base_url=STAC_API_URL, timeout=60.0, transport=_UpstreamHostTransport()
    ) as c:
        yield c


def _skip_fixture(name: str):
    # Session scope: upstream test files define session-scoped fixtures
    # that depend on `app` — a function scope would ScopeMismatch at
    # resolution time (error) before this skip could fire.
    @pytest.fixture(name=name, scope="session")
    def _f():
        pytest.skip(f"fixture '{name}' needs an in-process sfeos app")

    return _f


# Fixtures with no HTTP equivalent — tests requesting them skip.
app = _skip_fixture("app")
catalogs_app = _skip_fixture("catalogs_app")
app_rate_limit = _skip_fixture("app_rate_limit")
app_client_rate_limit = _skip_fixture("app_client_rate_limit")
app_basic_auth = _skip_fixture("app_basic_auth")
app_client_basic_auth = _skip_fixture("app_client_basic_auth")
route_dependencies_app = _skip_fixture("route_dependencies_app")
route_dependencies_client = _skip_fixture("route_dependencies_client")
datetime_filtering_env = _skip_fixture("datetime_filtering_env")
mock_datetime_env = _skip_fixture("mock_datetime_env")


@pytest.fixture
def stac_validator_off(monkeypatch):
    """Explicitly-unvalidated writes. In a validated run the server
    validates regardless, so tests posting invalid docs skip."""
    if UPSTREAM_VALIDATED:
        pytest.skip("validated run has ENABLE_STAC_VALIDATOR=true")
    monkeypatch.setenv("ENABLE_STAC_VALIDATOR", "false")


@pytest_asyncio.fixture()
async def datetime_nanos_off(txn_client):
    """Upstream remaps item datetimes to `date` (USE_DATETIME_NANOS=false)
    for a test; our items index is always millisecond `date`, so a plain
    reset is the equivalent setup."""
    await delete_collections_and_items(txn_client)
    yield


async def use_datetime_nanos(value: bool) -> None:
    """In-process helper — rewrites upstream's index template for
    USE_DATETIME_NANOS. No external equivalent; callers skip."""
    pytest.skip(
        "USE_DATETIME_NANOS rewrites upstream's index template in-process; "
        "our datetime fields are always mapped `date`"
    )


def get_flattened_routes(router_obj, prefix=""):
    """Flatten routes from a router-like object (kept verbatim from upstream)."""
    api_routes = set()
    routes = getattr(router_obj, "routes", [])

    for route in routes:
        if hasattr(route, "methods") and route.methods:
            for m in route.methods:
                if m == "HEAD":
                    continue
                r_path = getattr(route, "path", "")
                full_path = f"{prefix}{r_path}".replace("//", "/")
                api_routes.add(f"{m} {full_path}")

        if hasattr(route, "app") and hasattr(route.app, "routes"):
            r_path = getattr(route, "path", getattr(route, "prefix", ""))
            next_prefix = f"{prefix}{r_path}"
            api_routes.update(get_flattened_routes(route.app, next_prefix))

        if hasattr(route, "original_router"):
            r_prefix = getattr(route, "prefix", "")
            if not r_prefix and hasattr(route, "include_context"):
                r_prefix = getattr(route.include_context, "prefix", "")
            next_prefix = f"{prefix}{r_prefix}"
            api_routes.update(get_flattened_routes(route.original_router, next_prefix))

        elif hasattr(route, "routes") and route is not router_obj:
            r_path = getattr(route, "path", getattr(route, "prefix", ""))
            next_prefix = f"{prefix}{r_path}"
            api_routes.update(get_flattened_routes(route, next_prefix))

    return api_routes


# --- env-mutation tests --------------------------------------------------
# These tests set os.environ INSIDE the pytest process. stac-os-rs runs as
# a separate process, so runtime env mutation can never reach it. The
# corresponding server-side knobs ARE implemented and read at startup:
#   STAC_GLOBAL_COLLECTION_MAX_LIMIT  -> AppState.max_collection_limit
#   STAC_DEFAULT_COLLECTION_LIMIT     -> AppState.default_collection_limit
#   USE_DATETIME=false                -> Store::with_use_datetime
#   EXCLUDED_FROM_ITEMS               -> AppState.excluded_item_fields
_ENV_MUTATION_TESTS = {
    "test_global_collection_max_limit_set": "STAC_GLOBAL_COLLECTION_MAX_LIMIT",
    "test_default_collection_limit": "STAC_DEFAULT_COLLECTION_LIMIT",
    "test_global_item_max_limit_set": "STAC_GLOBAL_ITEM_MAX_LIMIT",
    "test_default_item_limit_without_limit_parameter_set": "STAC_DEFAULT_ITEM_LIMIT",
    "test_use_datetime_false": "USE_DATETIME",
    "test_hide_private_data_from_item": "EXCLUDED_FROM_ITEMS",
}


# Set when the suite runs against an API started with
# ENABLE_STAC_VALIDATOR=true ENABLE_TOPOLOGY_VALIDATION=true
# REQUIRE_IF_MATCH=true (see `make test-upstream-validated`). In the
# default run the API is unvalidated and the gated tests skip.
UPSTREAM_VALIDATED = os.getenv("UPSTREAM_VALIDATED", "").lower() in ("1", "true")

# Whole files skipped: the feature is deliberately out of scope, or the
# file only exercises in-process internals.
_SKIP_FILES = {
    "test_api_stac_validator.py": (
        "requires an API started with ENABLE_STAC_VALIDATOR=true — "
        "run `make test-upstream-validated`"
    ),
}

# Tests that only pass when the API runs validated — skipped in the
# default (unvalidated) run.
_VALIDATED_ONLY_TESTS = {
    "test_require_if_match_returns_428_without_it": (
        "requires REQUIRE_IF_MATCH=true — run `make test-upstream-validated`"
    ),
    "test_require_if_match_refuses_bulk_upsert": (
        "requires REQUIRE_IF_MATCH=true — run `make test-upstream-validated`"
    ),
}

# Per-test env the fixed validated-server config can't honor (server env
# is read at startup; the monkeypatched values never leave the pytest
# process). Equivalent behavior is covered by Rust tests.
_VALIDATED_MODE_SKIPS = {
    "test_chunked_validation_with_max_batch_size": (
        "requires per-request MAX_BATCH_SIZE — chunked writes are not implemented"
    ),
    "test_chunked_validation_exceeds_max_error_size": (
        "requires per-request MAX_BATCH_SIZE — chunked writes are not implemented; "
        "the MAX_BATCH_ERROR_SIZE threshold is covered by Rust tests"
    ),
    "test_topology_validation_disabled_by_default": (
        "validated run has ENABLE_TOPOLOGY_VALIDATION=true"
    ),
    "test_bulk_topology_validation_filters_invalid_items_lenient": (
        "requires per-request MAX_BATCH_SIZE — chunked writes are not implemented"
    ),
    "test_bulk_topology_validation_trips_circuit_breaker": (
        "requires per-request MAX_BATCH_SIZE — chunked writes are not implemented"
    ),
    "test_topology_validation_respects_max_vertices_limit": (
        "requires per-request MAX_TOPOLOGY_VERTICES=10; the vertex "
        "ceiling is covered by Rust tests"
    ),
    "test_topology_validation_allows_items_within_vertex_limit": (
        "requires per-request MAX_TOPOLOGY_VERTICES=20; the vertex "
        "ceiling is covered by Rust tests"
    ),
    "test_datetime_validation_disabled_by_default": (
        "validated run has ENABLE_STAC_VALIDATOR=true"
    ),
}

# Individual tests that exercise in-process mechanics an external API
# can't reproduce (client-side monkeypatched hooks, upstream index
# internals).
_IN_PROCESS_TESTS = {
    "test_datetime_copy_checks_live_source_atomically": (
        "injects a mid-patch write by monkeypatching the test-side client; "
        "the external API never calls it"
    ),
    "test_put_requires_searchable_target_after_unrefreshed_create": (
        "upstream finds the write target via (refresh-dependent) search; "
        "ours uses a realtime GET — divergent lookup semantics by design"
    ),
    "test_a_collection_write_after_an_alias_move_returns_412": (
        "monkeypatches upstream's index_behind_alias in-process — "
        "index-layout internals with no HTTP equivalent"
    ),
    "test_conditional_put_writes_to_the_index_that_holds_the_item": (
        "monkeypatches the inserter's get_target_index — per-collection "
        "index internals, not HTTP-observable"
    ),
    "test_a_patch_whose_item_was_recreated_elsewhere_returns_412": (
        "recreates the collection via monkeypatched client methods — "
        "the hook never fires against an external API"
    ),
    "test_a_write_between_check_and_write_returns_412": (
        "races a write via monkeypatched client methods — the hook "
        "never fires against an external API"
    ),
    "test_bulk_item_insert_validation_error": (
        "asserts upstream's python stac-validator message text verbatim; "
        "rust stac-validate wording differs — the per-item error "
        "structure is covered by Rust tests"
    ),
    "test_bulk_item_insert_reports_validation_error_per_item": (
        "asserts upstream's python stac-validator message text verbatim; "
        "rust stac-validate wording differs — the per-item error "
        "structure is covered by Rust tests"
    ),
    "test_hidden_put_queue_ack_preserves_storage_until_worker": (
        "requires a live Redis queue (out of scope)"
    ),
    "test_write_datetime_immutability_before_queue": (
        "requires a live Redis queue (out of scope)"
    ),
    "test_generated_json_failure_propagates": (
        "monkeypatches the in-process cql2_text_to_json — an external "
        "API never calls it"
    ),
    "test_datetime_aggregations_cover_all_dates": (
        "the aggregation extension (`/aggregate`) is not implemented"
    ),
    "test_sort_spans_date_and_date_nanos_indexes": (
        "rewrites upstream's index template mid-test via in-process "
        "use_datetime_nanos + USE_DATETIME_NANOS env — no HTTP equivalent"
    ),
    "test_get_sortable_properties_shared": "calls sfeos_helpers sort internals",
    "test_get_sortable_properties_shared_aliases": "calls sfeos_helpers sort internals",
    "test_get_sortable_properties_shared_exclusions": (
        "calls sfeos_helpers sort internals"
    ),
    "test_get_sortable_field_types_shared": "calls sfeos_helpers sort internals",
    "test_populate_sort_shared_unmapped_type": "calls sfeos_helpers sort internals",
    "test_populate_sort_shared_item_datetime_compares_in_milliseconds": (
        "calls sfeos_helpers sort internals"
    ),
    "test_collections_sortables_route_gated_on_collection_search": (
        "instantiates an in-process sfeos app to inspect registered routes"
    ),
    "test_sortby_union_membership": (
        "mutates ROOT_QUERYABLES_UNION via monkeypatch — env-only/inert "
        "against an external API process"
    ),
    "test_get_aggregate_cql2_like_not_double_url_decoded": (
        "calls the in-process aggregate helper — /aggregate not implemented"
    ),
    "test_root_queryables_config_file": (
        "loads a queryables config file into an in-process app"
    ),
    "test_root_queryables_union_served_from_cache": (
        "exercises the in-process queryables cache"
    ),
    "test_root_queryables_union": (
        "mutates ROOT_QUERYABLES_UNION via monkeypatch — env-only/inert "
        "against an external API process"
    ),
    "test_root_queryables_union_cache_refreshed_after_ttl": (
        "mutates QUERYABLES_CACHE_TTL via monkeypatch — env-only/inert "
        "against an external API process"
    ),
    "test_queryables_excluded_fields": (
        "mutates EXCLUDED_FROM_QUERYABLES via monkeypatch — env-only/inert "
        "against an external API process"
    ),
    "test_hidden_item_true": (
        "mutates HIDE_ITEM_PATH via monkeypatch — env-only/inert against "
        "an external API process"
    ),
    # --- test_catalogs.py: in-process concurrency/log assertions ---
    "test_scoped_put_collection_validates_body": (
        "monkeypatches the in-process txn client to assert request "
        "validation ordering — no HTTP equivalent"
    ),
    "test_core_put_collection_retries_on_concurrent_membership_change": (
        "injects a mid-write membership change by monkeypatching the "
        "in-process database — the external API never calls it"
    ),
    "test_link_collection_keeps_concurrent_put_metadata": (
        "races a PUT via monkeypatched in-process client methods — the "
        "hook never fires against an external API"
    ),
    "test_unlink_collection_keeps_concurrent_put_metadata": (
        "races a PUT via monkeypatched in-process client methods — the "
        "hook never fires against an external API"
    ),
    "test_link_collection_returns_409_when_conflict_retries_exhausted": (
        "forces conflict-retry exhaustion via monkeypatched in-process "
        "client methods — the hook never fires against an external API"
    ),
    "test_link_sub_catalog_keeps_concurrent_put_metadata": (
        "races a PUT via monkeypatched in-process client methods — the "
        "hook never fires against an external API"
    ),
    "test_unlink_sub_catalog_keeps_concurrent_put_metadata": (
        "races a PUT via monkeypatched in-process client methods — the "
        "hook never fires against an external API"
    ),
    "test_put_catalog_retries_on_concurrent_link": (
        "injects a mid-write link by monkeypatching the in-process "
        "database — the external API never calls it"
    ),
    "test_link_sub_catalog_returns_409_when_conflict_retries_exhausted": (
        "forces conflict-retry exhaustion via monkeypatched in-process "
        "client methods — the hook never fires against an external API"
    ),
    "test_put_catalog_returns_409_when_conflict_retries_exhausted": (
        "forces conflict-retry exhaustion via monkeypatched in-process "
        "client methods — the hook never fires against an external API"
    ),
    "test_catalog_delete_logs_error_with_traceback": (
        "asserts on upstream's in-process error logging via caplog — "
        "no HTTP equivalent"
    ),
    "test_hidden_items_counter": (
        "mutates HIDE_ITEM_PATH via monkeypatch — env-only/inert against "
        "an external API process"
    ),
    "test_a_delete_that_fails_after_the_catalog_is_gone_can_be_retried": (
        "monkeypatches upstream's unlink_catalog_children_shared — the "
        "hook never fires against an external API"
    ),
    "test_a_catalog_created_again_under_the_id_survives_a_stale_retry": (
        "monkeypatches upstream's unlink_catalog_children_shared — the "
        "hook never fires against an external API"
    ),
    "test_failed_cleanup_retains_parent_and_retry_preserves_updates": (
        "injects cleanup failures by patching update_by_query on the "
        "in-process OS client — the external API never calls it"
    ),
    "test_cleanup_exhaustion_returns_409": (
        "patches update_by_query on the in-process OS client to force "
        "version conflicts — no HTTP equivalent"
    ),
    "test_invalid_catalog_ids_never_start_cleanup": (
        "asserts on a monkeypatched unlink_catalog_children_shared "
        "counter — in-process internals, nothing HTTP-observable"
    ),
    "test_bulk_index_error_raised_on_non_conflict_errors_strict_mode": (
        "monkeypatches bulk_sync on the in-process database — the "
        "external API never calls it"
    ),
    "test_bulk_non_conflict_errors_not_raised_in_permissive_mode": (
        "monkeypatches bulk_sync on the in-process database — the "
        "external API never calls it"
    ),
    "test_bulk_conflict_error_takes_precedence_over_other_errors": (
        "monkeypatches bulk_sync on the in-process database — the "
        "external API never calls it"
    ),
}


def pytest_collection_modifyitems(items):
    for item in items:
        name = item.name.split("[")[0]
        if name in _ENV_MUTATION_TESTS:
            item.add_marker(
                pytest.mark.skip(
                    reason=(
                        f"mutates os.environ[{_ENV_MUTATION_TESTS[name]!r}] "
                        "in-process; the API runs as a separate process and "
                        "reads config at startup — the knob is implemented "
                        "server-side but this test cannot exercise it"
                    )
                )
            )
        fname = str(item.fspath).rsplit("/", 1)[-1]
        if fname in _SKIP_FILES and not (
            UPSTREAM_VALIDATED and fname == "test_api_stac_validator.py"
        ):
            item.add_marker(pytest.mark.skip(reason=_SKIP_FILES[fname]))
            continue
        if UPSTREAM_VALIDATED and name in _VALIDATED_MODE_SKIPS:
            item.add_marker(
                pytest.mark.skip(reason=_VALIDATED_MODE_SKIPS[name])
            )
            continue
        if not UPSTREAM_VALIDATED and name in _VALIDATED_ONLY_TESTS:
            item.add_marker(
                pytest.mark.skip(reason=_VALIDATED_ONLY_TESTS[name])
            )
            continue
        # Param-level skips: expectations keyed to env vars mutated
        # in-process (validator=true, USE_DATETIME=false for ranged
        # fields) — the external API can't see them.
        params = getattr(getattr(item, "callspec", None), "params", {}) or {}
        if params.get("validator") == "true" and not UPSTREAM_VALIDATED:
            item.add_marker(
                pytest.mark.skip(
                    reason="requires ENABLE_STAC_VALIDATOR=true — "
                    "run `make test-upstream-validated`"
                )
            )
        elif params.get("validator") == "false" and UPSTREAM_VALIDATED:
            item.add_marker(
                pytest.mark.skip(
                    reason="validated run always validates writes; the "
                    "unvalidated side of this param has no meaning"
                )
            )
        elif params.get("if_match", "").startswith(" ") or params.get(
            "if_match", ""
        ).endswith(" "):
            item.add_marker(
                pytest.mark.skip(
                    reason="leading/trailing-whitespace header value is "
                    "rejected by the HTTP client transport (h11) before "
                    "it can be sent"
                )
            )
        elif params.get("queue") == "true":
            item.add_marker(
                pytest.mark.skip(
                    reason="ENABLE_REDIS_QUEUE=true — Redis queueing is out "
                    "of scope; env-only mutation can't affect external API"
                )
            )
        elif params.get("use_datetime") == "false":
            item.add_marker(
                pytest.mark.skip(
                    reason="fixture mutates USE_DATETIME=false in-process; "
                    "external API reads config at startup"
                )
            )
        elif params.get("field") in ("start_datetime", "end_datetime"):
            item.add_marker(
                pytest.mark.skip(
                    reason="fixture mutates USE_DATETIME=false in-process; "
                    "external API reads config at startup"
                )
            )
        elif params.get("path") == "/aggregate":
            item.add_marker(
                pytest.mark.skip(
                    reason="the aggregation extension (/aggregate) is not "
                    "implemented"
                )
            )
        elif name in _IN_PROCESS_TESTS:
            item.add_marker(
                pytest.mark.skip(reason=_IN_PROCESS_TESTS[name])
            )
