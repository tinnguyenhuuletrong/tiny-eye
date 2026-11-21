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
build: ## Release build of the tiny-eye binary
	cargo build --release --bin tiny-eye

.PHONY: run
run: tools ## Run a single node with config/dev.yaml (local disk)
	cargo run --release --bin tiny-eye -- --config config/dev.yaml

.PHONY: schemas
schemas: ## Regenerate doc/schema (JSON Schemas for TQS/dashboards/config, MCP tools, formula grammar)
	cargo run --quiet --bin tiny-eye -- schemas --out doc/schema

.PHONY: fmt
fmt: ## Format all code
	cargo fmt --all

.PHONY: lint
lint: ## fmt check + clippy (warnings are errors)
	cargo fmt --all -- --check
	cargo clippy --workspace --all-targets -- -D warnings

## ---- test
.PHONY: test
test: tools ## Unit + integration tests (no external services)
	cargo test --workspace

.PHONY: poc
poc: tools ## Proof-of-concept storage tests (DuckLake, hot tail, compaction, retention, bloom)
	cargo test -p engine --test poc -- --nocapture

.PHONY: conformance
conformance: tools ## TQS vs Prometheus 3.16 point by point + validation + mutation check
	TQS_COVERAGE_OUT=$(ROOT)/doc/results/tqs-coverage.md cargo test --release -p tqs --features mutants --test conformance

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
check: lint test conformance ## Everything CI runs (test includes e2e vs Prometheus and the schema sync check)

## ---- demo (examples/demo/README.md)
.PHONY: demo demo-app demo-load
demo: tools build ## Run tiny-eye with the demo config (dashboards from examples/demo)
	./target/release/tiny-eye --config examples/demo/tiny-eye.yaml

demo-app: ## Run the demo shop: shop-api :3000 + payments :3001 (Node.js, OpenTelemetry)
	cd examples/demo/app && npm install --no-audit --no-fund && npm start

demo-load: ## k6 load against the demo shop via Docker (DURATION=10m for longer)
	docker run --rm -i --add-host=host.docker.internal:host-gateway -e BASE_URL=http://host.docker.internal:3000 \
	  -e DURATION=$(or $(DURATION),3m) grafana/k6:1.4.2 run - < examples/demo/k6/load.js

## ---- benchmark
.PHONY: bench
bench: tools ## Ingest + query + trace-lookup benchmark (local disk); writes doc/results/bench-local.md
	cargo run --release -p server --bin tiny-eye-bench -- --out doc/results/bench-local.md

.PHONY: bench-s3
bench-s3: tools s3-up ## Same benchmark with data on MinIO; writes doc/results/bench-s3.md
	cargo run --release -p server --bin tiny-eye-bench -- --s3 http://127.0.0.1:19000 --out doc/results/bench-s3.md

.PHONY: clean
clean: ## Remove build output and local state
	cargo clean; rm -rf .state data
