#  sendme-balloon — Makefile
#
#  Development workflow.  The full release lifecycle is handled by
#  scripts/release.sh — run `make release V=0.1.0` to invoke it.
#

# ── Configuration ──────────────────────────────────────────────────────────

PROJECT := sendme
TARGET  := x86_64-unknown-linux-gnu
VERSION := $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)

# Terminal colours (disabled if not a tty).
ifeq ($(shell test -t 1 && echo yes),yes)
  BOLD  := \033[1m
  GREEN := \033[32m
  CYAN  := \033[36m
  RESET := \033[0m
else
  BOLD  :=
  GREEN :=
  CYAN  :=
  RESET :=
endif

.DEFAULT_GOAL := help

# ── Help ───────────────────────────────────────────────────────────────────

.PHONY: help
help: ## Show this help
	@printf "$(BOLD)sendme-balloon — Makefile$(RESET)\n\n"
	@printf "$(BOLD)Development:$(RESET)\n"
	@awk 'BEGIN {FS = ":.*?## "} /^[a-zA-Z_-]+:.*?## .*Dev/ {printf "  $(GREEN)%-20s$(RESET) %s\n", $$1, $$2}' $(MAKEFILE_LIST)
	@printf "\n$(BOLD)Testing & quality:$(RESET)\n"
	@awk 'BEGIN {FS = ":.*?## "} /^[a-zA-Z_-]+:.*?## .*Test/ {printf "  $(GREEN)%-20s$(RESET) %s\n", $$1, $$2}' $(MAKEFILE_LIST)
	@printf "\n$(BOLD)Release:$(RESET)\n"
	@awk 'BEGIN {FS = ":.*?## "} /^[a-zA-Z_-]+:.*?## .*Release/ {printf "  $(GREEN)%-20s$(RESET) %s\n", $$1, $$2}' $(MAKEFILE_LIST)
	@printf "\n$(BOLD)Container:$(RESET)\n"
	@awk 'BEGIN {FS = ":.*?## "} /^[a-zA-Z_-]+:.*?## .*Docker/ {printf "  $(GREEN)%-20s$(RESET) %s\n", $$1, $$2}' $(MAKEFILE_LIST)
	@printf "\n$(CYAN)Version: $(VERSION)$(RESET)\n"

# ── Development ─────────────────────────────────────────────────────────────

.PHONY: build
build: ## Dev — compile debug CLI
	cargo build

.PHONY: build-balloon
build-balloon: ## Dev — compile the balloon GUI (needs GUI libs on Linux)
	cargo build --features balloon --bin sendme-balloon

.PHONY: run
run: ## Dev — run the CLI in debug mode (pass ARGS="send file.txt")
	cargo run -- $(ARGS)

.PHONY: clean
clean: ## Dev — remove build artefacts
	cargo clean

# ── Testing & quality ─────────────────────────────────────────────────────

.PHONY: test
test: ## Test — run all tests
	cargo test --all-features

.PHONY: test-cli
test-cli: ## Test — run tests without the balloon feature
	cargo test

.PHONY: lint
lint: ## Test — fmt check + clippy
	cargo fmt --all -- --check
	cargo clippy --all-features --all-targets -- -D warnings

.PHONY: fmt
fmt: ## Test — auto-format code
	cargo fmt --all

.PHONY: check
check: ## Test — type-check all features
	cargo check --all-features

# ── Release ────────────────────────────────────────────────────────────────

.PHONY: release
release: ## Release — interactive: shows current version, asks for next, does everything
	./scripts/release.sh

.PHONY: build-release
build-release: ## Release — optimised CLI binary only (no release process)
	cargo build --release --target $(TARGET) --bin $(PROJECT)

.PHONY: build-release-balloon
build-release-balloon: ## Release — optimised balloon binary only (no release process)
	cargo build --release --features balloon --target $(TARGET) --bin sendme-balloon

# ── Container ───────────────────────────────────────────────────────────────

.PHONY: docker-build
docker-build: ## Docker — build the container image locally
	docker build --tag ghcr.io/imp1sh/sendme-balloon:$(VERSION) .

.PHONY: docker-run
docker-run: ## Docker — run the CLI in a throwaway container (pass ARGS="--help")
	docker run --rm -it ghcr.io/imp1sh/sendme-balloon:$(VERSION) $(ARGS)
