# Contributing

Thanks for your interest in `stac-os-rs`. This file covers the local
dev loop and what we expect in a PR.

## Getting set up

```bash
rustup install stable      # or: curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
cargo build                # fetches deps from Cargo.toml automatically
docker compose up -d opensearch   # backend on :9200 (security disabled — dev only)
ENABLE_TRANSACTIONS_EXTENSIONS=true ENABLE_COLLECTIONS_SEARCH_ROUTE=true cargo run
```

The API listens on `http://localhost:3000`. Config knobs (env vars) are
documented in the README's Configuration Reference.

## Running tests

```bash
cargo test                 # unit + integration; integration tests skip cleanly if OpenSearch is down
cargo fmt                  # required — CI runs `cargo fmt --check`
cargo clippy               # required — CI runs with warnings denied
make test                  # same, but starts OpenSearch and sweeps test indices afterwards
make test-upstream         # vendored sfeos API suite over HTTP — needs the API
                           # running on :3000 with both ENABLE_* flags above
```

Integration tests create isolated `it-*` OpenSearch indices per test.
`make test*`/`make test-clean` sweeps them; bare `cargo test` leaves them
behind (harmless, but they accumulate shards — the dev cluster cap is
raised to 3000 in `compose.yml` for this reason).

## Submitting changes

- **Keep it working**: `cargo fmt`, `cargo clippy`, and `cargo test`
  must be clean. If you change route behavior, run `make test-upstream`
  against a live API before opening the PR.
- **Tests**: add coverage for new behavior. Don't delete tests — if a
  test can't run in a given context (e.g. it needs in-process hooks an
  external HTTP server doesn't have), skip it with a clear reason
  instead.
- **Docs**: new routes go in the README route tables *and*
  `src/openapi.json` (hand-maintained for now). New config knobs go in
  the README Configuration Reference. New env vars should be read once
  at startup in `src/main.rs`.
- **Changelog**: add a line under `## [Unreleased]` in
  [`CHANGELOG.md`](CHANGELOG.md) — `### Added`, `### Changed`,
  `### Fixed`, or `### Removed` as appropriate. One line, user-facing
  language (what changed, not how).
- **Commits**: descriptive messages explaining *why*, not just *what*.

## Conventions worth knowing

- Error responses use `{"detail": "..."}` (FastAPI-compatible shape —
  the upstream test suite depends on it).
- Item documents are keyed `"{item_id}|{collection_id}"` in OpenSearch;
  public ids stay in the document `id` field. Filter on `id`, never `_id`.
- Hierarchy edges live in `stac-hierarchy` (`{kind, parents}` per node);
  links in responses are generated dynamically, not stored.
- `bbox_shape` on collection docs is derived write-time plumbing —
  never return it to clients (`strip_collection_internals`).
- Upstream-compat quirks (e.g. `filter-lang` on GET, lenient collection
  cursors, 403 on out-of-scope catalog collections) are deliberate —
  check `tests/upstream/` before "fixing" them.

## Reporting issues

Open a GitHub issue with a minimal reproduction: the request, the
response you got, and the response you expected. Include the
`OPENSEARCH_URL` backend version if it's storage-related.
