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
	@grep -hE '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-14s\033[0m %s\n", $$1, $$2}'

## ---- setup
.PHONY: tools
tools: $(TOOLS)/prometheus $(TOOLS)/duckdb_ext/$(DUCKDB_VER)/$(DUCK_PLAT)/ducklake.duckdb_extension ## Download pinned Prometheus + DuckDB extensions into .tools/
	@echo "tools ready in $(TOOLS)"

$(TOOLS)/prometheus:
	mkdir -p $(TOOLS) && cd $(TOOLS) && \
	curl -sSfL -o prom.tgz https://github.com/prometheus/prometheus/releases/download/v$(PROM_VER)/prometheus-$(PROM_VER).$(OS)-$(ARCH).tar.gz && \
	tar xzf prom.tgz && cp prometheus-$(PROM_VER).$(OS)-$(ARCH)/{prometheus,promtool} . && rm -rf prom.tgz prometheus-$(PROM_VER).*

$(TOOLS)/duckdb_ext/$(DUCKDB_VER)/$(DUCK_PLAT)/ducklake.duckdb_extension:
	mkdir -p $(dir $@) && for e in ducklake httpfs icu postgres_scanner; do \
	  curl -sSfL http://extensions.duckdb.org/$(DUCKDB_VER)/$(DUCK_PLAT)/$$e.duckdb_extension.gz | gunzip > $(dir $@)$$e.duckdb_extension; done

.PHONY: hooks
hooks: ## Install the git pre-commit hook (fmt + clippy)
	git config core.hooksPath .githooks

## ---- develop
.PHONY: build
build: ui ## Release build of the tiny-eye binary, web UI included
	cargo build --release --bin tiny-eye

.PHONY: run
run: tools build ## Run a single node with config/dev.yaml (local disk); UI at http://127.0.0.1:7070
	./target/release/tiny-eye --config config/dev.yaml

## ---- web UI (ui/) and docs site (website/)
.PHONY: ui ui-dev docs docs-dev
ui: ## Build the web UI into ui/dist (embedded by the next cargo build); API types generated from api/openapi.yaml
	cd ui && npm ci --no-audit --no-fund && npm run build

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

.PHONY: check
check: lint ui test conformance ## Everything CI runs (test includes e2e vs Prometheus and the schema sync check)

## ---- container
.PHONY: docker
docker: ## Build the container image tiny-eye:dev (UI + binary + pinned DuckDB extensions)
	docker build -t tiny-eye:dev .

## ---- demo (examples/demo/README.md)
.PHONY: demo demo-app demo-load machine machine-agent machine-hog
demo: tools build ## Run tiny-eye with the demo config (dashboards from examples/demo); UI at http://127.0.0.1:7070
	./target/release/tiny-eye --config examples/demo/tiny-eye.yaml

demo-app: ## Run the demo shop: shop-api :3000 + payments :3001 (Node.js, OpenTelemetry)
	cd examples/demo/app && npm install --no-audit --no-fund && npm start

demo-load: ## k6 load against the demo shop via Docker (DURATION=10m for longer)
	docker run --rm -i --add-host=host.docker.internal:host-gateway -e BASE_URL=http://host.docker.internal:3000 \
	  -e DURATION=$(or $(DURATION),3m) grafana/k6:1.4.2 run - < examples/demo/k6/load.js

machine: tools build ## Run tiny-eye for the machine example (examples/machine): UI at http://127.0.0.1:7070
	./target/release/tiny-eye --config examples/machine/tiny-eye.yaml

machine-agent: ## Send this computer's CPU, memory, network, disk and top processes to tiny-eye (Node.js, no deps)
	node examples/machine/agent.mjs

machine-hog: ## Start a runaway process that pins one CPU core (stop it with: pkill -f runaway-job)
	sh examples/machine/runaway-job.sh

## ---- benchmark
.PHONY: bench
bench: tools ## Ingest + query + trace-lookup benchmark (local disk); writes docs/deep-dive/evidence/bench-local.md
	cargo run --release -p server --bin tiny-eye-bench -- --out docs/deep-dive/evidence/bench-local.md

.PHONY: bench-s3
bench-s3: tools s3-up ## Same benchmark with data on MinIO; writes docs/deep-dive/evidence/bench-s3.md
	cargo run --release -p server --bin tiny-eye-bench -- --s3 http://127.0.0.1:19000 --out docs/deep-dive/evidence/bench-s3.md

.PHONY: clean
clean: ## Remove build output and local state
	cargo clean; rm -rf .state data
