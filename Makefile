OPENSEARCH_URL ?= http://localhost:9200
STAC_API_URL ?= http://localhost:3000
COMPOSE := docker compose

.PHONY: up down reset opensearch test test-integration test-unit test-clean test-upstream ingest validate check fmt

up: ## Start the full stack (api + opensearch + dashboards)
	$(COMPOSE) up -d --build --wait

opensearch: ## Start just OpenSearch and wait for it to be healthy
	$(COMPOSE) up -d --wait opensearch

down: ## Stop all containers (keeps data volumes)
	$(COMPOSE) down

reset: ## Stop, wipe OpenSearch data, restart clean
	$(COMPOSE) down -v
	$(COMPOSE) up -d --build --wait

# Tests run against isolated it-* indices; the trailing curl sweeps them
# (runs even when the suite fails — status is preserved via $$status).
test: opensearch ## Unit + integration tests (starts OpenSearch first)
	@OPENSEARCH_URL=$(OPENSEARCH_URL) cargo test; status=$$?; \
	curl -sf -XDELETE '$(OPENSEARCH_URL)/it-*' > /dev/null 2>&1 || true; \
	exit $$status

test-integration: opensearch ## Integration tests only
	@OPENSEARCH_URL=$(OPENSEARCH_URL) cargo test --test integration --test catalogs --test collections --test validation; status=$$?; \
	curl -sf -XDELETE '$(OPENSEARCH_URL)/it-*' > /dev/null 2>&1 || true; \
	exit $$status

test-unit: ## Unit tests only (no OpenSearch needed)
	cargo test --lib

test-clean: ## Delete leftover it-* test indices without running tests
	curl -sf -XDELETE '$(OPENSEARCH_URL)/it-*' || true

# Vendored sfeos API suite (tests/upstream). Runs against a live API — start
# one first via `make up`, or `make opensearch` + `cargo run`.
test-upstream: ## Run vendored upstream sfeos API tests against the API on :3000
	@test -d tests/upstream/.venv || python3 -m venv tests/upstream/.venv
	@tests/upstream/.venv/bin/pip install -q -r tests/upstream/requirements.txt
	@curl -sf $(STAC_API_URL)/ > /dev/null || { echo "API not reachable at $(STAC_API_URL) — run 'make up' first"; exit 1; }
	STAC_API_URL=$(STAC_API_URL) OPENSEARCH_URL=$(OPENSEARCH_URL) \
		tests/upstream/.venv/bin/pytest tests/upstream -v

# Validation-gated upstream tests need an API started with the flags
# on — two phases, since validator tests PATCH without If-Match while
# the require_if_match tests need the 428 enforcement. Each phase
# launches its own :3000 instance (fails fast if one's already up),
# runs its slice with UPSTREAM_VALIDATED=1, then tears it down. The
# binary is launched directly (not `cargo run`, whose child process
# would survive `kill`) and `wait`ed after each phase so the port is
# actually free before the next server binds.
test-upstream-validated: opensearch ## Upstream validation tests against a validated API instance
	@test -d tests/upstream/.venv || python3 -m venv tests/upstream/.venv
	@tests/upstream/.venv/bin/pip install -q -r tests/upstream/requirements.txt
	@if curl -sf $(STAC_API_URL)/ > /dev/null 2>&1; then \
		echo "API already running on :3000 — stop it first (make down)"; \
		exit 1; \
	fi
	cargo build
	@ENABLE_STAC_VALIDATOR=true ENABLE_TOPOLOGY_VALIDATION=true \
		ENABLE_TRANSACTIONS_EXTENSIONS=true OPENSEARCH_URL=$(OPENSEARCH_URL) \
		./target/debug/stac-os-rs > /tmp/stac-os-rs-validated-1.log 2>&1 & \
	apid=$$!; \
	trap 'kill $$apid 2>/dev/null' EXIT; \
	for i in $$(seq 1 60); do \
		curl -sf $(STAC_API_URL)/ > /dev/null 2>&1 && break; \
		sleep 1; \
	done; \
	curl -sf $(STAC_API_URL)/ > /dev/null 2>&1 || { \
		echo "validated API failed to start — see /tmp/stac-os-rs-validated-1.log"; \
		exit 1; \
	}; \
	UPSTREAM_VALIDATED=1 STAC_API_URL=$(STAC_API_URL) OPENSEARCH_URL=$(OPENSEARCH_URL) \
		tests/upstream/.venv/bin/pytest tests/upstream/api/test_api_stac_validator.py -v; \
	status=$$?; \
	kill $$apid 2>/dev/null; wait $$apid 2>/dev/null; \
	REQUIRE_IF_MATCH=true ENABLE_TRANSACTIONS_EXTENSIONS=true \
		OPENSEARCH_URL=$(OPENSEARCH_URL) \
		./target/debug/stac-os-rs > /tmp/stac-os-rs-validated-2.log 2>&1 & \
	apid=$$!; \
	for i in $$(seq 1 60); do \
		curl -sf $(STAC_API_URL)/ > /dev/null 2>&1 && break; \
		sleep 1; \
	done; \
	curl -sf $(STAC_API_URL)/ > /dev/null 2>&1 || { \
		echo "if-match API failed to start — see /tmp/stac-os-rs-validated-2.log"; \
		exit $$((status + 1)); \
	}; \
	UPSTREAM_VALIDATED=1 STAC_API_URL=$(STAC_API_URL) OPENSEARCH_URL=$(OPENSEARCH_URL) \
		tests/upstream/.venv/bin/pytest tests/upstream/api/test_conditional_requests.py -v \
			-k "test_require_if_match"; \
	status=$$((status + $$?)); \
	kill $$apid 2>/dev/null || true; \
	exit $$status

ingest: ## Load sample_data into the running API
	STAC_API_URL=$(STAC_API_URL) python3 scripts/ingest_sample_data.py

# Needs: API on :3000 + sample data ingested + `pip install stac-api-validator`
validate: ## Run stac-api-validator (core, item-search, features, browseable)
	stac-api-validator \
		--root-url $(STAC_API_URL) \
		--conformance core \
		--conformance item-search \
		--conformance features \
		--conformance browseable \
		--collection landsat-c2-l2 \
		--geometry '{"type":"Polygon","coordinates":[[[-123,36],[-119,36],[-119,39],[-123,39],[-123,36]]]}'

check: ## cargo fmt check + clippy + test-compile
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings
	cargo test --no-run

fmt: ## Auto-format
	cargo fmt
