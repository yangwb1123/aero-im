# Aero IM — developer Makefile
# Common workflows so engineers don't have to memorize cargo/docker flags.

SHELL := /usr/bin/env bash
.DEFAULT_GOAL := help

##@ Setup
.PHONY: help
help: ## show this help
	@awk 'BEGIN {FS = ":.*##"; printf "\nUsage:\n  make \033[36m<target>\033[0m\n"} /^[a-zA-Z_-]+:.*?##/ { printf "  \033[36m%-20s\033[0m %s\n", $$1, $$2 } /^##@/ { printf "\n\033[1m%s\033[0m\n", substr($$0, 5) }' $(MAKEFILE_LIST)

.PHONY: jwt-keys
jwt-keys: ## generate RSA jwt keys in ./secrets/
	@bash scripts/gen-jwt-keys.sh

.PHONY: env
env: ## copy example config + env files (won't overwrite)
	@[ -f config.toml ] || cp config.example.toml config.toml
	@[ -f .env ] || cp .env.example .env
	@echo "✓ config.toml and .env present"

##@ Services
.PHONY: up
up: ## start docker dev services (postgres, redis, nats, jaeger, minio)
	docker compose up -d

.PHONY: down
down: ## stop docker dev services
	docker compose down

.PHONY: logs
logs: ## tail docker dev service logs
	docker compose logs -f --tail=50

.PHONY: ps
ps: ## list running docker services
	docker compose ps

##@ Build / Test
.PHONY: check
check: ## cargo check the whole workspace
	cargo check --workspace --all-targets

.PHONY: build
build: ## cargo build (debug)
	cargo build --workspace

.PHONY: test
test: ## cargo test, ignoring integration tests
	cargo test --workspace --lib

.PHONY: test-all
test-all: ## cargo test including ignored (requires services)
	cargo test --workspace -- --include-ignored

.PHONY: fmt
fmt: ## rustfmt all first-party crates (vendored MSRV snapshots are immutable)
	@set -e; for manifest in crates/*/Cargo.toml; do \
		cargo fmt --manifest-path "$$manifest"; \
	done

.PHONY: lint
lint: ## clippy
	cargo clippy --workspace --all-targets -- -D warnings

.PHONY: check-truth
check-truth: ## 检测死代码（孤儿模块 + 零调用 builder）
	@bash scripts/truth-check.sh

.PHONY: check-web
check-web: ## 前端完整性校验
	@bash scripts/web-check.sh

##@ Aero CLI
.PHONY: gate
gate: ## Run all gates via aero-cli
	cargo run --bin aero-cli -- gate all

.PHONY: doctor
doctor: ## Environment diagnostics
	cargo run --bin aero-cli -- doctor

.PHONY: skill-list
skill-list: ## List available skills
	cargo run --bin aero-cli -- skill list

.PHONY: ci
ci: ## Full CI pipeline: check + gates
	cargo run --bin aero-cli -- check
	cargo run --bin aero-cli -- gate all

.PHONY: ci-full
ci-full: ci ## Full CI + integration tests (requires PostgreSQL)
	bash scripts/test-integration.sh

.PHONY: dev
dev: ## Start development environment
	cargo run --bin aero-cli -- dev

.PHONY: completion
completion: ## Generate shell completion
	cargo run --bin aero-cli -- completion bash

##@ Git Hooks
.PHONY: install-hooks
install-hooks: ## Install git pre-commit hooks
	git config core.hooksPath .githooks
	@echo "✓ Git hooks installed (pre-commit: aero-cli check before each commit)"

##@ Run
.PHONY: migrate
migrate: ## apply DB migrations
	cargo run --bin aero-cli -- migrate

.PHONY: migrate-smoke
migrate-smoke: ## replay every migration on a throwaway DB (fresh-deploy chain check)
	PSQL="docker exec -i aero-postgres psql -U aero" scripts/migrate_chain_smoke.sh

.PHONY: run
run: ## run the server (foreground)
	cargo run --bin aero-server

.PHONY: dev
dev: env jwt-keys up migrate run ## one-shot: copy env, gen keys, start services, migrate, run

##@ Misc
.PHONY: clean
clean: ## cargo clean + remove docker volumes
	cargo clean
	rm -rf ./data
