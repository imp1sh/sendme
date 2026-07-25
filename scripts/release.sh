#!/usr/bin/env bash
#
# sendme-balloon — interactive release script (desktop, multi-platform)
#
# Usage:
#   make release
#   ./scripts/release.sh
#
# Performs the FULL release on your amd64 Linux desktop:
#   - validates repository state, lints, tests
#   - asks for the next version, bumps Cargo.toml
#   - builds EVERY target platform via hermetic containers (scripts/build.sh)
#     → linux (gnu/musl, amd64/arm64), windows, macos
#   - packages archives + unified SHA256SUMS
#   - builds & pushes the container image to GHCR
#   - commits, tags, pushes, and creates a GitHub Release with all downloads
#
# macOS (darwin) targets need the Apple SDK at ./sdks/MacOSX.sdk — run
# `make fetch-sdk X=<Xcode.xip>` once.  If absent, darwin targets are skipped
# (the release proceeds with the remaining platforms).
#
# Prerequisites (one-time):
#   gh auth login
#   podman installed
#

set -euo pipefail

# ── Colours ─────────────────────────────────────────────────────────────────
if [[ -t 1 ]]; then
    B='\033[1m'; G='\033[32m'; R='\033[31m'; Y='\033[33m'; C='\033[36m'; N='\033[0m'
else
    B=''; G=''; R=''; Y=''; C=''; N=''
fi
info()  { printf "${C}▶${N} %s\n" "$*"; }
ok()    { printf "${G}✓${N} %s\n" "$*"; }
warn()  { printf "${Y}⚠${N} %s\n" "$*"; }
die()   { printf "${R}✗${N} %s\n" "$*" >&2; exit 1; }

REPO_OWNER="imp1sh"
REPO_NAME="sendme-balloon"
IMAGE="ghcr.io/${REPO_OWNER}/${REPO_NAME}"
# Toolchain the release gate uses on the host.  Matches the pin in
# Dockerfile.cross so lint/test catches the same issues the container builds
# would (e.g. egui 0.35 requiring 1.92.0).
REQUIRED_TOOLCHAIN="1.92.0"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
DIST="$ROOT_DIR/dist"
cd "$ROOT_DIR"

# ── Helpers ─────────────────────────────────────────────────────────────────
bump_patch() {
    local v="$1"
    IFS='.' read -r major minor patch <<< "$v"
    echo "${major}.${minor}.$((patch + 1))"
}

# Return 0 if $1 >= $2 (semver x.y.z), else 1.
version_ge() {
    local IFS=.
    local a1 a2 a3 b1 b2 b3
    read -r a1 a2 a3 <<< "$1"
    read -r b1 b2 b3 <<< "$2"
    a1=${a1:-0}; a2=${a2:-0}; a3=${a3:-0}
    b1=${b1:-0}; b2=${b2:-0}; b3=${b3:-0}
    (( a1 > b1 )) && return 0
    (( a1 < b1 )) && return 1
    (( a2 > b2 )) && return 0
    (( a2 < b2 )) && return 1
    (( a3 >= b3 ))
}

# Ensure a suitable Rust toolchain is available on the host.  Installs rustup
# + the pinned toolchain if cargo is absent; otherwise ensures the pinned
# toolchain is installed (via rustup) and selected for this session without
# disturbing the user's global default.  Falls back to a version check for
# non-rustup cargo installations.
ensure_rust() {
    if ! command -v cargo >/dev/null 2>&1; then
        info "cargo not found — installing rustup + Rust ${REQUIRED_TOOLCHAIN}..."
        command -v curl >/dev/null 2>&1 || die "curl not found — install curl first"
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
            | sh -s -- -y --profile minimal --default-toolchain "${REQUIRED_TOOLCHAIN}" \
            || die "rustup installation failed"
        # shellcheck disable=SC1091
        . "$HOME/.cargo/env"
        command -v cargo >/dev/null 2>&1 || die "cargo still not on PATH after install"
        ok "Rust ${REQUIRED_TOOLCHAIN} installed"
        return
    fi

    # cargo present — is it rustup-managed?
    if command -v rustup >/dev/null 2>&1; then
        rustup toolchain install "${REQUIRED_TOOLCHAIN}" --profile minimal >/dev/null 2>&1 \
            || die "failed to install toolchain ${REQUIRED_TOOLCHAIN} via rustup"
        # Pin the toolchain for THIS session only (does not change the user's
        # global default), so every `cargo` call below uses the pinned version.
        export RUSTUP_TOOLCHAIN="${REQUIRED_TOOLCHAIN}"
        ok "using rustup toolchain ${REQUIRED_TOOLCHAIN} for this session"
    else
        # Non-rustup cargo (distro package, etc.) — just verify the version.
        INSTALLED=$(rustc --version | awk '{print $2}')
        if version_ge "$INSTALLED" "${REQUIRED_TOOLCHAIN}"; then
            ok "host cargo ${INSTALLED} (>= ${REQUIRED_TOOLCHAIN})"
        else
            die "host cargo ${INSTALLED} is older than ${REQUIRED_TOOLCHAIN}; install rustup (https://rustup.rs) or upgrade Rust"
        fi
    fi
}

# ── 1. Prerequisites ───────────────────────────────────────────────────────
info "Checking prerequisites..."

ensure_rust
command -v podman >/dev/null 2>&1 || die "podman not found — install: https://podman.io"
command -v gh     >/dev/null 2>&1 || die "gh CLI not found — install: https://cli.github.com/"

gh auth status >/dev/null 2>&1 || die "gh not authenticated — run: gh auth login"

gh repo set-default "${REPO_OWNER}/${REPO_NAME}" 2>/dev/null || true

# Ensure gh has the write:packages scope needed to push to GHCR.
SCOPE_OK=false
while IFS= read -r line; do
    [[ "$line" == *"write:packages"* ]] && SCOPE_OK=true
done < <(gh auth status 2>&1)
if [[ "$SCOPE_OK" != "true" ]]; then
    info "Adding write:packages scope to gh token..."
    gh auth refresh -s write:packages || die "failed to refresh gh token scope"
fi

# Auto-login to GHCR using the gh token — no manual podman login needed.
info "Authenticating to GHCR..."
GH_USER=$(gh api user --jq .login 2>/dev/null || echo "$REPO_OWNER")
GH_TOKEN=$(gh auth token)
echo "$GH_TOKEN" | podman login ghcr.io -u "$GH_USER" --password-stdin >/dev/null 2>&1 \
    || die "podman login to ghcr.io failed"
ok "prerequisites satisfied"

# ── 2. Repository state ────────────────────────────────────────────────────
info "Validating repository state..."

BRANCH="$(git branch --show-current)"
[[ "$BRANCH" == "main" ]] || die "not on main (on '${BRANCH}') — merge your feature branch first"

[[ -z "$(git status --porcelain)" ]] || die "working tree is dirty — commit or stash first"

git fetch origin --quiet
LOCAL=$(git rev-parse HEAD)
REMOTE=$(git rev-parse origin/main)
if [[ "$LOCAL" != "$REMOTE" ]]; then
    AHEAD=$(git rev-list --count origin/main..HEAD)
    BEHIND=$(git rev-list --count HEAD..origin/main)
    if [[ "$AHEAD" -gt 0 && "$BEHIND" -eq 0 ]]; then
        die "you have $AHEAD unpushed commit(s) on main — run: git push"
    elif [[ "$BEHIND" -gt 0 && "$AHEAD" -eq 0 ]]; then
        die "local main is $BEHIND commit(s) behind origin — run: git pull"
    else
        die "local and origin/main have diverged ($AHEAD ahead, $BEHIND behind) — reconcile manually"
    fi
fi

ok "on main, clean tree, in sync with origin"

# ── 3. Lint and test (before version bump so failure leaves no dirty tree) ─
info "Linting..."
cargo fmt --all -- --check
ok "fmt clean"

cargo clippy --all-features --all-targets -- -D warnings
ok "clippy clean"

if [[ "${SKIP_TESTS:-}" == "1" ]]; then
    warn "skipping tests (SKIP_TESTS=1)"
else
    info "Testing..."
    TEST_OUTPUT=$(cargo test --all-features --bins --tests 2>&1) || {
        echo "$TEST_OUTPUT"
        die "tests failed"
    }
    ok "tests pass"
fi

# ── 4. Ask for the next version ────────────────────────────────────────────
CURRENT_VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
SUGGESTED=$(bump_patch "$CURRENT_VERSION")

echo ""
printf "${B}sendme-balloon release${N}\n"
printf "  Current version: ${G}%s${N}\n" "$CURRENT_VERSION"
printf "  Suggested next:   %s\n" "$SUGGESTED"
echo ""

while true; do
    read -rp "Release version [${SUGGESTED}]: " VERSION
    VERSION="${VERSION:-$SUGGESTED}"
    VERSION="${VERSION#v}"  # strip leading 'v'

    if [[ ! "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
        printf "%b\n" "${R}Must be x.y.z e.g. 0.1.0${N}"
        continue
    fi

    TAG="v${VERSION}"
    if git rev-parse "$TAG" >/dev/null 2>&1; then
        printf "%b\n" "${R}Tag ${TAG} already exists.${N}"
        continue
    fi
    break
done

echo ""
printf "%b\n" "${B}Releasing ${G}${VERSION}${N}${B} tag ${TAG}${N}"
echo ""
printf "Platforms: linux (gnu/musl, amd64+arm64), windows, macos,\n"
printf "           OCI image to GHCR.\n"
echo ""

read -rp "Proceed? [y/N] " confirm
[[ "$confirm" =~ ^[Yy]$ ]] || die "aborted."

# ── 5. Version bump ────────────────────────────────────────────────────────
info "Bumping version to ${VERSION}..."
sed -i "s/^version = .*/version = \"${VERSION}\"/" Cargo.toml
cargo update -p sendme --precise "$VERSION" 2>/dev/null || true
ok "version bumped"

# ── 6. Build all target platforms ──────────────────────────────────────────
info "Building all target platforms (hermetic containers)..."

SDK_NOTE=""
if [[ ! -d "$ROOT_DIR/sdks/MacOSX.sdk" ]]; then
    SDK_NOTE=" (macOS targets will be skipped — no Apple SDK)"
fi
printf "${C}▶${N} Running scripts/build.sh --dist%s\n" "$SDK_NOTE"

if ! ./scripts/build.sh --dist; then
    die "multi-platform build failed — see output above"
fi
ok "all available targets built and packaged"

# ── 7. Container image ────────────────────────────────────────────────────
info "Building container image..."
GIT_SHA=$(git rev-parse --short HEAD)

podman build \
    --tag "${IMAGE}:${VERSION}" \
    --tag "${IMAGE}:latest" \
    --tag "${IMAGE}:sha-${GIT_SHA}" \
    .
ok "image built"

info "Pushing container image..."
podman push "${IMAGE}:${VERSION}"
podman push "${IMAGE}:latest"
podman push "${IMAGE}:sha-${GIT_SHA}"
ok "image pushed"

# ── 8. Commit, tag, push, release ─────────────────────────────────────────
info "Committing, tagging, and creating GitHub release..."

git add Cargo.toml Cargo.lock
git commit -m "release v${VERSION}" --allow-empty 2>/dev/null || true

git tag -a "$TAG" -m "Release $TAG"
git push origin main
git push origin "$TAG"
ok "committed and pushed tag ${TAG}"

info "Creating GitHub release..."
# Upload every archive produced by build.sh plus the checksums.
shopt -s nullglob
ASSETS=( "$DIST"/*.tar.gz "$DIST"/*.zip "$DIST"/SHA256SUMS )
gh release create "$TAG" \
    --title "$TAG" \
    --generate-notes \
    "${ASSETS[@]}"
ok "GitHub release created"

# ── Done ───────────────────────────────────────────────────────────────────
echo ""
printf "${B}${G}═══ Release ${VERSION} complete ═══${N}\n"
printf "  Downloads:  https://github.com/${REPO_OWNER}/${REPO_NAME}/releases/tag/${TAG}\n"
printf "  Container:  %s:%s\n" "$IMAGE" "$VERSION"
printf "  Container:  %s:latest\n" "$IMAGE"
echo ""
