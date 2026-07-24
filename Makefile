#  sendme-balloon — Makefile
#
#  Common development and release workflow.
#  Run `make` or `make help` for a list of targets.
#

# ── Configuration ──────────────────────────────────────────────────────────

PROJECT   := sendme
REPO      := imp1sh/sendme-balloon
REGISTRY  := ghcr.io
IMAGE     := $(REGISTRY)/$(REPO)
TARGET    := x86_64-unknown-linux-gnu

# Derive the version from Cargo.toml.
VERSION   := $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
GIT_SHA   := $(shell git rev-parse --short HEAD 2>/dev/null || echo unknown)

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
help: ## Show available targets
	@printf "$(BOLD)sendme-balloon — Makefile$(RESET)\n\n"
	@printf "$(BOLD)Development:$(RESET)\n"
	@awk 'BEGIN {FS = ":.*?## "} /^[a-zA-Z_-]+:.*?## .*Dev/ {printf "  $(GREEN)%-20s$(RESET) %s\n", $$1, $$2}' $(MAKEFILE_LIST)
	@printf "\n$(BOLD)Testing & quality:$(RESET)\n"
	@awk 'BEGIN {FS = ":.*?## "} /^[a-zA-Z_-]+:.*?## .*Test/ {printf "  $(GREEN)%-20s$(RESET) %s\n", $$1, $$2}' $(MAKEFILE_LIST)
	@printf "\n$(BOLD)Release:$(RESET)\n"
	@awk 'BEGIN {FS = ":.*?## "} /^[a-zA-Z_-]+:.*?## .*Release/ {printf "  $(GREEN)%-20s$(RESET) %s\n", $$1, $$2}' $(MAKEFILE_LIST)
	@printf "\n$(BOLD)Container:$(RESET)\n"
	@awk 'BEGIN {FS = ":.*?## "} /^[a-zA-Z_-]+:.*?## .*Docker/ {printf "  $(GREEN)%-20s$(RESET) %s\n", $$1, $$2}' $(MAKEFILE_LIST)
	@printf "\n$(CYAN)Version: $(VERSION)  Image: $(IMAGE)$(RESET)\n"

# ── Development ─────────────────────────────────────────────────────────────

.PHONY: build
build: ## Dev — compile debug binary
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
release: ## Release — optimised CLI binary (amd64)
	cargo build --release --target $(TARGET) --bin sendme
	@printf "\n$(GREEN)Built:$RESET target/$(TARGET)/release/$(PROJECT)\n"

.PHONY: release-balloon
release-balloon: ## Release — optimised balloon GUI binary (amd64)
	cargo build --release --features balloon --target $(TARGET) --bin sendme-balloon
	@printf "\n$(GREEN)Built:$RESET target/$(TARGET)/release/sendme-balloon\n"

.PHONY: release-all
release-all: lint test release release-balloon ## Release — full local check (lint + test + both binaries)

.PHONY: package
package: release ## Release — tarball for distribution
	mkdir -p dist
	tar czf dist/$(PROJECT)-v$(VERSION)-linux-amd64.tar.gz \
	    -C target/$(TARGET)/release $(PROJECT)
	@printf "$(GREEN)Packaged:$RESET dist/$(PROJECT)-v$(VERSION)-linux-amd64.tar.gz\n"

.PHONY: bump-version
bump-version: ## Release — bump Cargo.toml version (usage: make bump-version V=0.37.0)
ifndef V
	$(error Usage: make bump-version V=<version>, e.g. make bump-version V=0.37.0)
endif
	sed -i 's/^version = .*/version = "$(V)"/' Cargo.toml
	@printf "$(GREEN)Bumped to $(V)$RESET — review, commit, then: make release-tag V=$(V)\n"

.PHONY: release-tag
release-tag: ## Release — create and push a git tag (usage: make release-tag V=0.37.0)
ifndef V
	$(error Usage: make release-tag V=<version>, e.g. make release-tag V=0.37.0)
endif
	@test "$$(git status --porcelain)" = "" || { echo "Working tree is dirty — commit first."; exit 1; }
	@git rev-parse v$(V) >/dev/null 2>&1 && { echo "Tag v$(V) already exists."; exit 1; } || true
	git tag -a v$(V) -m "Release v$(V)"
	git push origin v$(V)
	@printf "$(GREEN)Pushed tag v$(V)$RESET — CI will build the release and container image.\n"

# ── Container ───────────────────────────────────────────────────────────────

.PHONY: docker-build
docker-build: ## Docker — build the container image
	docker build \
	  --tag $(IMAGE):$(VERSION) \
	  --tag $(IMAGE):latest \
	  --tag $(IMAGE):sha-$(GIT_SHA) \
	  .
	@printf "$(GREEN)Image tags:$RESET $(IMAGE):$(VERSION), :latest, :sha-$(GIT_SHA)\n"

.PHONY: docker-push
docker-push: docker-build ## Docker — push image to GHCR (requires docker login first)
	docker push $(IMAGE):$(VERSION)
	docker push $(IMAGE):latest
	docker push $(IMAGE):sha-$(GIT_SHA)
	@printf "$(GREEN)Pushed to $(IMAGE)$RESET\n"

.PHONY: docker-login
docker-login: ## Docker — authenticate to GHCR
	echo "$$GHCR_TOKEN" | docker login $(REGISTRY) -u $(GHCR_USER) --password-stdin

.PHONY: docker-run
docker-run: ## Docker — run the CLI in a throwaway container (pass ARGS="--help")
	docker run --rm -it $(IMAGE):$(VERSION) $(ARGS)
