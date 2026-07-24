#!/usr/bin/env bash
#
# sendme-balloon — interactive release script
#
# Usage:
#   make release
#   ./scripts/release.sh
#
# The script checks everything is in order, shows the current version,
# suggests the next patch version, and asks you to confirm.  Then it does
# everything:
#   - bumps version, lints, tests
#   - builds optimised binaries (CLI + balloon GUI)
#   - packages tarballs
#   - builds and pushes container image to GHCR
#   - commits, tags, pushes, and creates a GitHub Release with downloads
#
# Prerequisites (one-time):
#   gh auth login
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
TARGET="x86_64-unknown-linux-gnu"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$ROOT_DIR"

# ── Helpers ─────────────────────────────────────────────────────────────────
bump_patch() {
    local v="$1"
    IFS='.' read -r major minor patch <<< "$v"
    echo "${major}.${minor}.$((patch + 1))"
}

# ── 1. Prerequisites ───────────────────────────────────────────────────────
info "Checking prerequisites..."

command -v cargo  >/dev/null 2>&1 || die "cargo not found — install Rust: https://rustup.rs"
command -v podman >/dev/null 2>&1 || die "podman not found — install: https://podman.io"
command -v gh     >/dev/null 2>&1 || die "gh CLI not found — install: https://cli.github.com/"

gh auth status >/dev/null 2>&1 || die "gh not authenticated — run: gh auth login"

# Ensure gh knows which repo to use (avoids 'no default remote repository').
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
    # Capture all test output so subprocess eprintln!/progress bars can't
    # scramble the terminal.  Only shown on failure.
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

read -rp "Proceed? [y/N] " confirm
[[ "$confirm" =~ ^[Yy]$ ]] || die "aborted."

# ── 5. Version bump ────────────────────────────────────────────────────────
info "Bumping version to ${VERSION}..."
sed -i "s/^version = .*/version = \"${VERSION}\"/" Cargo.toml
cargo update -p sendme --precise "$VERSION" 2>/dev/null || true
ok "version bumped"

# ── 6. Build binaries ──────────────────────────────────────────────────────
info "Building release binaries..."
cargo build --release --target "$TARGET" --bin sendme
ok "sendme built"

cargo build --release --features balloon --target "$TARGET" --bin sendme-balloon
ok "sendme-balloon built"

# ── 7. Package tarballs ───────────────────────────────────────────────────
info "Packaging tarballs..."
DIST="$ROOT_DIR/dist"
rm -rf "$DIST"
mkdir -p "$DIST"

tar czf "$DIST/sendme-v${VERSION}-linux-amd64.tar.gz" \
    -C "target/$TARGET/release" sendme
tar czf "$DIST/sendme-balloon-v${VERSION}-linux-amd64.tar.gz" \
    -C "target/$TARGET/release" sendme-balloon

# Generate checksums for the installer to verify
(cd "$DIST" && sha256sum sendme-v${VERSION}-linux-amd64.tar.gz sendme-balloon-v${VERSION}-linux-amd64.tar.gz > SHA256SUMS)
ok "tarballs and checksums packaged"

# ── 8. Container image ────────────────────────────────────────────────────
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

# ── 9. Commit, tag, push, release ─────────────────────────────────────────
info "Committing, tagging, and creating GitHub release..."

git add Cargo.toml Cargo.lock
git commit -m "release v${VERSION}" --allow-empty 2>/dev/null || true

git tag -a "$TAG" -m "Release $TAG"
git push origin main
git push origin "$TAG"
ok "committed and pushed tag ${TAG}"

info "Creating GitHub release..."
gh release create "$TAG" \
    --title "$TAG" \
    --generate-notes \
    "$DIST/sendme-v${VERSION}-linux-amd64.tar.gz" \
    "$DIST/sendme-balloon-v${VERSION}-linux-amd64.tar.gz" \
    "$DIST/SHA256SUMS"
ok "GitHub release created"

# ── Done ───────────────────────────────────────────────────────────────────
echo ""
printf "${B}${G}═══ Release ${VERSION} complete ═══${N}\n"
printf "  Downloads:  https://github.com/${REPO_OWNER}/${REPO_NAME}/releases/tag/${TAG}\n"
printf "  Container:  %s:%s\n" "$IMAGE" "$VERSION"
printf "  Container:  %s:latest\n" "$IMAGE"
echo ""
