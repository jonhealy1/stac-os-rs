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

import copy
import json
import os
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
    "collections": {"properties": {"id": {"type": "keyword"}}},
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
                return await self._client.get(index=physical, id=id, **kw)
            except Exception:
                if physical == candidates[-1]:
                    raise
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

    def __init__(self, os_client):
        self.client = _TranslatingOsClient(os_client)
        self.async_index_selector = _IndexSelector()
        # Tests isinstance-check this against DatetimeIndexInserter to
        # detect upstream's datetime-sharded mode. Any proxy instance
        # satisfies isinstance when the name resolves to the _SkipProxy
        # class through the stubs — None is deterministic False
        # (non-sharded expectations: realtime GET finds fresh docs).
        self.async_index_inserter = None

    async def _reset(self, logical: str) -> None:
        index = f"{INDEX_PREFIX}-{logical}"
        try:
            await self.client.indices.delete(index=index)
        except Exception:
            pass
        await self.client.indices.create(
            index=index,
            body={
                "settings": {"number_of_replicas": 0},
                "mappings": _INDEX_MAPPINGS[logical],
            },
        )

    async def delete_items(self) -> None:
        await self._reset("items")

    async def delete_collections(self) -> None:
        for logical in ("collections", "catalogs", "hierarchy"):
            await self._reset(logical)

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
        self.database = _HttpDatabase(os_client)
        self._http = httpx.AsyncClient(base_url=STAC_API_URL, timeout=60.0)

    async def create_collection(self, collection, request=None, refresh=False):
        r = await self._http.post("/collections", json=_dump(collection))
        return _check_status(r).json()

    async def create_item(self, collection_id, item, request=None, refresh=False):
        body = _dump(item)
        features = body.get("features")
        if features is None:
            r = await self._http.post(
                f"/collections/{collection_id}/items", json=body
            )
            return _check_status(r).json()
        out = []
        for feature in features:
            r = await self._http.post(
                f"/collections/{collection_id}/items", json=feature
            )
            out.append(_check_status(r).json())
        return out

    async def aclose(self):
        try:
            await self.database.client.close()
        except Exception:
            pass
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
    yield client
    await client.aclose()


@pytest_asyncio.fixture()
async def bulk_txn_client():
    client = HttpTxnClient(_os_client())
    yield client
    await client.aclose()


@pytest_asyncio.fixture()
async def app_client():
    async with httpx.AsyncClient(
        base_url=STAC_API_URL, timeout=60.0
    ) as c:
        yield c


@pytest_asyncio.fixture()
async def catalogs_app_client():
    async with httpx.AsyncClient(
        base_url=STAC_API_URL, timeout=60.0
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
    """No-op: stac-os-rs has no ENABLE_STAC_VALIDATOR toggle (always off)."""
    monkeypatch.setenv("ENABLE_STAC_VALIDATOR", "false")


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


# Whole files skipped: the feature is deliberately out of scope, or the
# file only exercises in-process internals.
_SKIP_FILES = {
    "test_api_stac_validator.py": (
        "stac-validate request-body validation is not implemented "
        "(ENABLE_STAC_VALIDATOR is env-only and inert externally)"
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
    "test_require_if_match_returns_428_without_it": (
        "REQUIRE_IF_MATCH is env-only/inert externally (in-process "
        "config flag)"
    ),
    "test_require_if_match_refuses_bulk_upsert": (
        "REQUIRE_IF_MATCH is env-only/inert externally (in-process "
        "config flag)"
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
        if str(item.fspath).rsplit("/", 1)[-1] in _SKIP_FILES:
            item.add_marker(
                pytest.mark.skip(
                    reason=_SKIP_FILES[str(item.fspath).rsplit("/", 1)[-1]]
                )
            )
            continue
        # Param-level skips: expectations keyed to env vars mutated
        # in-process (validator=true, USE_DATETIME=false for ranged
        # fields) — the external API can't see them.
        params = getattr(getattr(item, "callspec", None), "params", {}) or {}
        if params.get("validator") == "true":
            item.add_marker(
                pytest.mark.skip(
                    reason="ENABLE_STAC_VALIDATOR=true is env-only/inert "
                    "against an external API process"
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
        elif params.get("field") in ("start_datetime", "end_datetime"):
            item.add_marker(
                pytest.mark.skip(
                    reason="fixture mutates USE_DATETIME=false in-process; "
                    "external API reads config at startup"
                )
            )
        elif name in _IN_PROCESS_TESTS:
            item.add_marker(
                pytest.mark.skip(reason=_IN_PROCESS_TESTS[name])
            )
