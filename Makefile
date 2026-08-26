# Aero IM — developer Makefile
# Common workflows so engineers don't have to memorize cargo/docker flags.

SHELL := /usr/bin/env bash
.DEFAULT_GOAL := help

##@ Setup
.PHONY: help
help: ## show this help
	@awk 'BEGIN {FS = ":.*##"; printf "\nUsage:\n  make \033[36m<target>\033[0m\n"} /^[a-zA-Z_-]+:.*?##/ { printf "  \033[36m%-20s\033[0m %s\n", $$1, $$2 } /^##@/ { printf "\n\033[1m%s\033[0m\n", substr($$0, 5) }' $(MAKEFILE_LIST)

.PHONY: jwt-keys
jwt-keys: ## generate RSA jwt keys in ./secrets/ if absent
	@bash scripts/gen-jwt-keys.sh

.PHONY: metrics-token
metrics-token: ## generate the shared /metrics bearer token if absent
	@bash scripts/gen-metrics-token.sh

.PHONY: runtime-secrets
runtime-secrets: jwt-keys metrics-token ## ensure Docker runtime secrets exist

.PHONY: env
env: ## copy example config + env files (won't overwrite)
	@[ -f config.toml ] || cp config.example.toml config.toml
	@[ -f .env ] || cp .env.example .env
	@echo "✓ config.toml and .env present"

##@ Services
.PHONY: up
up: ## start docker dev services (postgres, redis, nats, jaeger, minio)
	docker compose up -d

.PHONY: up-app
up-app: runtime-secrets ## build and start the complete Docker deployment
	docker compose --profile app up -d --build

.PHONY: up-observability
up-observability: runtime-secrets ## start app plus Prometheus, Alertmanager, and Grafana
	docker compose --profile app --profile observability up -d --build

.PHONY: docker-smoke
docker-smoke: ## verify deployment, recovery, observability, health, and relay
	@bash scripts/docker-deploy-smoke.sh

.PHONY: monitoring-check
monitoring-check: ## validate Prometheus, Alertmanager, and Grafana configuration
	@bash scripts/monitoring-config-check.sh

.PHONY: postgres-backup
postgres-backup: ## create an atomic custom-format backup from Compose PostgreSQL
	@bash scripts/postgres-backup.sh "$(BACKUP)"

.PHONY: postgres-restore-verify
postgres-restore-verify: ## restore DUMP into a disposable database and validate it
	@test -n "$(DUMP)" || { echo "usage: make postgres-restore-verify DUMP=path/to/aero.dump" >&2; exit 2; }
	@bash scripts/postgres-restore-verify.sh "$(DUMP)"

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

.PHONY: fmt-check
fmt-check: ## reject new rustfmt debt while tolerating reported legacy debt
	@bash scripts/fmt-check.sh

.PHONY: fmt-check-test
fmt-check-test: ## run hermetic positive/negative tests for incremental rustfmt guard
	@bash scripts/test-fmt-check.sh

.PHONY: migration-guard
migration-guard: ## reject edits/deletes of historical migrations
	@bash scripts/migration-immutability-check.sh

.PHONY: migration-guard-test
migration-guard-test: ## run hermetic positive/negative tests for migration guard
	@bash scripts/test-migration-immutability-check.sh

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
ci-full: ci ## Full CI + integration tests (requires PostgreSQL + Redis)
	bash scripts/test-integration.sh

.PHONY: test-notification-fanout
test-notification-fanout: ## notification fan-out DB-gated suite (throwaway DB + required Redis presence leg)
	@bash scripts/test-notification-fanout.sh

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
