# tiny-eye — every developer task is a make target. `make help` lists them.
SHELL := /bin/bash
.DEFAULT_GOAL := help
export PATH := $(HOME)/.cargo/bin:$(PATH)

ROOT        := $(abspath .)
TOOLS       := $(ROOT)/.tools
DUCKDB_VER  := v2.0.1
PROM_VER    := 3.16.0
OS          := $(shell uname -s | tr A-Z a-z)
ARCH        := $(shell uname -m | sed 's/x86_64/amd64/; s/aarch64/arm64/')
DUCK_PLAT   := $(if $(filter darwin,$(OS)),osx_$(ARCH),linux_$(ARCH))
COMPOSE     := docker compose -f deploy/docker-compose.yml
export TINY_EYE_DUCKDB_EXT := $(TOOLS)/duckdb_ext

.PHONY: help
help: ## List targets
	@grep -hE '^[a-zA-Z0-9_-]+:.*?## ' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-14s\033[0m %s\n", $$1, $$2}'

## ---- setup
# run, demo and machine need only the DuckDB extensions (ext); the test suites also start the
# pinned Prometheus (tools). Both are files: once downloaded, nothing is fetched again.
EXT := $(TOOLS)/duckdb_ext/$(DUCKDB_VER)/$(DUCK_PLAT)/ducklake.duckdb_extension
.PHONY: tools ext
tools: ext $(TOOLS)/prometheus ## Download the pinned DuckDB extensions + Prometheus (for the tests) into .tools/
	@echo "tools ready in $(TOOLS)"

ext: $(EXT) ## Download only the pinned DuckDB extensions into .tools/ (all a node needs to run)

$(TOOLS)/prometheus:
	mkdir -p $(TOOLS) && cd $(TOOLS) && \
	curl -sSfL -o prom.tgz https://github.com/prometheus/prometheus/releases/download/v$(PROM_VER)/prometheus-$(PROM_VER).$(OS)-$(ARCH).tar.gz && \
	tar xzf prom.tgz && cp prometheus-$(PROM_VER).$(OS)-$(ARCH)/{prometheus,promtool} . && rm -rf prom.tgz prometheus-$(PROM_VER).*

# ducklake comes last: its file (the target) exists only once all four are in place.
$(EXT):
	set -o pipefail; mkdir -p $(dir $@) && for e in httpfs icu postgres_scanner ducklake; do \
	  curl -sSfL https://extensions.duckdb.org/$(DUCKDB_VER)/$(DUCK_PLAT)/$$e.duckdb_extension.gz | gunzip > $(dir $@)$$e.part && \
	  mv $(dir $@)$$e.part $(dir $@)$$e.duckdb_extension || exit 1; done

.PHONY: hooks
hooks: ## Install the git pre-commit hook (fmt + the same clippy as make lint, sharing its build cache)
	git config core.hooksPath .githooks

## ---- develop
.PHONY: build
build: ui ## Release build of the tiny-eye binary, web UI included
	cargo build --release --bin tiny-eye

.PHONY: run
run: ext build ## Run a single node with config/dev.yaml (local disk); UI at http://127.0.0.1:7070
	./target/release/tiny-eye --config config/dev.yaml

## ---- web UI (ui/) and docs site (website/)
# ui/dist is rebuilt only when a UI source, the lockfile or api/openapi.yaml changed; npm ci runs
# only when node_modules is missing or older than the lockfile. An unchanged ui/dist keeps its
# timestamps, so cargo does not rebuild the server to embed it again.
UI_SRC := $(filter-out ui/src/api.gen.ts,$(shell find ui/src -type f)) ui/index.html ui/package.json ui/tsconfig.json ui/vite.config.ts api/openapi.yaml
.PHONY: ui ui-dev docs docs-dev
ui: ui/dist/index.html ## Build the web UI into ui/dist (embedded by the next cargo build); API types generated from api/openapi.yaml

ui/node_modules/.package-lock.json: ui/package-lock.json
	cd ui && npm ci --no-audit --no-fund
	touch $@

ui/dist/index.html: ui/node_modules/.package-lock.json $(UI_SRC)
	cd ui && npm run build

ui-dev: ## UI dev server on :5173 with hot reload, proxying the API to a running node (make run)
	cd ui && npm install --no-audit --no-fund && npm run dev

docs: ## Build the documentation site (docs/ -> website/build)
	cd website && npm ci --no-audit --no-fund && BASE_URL=/ npm run build

docs-dev: ## Preview the documentation site on :3000
	cd website && npm install --no-audit --no-fund && BASE_URL=/ npm start

.PHONY: media
media: ## Re-record the README/docs GIFs and videos (needs Chrome + ffmpeg; getting-started needs make demo, demo-app, demo-load running)
	node docs/assets/media/record.mjs how-it-works
	node docs/assets/media/record.mjs materialized-panels
	node docs/assets/media/record.mjs getting-started

.PHONY: schemas
schemas: ## Regenerate schema/generated (JSON Schemas, formula grammar) and docs/reference/mcp-tools.md
	cargo run --quiet --bin tiny-eye -- schemas --root .

.PHONY: fmt
fmt: ## Format all code
	cargo fmt --all

.PHONY: lint
lint: ## fmt check + clippy (warnings are errors)
	cargo fmt --all -- --check
	cargo clippy --workspace --all-targets --features tqs/mutants -- -D warnings

## ---- test
.PHONY: test
test: tools ## Unit + integration tests (no external services)
	cargo test --workspace

.PHONY: poc
poc: tools ## Proof-of-concept storage tests (DuckLake, hot tail, compaction, retention, bloom)
	cargo test -p engine --test poc -- --nocapture

.PHONY: conformance
conformance: tools ## TQS vs Prometheus 3.16 point by point + validation + mutation check
	TQS_COVERAGE_OUT=$(ROOT)/docs/deep-dive/evidence/tqs-coverage.md cargo test --release -p tqs --features mutants --test conformance

.PHONY: e2e
e2e: tools ## End-to-end: remote-write the same samples to Prometheus and tiny-eye, compare every panel
	cargo test -p server --test e2e -- --nocapture

.PHONY: s3-up s3-down
s3-up: ## Start MinIO (S3) on :19000 with bucket tiny-eye, for S3-mode benchmarks
	$(COMPOSE) up -d
	@for i in $$(seq 1 60); do curl -sf http://127.0.0.1:19000/minio/health/live >/dev/null && break; sleep 0.5; done
	@curl -s -o /dev/null --aws-sigv4 "aws:amz:us-east-1:s3" --user tinyeye:tinyeye-secret -X PUT http://127.0.0.1:19000/tiny-eye
	@echo "minio on :19000, bucket tiny-eye"
s3-down: ## Stop MinIO and drop its volume
	$(COMPOSE) down -v

# The same steps as .github/workflows/ci.yml, in its order: ui (Node.js), lint (rustfmt, clippy),
# examples-lint (Bun), test (unit + integration tests, including e2e against Prometheus and the
# check that generated files are up to date), conformance, and docs (Node.js; fails on broken links).
.PHONY: check
check: ui lint examples-lint test conformance docs ## Everything CI runs: ui, lint, examples-lint, test, conformance, docs (needs Node.js and Bun)

## ---- container
.PHONY: docker
docker: ## Build the container image tiny-eye:dev (UI + binary + pinned DuckDB extensions)
	docker build -t tiny-eye:dev .

## ---- demo (examples/demo/README.md)
.PHONY: demo demo-app demo-load machine machine-agent machine-hog
demo: ext build ## Run tiny-eye with the demo config (dashboards from examples/demo); UI at http://127.0.0.1:7070
	./target/release/tiny-eye --config examples/demo/tiny-eye.yaml

demo-app: ## Run the demo shop: shop-api :3000 + payments :3001 (Bun, OpenTelemetry)
	cd examples/demo/app && bun install && bun start

.PHONY: examples-lint
examples-lint: ## Lint, format check and typecheck the TypeScript examples (Biome + tsc, needs Bun)
	for d in examples/demo/app examples/machine; do (cd $$d && bun install --frozen-lockfile && bun run check) || exit 1; done

demo-load: ## k6 load against the demo shop via Docker (DURATION=10m for longer)
	docker run --rm -i --add-host=host.docker.internal:host-gateway -e BASE_URL=http://host.docker.internal:3000 \
	  -e DURATION=$(or $(DURATION),3m) grafana/k6:1.4.2 run - < examples/demo/k6/load.js

machine: ext build ## Run tiny-eye for the machine example (examples/machine): UI at http://127.0.0.1:7070
	./target/release/tiny-eye --config examples/machine/tiny-eye.yaml

machine-agent: ## Send this computer's CPU, memory, network, disk and top processes to tiny-eye (Bun, no runtime deps)
	bun examples/machine/agent.ts

machine-hog: ## Start a runaway process that pins one CPU core (stop it with: pkill -f runaway-job)
	sh examples/machine/runaway-job.sh

## ---- benchmark
.PHONY: bench
bench: tools ## Ingest + query + trace-lookup benchmark (local disk); writes docs/deep-dive/evidence/bench-local.md
	cargo run --release -p server --bin tiny-eye-bench -- --out docs/deep-dive/evidence/bench-local.md

.PHONY: bench-s3
bench-s3: tools s3-up ## Same benchmark with data on MinIO; writes docs/deep-dive/evidence/bench-s3.md
	cargo run --release -p server --bin tiny-eye-bench -- --s3 http://127.0.0.1:19000 --out docs/deep-dive/evidence/bench-s3.md

.PHONY: bench-all
bench-all: ## Every benchmark, one after the other; what .github/workflows/bench.yml runs weekly. Each report starts with a run block
	$(MAKE) bench
	$(MAKE) bench-s3
	$(MAKE) fleet-poc

.PHONY: fleet-poc
fleet-poc: tools s3-up ## Fleet proof of concept: nodes sharing one bucket (MinIO) query each other's data; writes docs/deep-dive/evidence/fleet-poc.md
	cargo run --release -p server --bin tiny-eye-fleet -- --s3 http://127.0.0.1:19000 --out docs/deep-dive/evidence/fleet-poc.md

.PHONY: clean
clean: ## Remove build output and local state
	cargo clean; rm -rf .state data
