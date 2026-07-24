#!/usr/bin/env bash
#
# sendme-balloon — release script
#
# Automates the full release lifecycle from a clean main branch:
#   1.  validate prerequisites and repository state
#   2.  bump version in Cargo.toml + Cargo.lock
#   3.  lint and test
#   4.  build release binaries (CLI + balloon GUI)
#   5.  package tarballs
#   6.  build and push container image to GHCR
#   7.  commit, tag, and push
#   8.  create a GitHub Release with downloadable tarballs
#
# Usage:
#   ./scripts/release.sh 0.1.0
#
# Prerequisites:
#   - Rust toolchain (rustup)
#   - Docker (logged in to ghcr.io)
#   - GitHub CLI (gh), authenticated
#
# Authenticate once before first use:
#   gh auth login
#   echo "$GHCR_TOKEN" | docker login ghcr.io -u YOUR_GITHUB_USERNAME --password-stdin
#

set -euo pipefail

# ── Colours ─────────────────────────────────────────────────────────────────
if [[ -t 1 ]]; then
    BOLD='\033[1m'; GREEN='\033[32m'; RED='\033[31m'; YELLOW='\033[33m'; CYAN='\033[36m'; RESET='\033[0m'
else
    BOLD=''; GREEN=''; RED=''; YELLOW=''; CYAN=''; RESET=''
fi

info()  { printf "${CYAN}▶${RESET} %s\n" "$*"; }
ok()    { printf "${GREEN}✓${RESET} %s\n" "$*"; }
warn()  { printf "${YELLOW}⚠${RESET} %s\n" "$*"; }
fail()  { printf "${RED}✗${RESET} %s\n" "$*" >&2; exit 1; }

# ── Arguments ───────────────────────────────────────────────────────────────
VERSION="${1:-}"
[[ -z "$VERSION" ]] && fail "Usage: $0 <version>   e.g. $0 0.1.0"

# Strip a leading 'v' if the user typed one.
VERSION="${VERSION#v}"

REPO_OWNER="imp1sh"
REPO_NAME="sendme-balloon"
IMAGE="ghcr.io/${REPO_OWNER}/${REPO_NAME}"
TARGET="x86_64-unknown-linux-gnu"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

cd "$ROOT_DIR"

# ── Step 1: Prerequisites ──────────────────────────────────────────────────
info "Step 1/8: checking prerequisites"

command -v cargo >/dev/null 2>&1 || fail "cargo not found — install Rust: https://rustup.rs"
command -v docker >/dev/null 2>&1 || fail "docker not found — install Docker: https://docs.docker.com/get-docker/"
command -v gh >/dev/null 2>&1 || fail "gh CLI not found — install: https://cli.github.com/"

gh auth status >/dev/null 2>&1 || fail "gh not authenticated — run: gh auth login"

ok "all prerequisites satisfied"

# ── Step 2: Repository state ────────────────────────────────────────────────
info "Step 2/8: validating repository state"

BRANCH="$(git branch --show-current)"
[[ "$BRANCH" == "main" ]] || fail "not on main (currently on '$BRANCH') — merge your feature branch first"

[[ -z "$(git status --porcelain)" ]] || fail "working tree is dirty — commit or stash first"

git fetch origin --quiet
LOCAL=$(git rev-parse HEAD)
REMOTE=$(git rev-parse origin/main)
[[ "$LOCAL" == "$REMOTE" ]] || fail "local main is not in sync with origin — push or pull first"

TAG="v${VERSION}"
if git rev-parse "$TAG" >/dev/null 2>&1; then
    fail "tag $TAG already exists"
fi

ok "on main, clean tree, in sync with origin, tag $TAG is free"

# ── Step 3: Version bump ───────────────────────────────────────────────────
info "Step 3/8: bumping version to ${VERSION}"

CURRENT_VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)

if [[ "$CURRENT_VERSION" == "$VERSION" ]]; then
    warn "Cargo.toml already at ${VERSION} — skipping bump"
else
    sed -i "s/^version = .*/version = \"${VERSION}\"/" Cargo.toml
    cargo update -p sendme --precise "$VERSION" 2>/dev/null || true
    ok "bumped Cargo.toml and Cargo.lock to ${VERSION}"
fi

# ── Step 4: Lint and test ──────────────────────────────────────────────────
info "Step 4/8: running lint and tests"

cargo fmt --all -- --check
ok "fmt clean"

cargo clippy --all-features --all-targets -- -D warnings
ok "clippy clean"

cargo test --all-features --bins --tests
ok "tests pass"

# ── Step 5: Build release binaries ──────────────────────────────────────────
info "Step 5/8: building release binaries"

cargo build --release --target "$TARGET" --bin sendme
ok "CLI binary built"

cargo build --release --features balloon --target "$TARGET" --bin sendme-balloon
ok "balloon binary built"

# ── Step 6: Package tarballs ───────────────────────────────────────────────
info "Step 6/8: packaging tarballs"

DIST="$ROOT_DIR/dist"
rm -rf "$DIST"
mkdir -p "$DIST"

tar czf "$DIST/sendme-v${VERSION}-linux-amd64.tar.gz" \
    -C "target/$TARGET/release" sendme
ok "packaged sendme-v${VERSION}-linux-amd64.tar.gz"

tar czf "$DIST/sendme-balloon-v${VERSION}-linux-amd64.tar.gz" \
    -C "target/$TARGET/release" sendme-balloon
ok "packaged sendme-balloon-v${VERSION}-linux-amd64.tar.gz"

# ── Step 7: Container image ────────────────────────────────────────────────
info "Step 7/8: building and pushing container image"

GIT_SHA=$(git rev-parse --short HEAD)

docker build \
    --tag "${IMAGE}:${VERSION}" \
    --tag "${IMAGE}:latest" \
    --tag "${IMAGE}:sha-${GIT_SHA}" \
    .

ok "container image built"

docker push "${IMAGE}:${VERSION}"
docker push "${IMAGE}:latest"
docker push "${IMAGE}:sha-${GIT_SHA}"
ok "pushed ${IMAGE}:${VERSION}, :latest, :sha-${GIT_SHA}"

# ── Step 8: Commit, tag, push, release ──────────────────────────────────────
info "Step 8/8: committing, tagging, and creating GitHub release"

git add Cargo.toml Cargo.lock
git commit -m "release v${VERSION}" --allow-empty || warn "nothing to commit (version already set)"
ok "committed"

git tag -a "$TAG" -m "Release $TAG"
git push origin main
git push origin "$TAG"
ok "pushed commit and tag $TAG"

gh release create "$TAG" \
    --title "$TAG" \
    --generate-notes \
    "$DIST/sendme-v${VERSION}-linux-amd64.tar.gz" \
    "$DIST/sendme-balloon-v${VERSION}-linux-amd64.tar.gz"

ok "GitHub release created: https://github.com/${REPO_OWNER}/${REPO_NAME}/releases/tag/${TAG}"

# ── Summary ────────────────────────────────────────────────────────────────
echo ""
printf "${BOLD}${GREEN}═══ Release ${VERSION} complete ═══${RESET}\n"
printf "  Binaries:  https://github.com/%s/%s/releases/tag/%s\n" "$REPO_OWNER" "$REPO_NAME" "$TAG"
printf "  Container: %s:%s\n" "$IMAGE" "$VERSION"
printf "  Container: %s:latest\n" "$IMAGE"
echo ""
