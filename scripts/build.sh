#!/usr/bin/env bash
#
# sendme — clean multi-platform build driver
#
# Builds every release target on an amd64 Linux desktop using hermetic
# containers (Dockerfile.cross + cargo-zigbuild).  Nothing is compiled on the
# host directly and no target-platform SDKs need to be installed on the host
# — every toolchain lives inside a container, so the host stays clean and
# every build is reconstructible from the pinned Dockerfile + Cargo.lock.
#
# macOS (darwin) targets additionally need the Apple macOS SDK at
#   ./sdks/MacOSX.sdk/
# (see scripts/fetch-macos-sdk.sh).  If it is absent, darwin targets are
# skipped with a warning rather than failing the whole run.
#
# Usage:
#   ./scripts/build.sh                # build all targets, extract to dist/
#   ./scripts/build.sh --dist         # also package archives + SHA256SUMS
#   ./scripts/build.sh --list         # list the target matrix and exit
#   ./scripts/build.sh --target x86_64-unknown-linux-musl
#   ./scripts/build.sh --target aarch64-apple-darwin --bin sendme
#   ./scripts/build.sh --clean        # wipe dist/ and exit
#   ./scripts/build.sh --no-darwin    # skip macOS targets entirely
#
# Environment:
#   SKIP_CACHE=1   disable BuildKit cache mounts (slower, fully hermetic)
#

set -euo pipefail

# ── Colours ─────────────────────────────────────────────────────────────────
if [[ -t 1 ]]; then
    B='\033[1m'; G='\033[32m'; R='\033[31m'; Y='\033[33m'; C='\033[36m'; D='\033[2m'; N='\033[0m'
else
    B=''; G=''; R=''; Y=''; C=''; D=''; N=''
fi
info()  { printf "${C}▶${N} %s\n" "$*"; }
ok()    { printf "${G}✓${N} %s\n" "$*"; }
warn()  { printf "${Y}⚠${N} %s\n" "$*"; }
die()   { printf "${R}✗${N} %s\n" "$*" >&2; exit 1; }
step()  { printf "\n${B}── %s ──${N}\n" "$*"; }

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$ROOT_DIR"

# Shared lifecycle helpers (logging, version_ge, detect_pm, ensure_tool).
# shellcheck disable=SC1091
. "$SCRIPT_DIR/lib/common.sh"

DIST="$ROOT_DIR/dist"
STAGING="$DIST/staging"
DOCKERFILE="$ROOT_DIR/Dockerfile.cross"
SDK_DIR="$ROOT_DIR/sdks/MacOSX.sdk"

# ── Target matrix ──────────────────────────────────────────────────────────
# Fields: TRIPLE : BIN : FEATURES : NEED_GUI : STAGE
#   STAGE  = base | darwin   (darwin injects the Apple SDK)
MATRIX=(
    # Linux gnu — amd64
    "x86_64-unknown-linux-gnu:sendme::false:base"
    "x86_64-unknown-linux-gnu:sendme-balloon:balloon:true:base"
    # Linux gnu — arm64
    "aarch64-unknown-linux-gnu:sendme::false:base"
    "aarch64-unknown-linux-gnu:sendme-balloon:balloon:true:base"
    # Linux musl — CLI only (static + GUI is impractical)
    "x86_64-unknown-linux-musl:sendme::false:base"
    "aarch64-unknown-linux-musl:sendme::false:base"
    # Windows gnu — amd64 (MinGW via Zig)
    "x86_64-pc-windows-gnu:sendme::false:base"
    "x86_64-pc-windows-gnu:sendme-balloon:balloon:true:base"
    # macOS darwin — amd64 (needs Apple SDK)
    "x86_64-apple-darwin:sendme::false:darwin"
    "x86_64-apple-darwin:sendme-balloon:balloon:true:darwin"
    # macOS darwin — arm64 (needs Apple SDK)
    "aarch64-apple-darwin:sendme::false:darwin"
    "aarch64-apple-darwin:sendme-balloon:balloon:true:darwin"
)

# Friendly archive suffix per triple (matches scripts/install.sh expectations).
suffix_for_triple() {
    case "$1" in
        x86_64-unknown-linux-gnu)   echo "linux-amd64" ;;
        aarch64-unknown-linux-gnu)  echo "linux-arm64" ;;
        x86_64-unknown-linux-musl)  echo "linux-musl-amd64" ;;
        aarch64-unknown-linux-musl) echo "linux-musl-arm64" ;;
        x86_64-pc-windows-gnu)     echo "windows-amd64" ;;
        aarch64-pc-windows-gnu)     echo "windows-arm64" ;;
        x86_64-apple-darwin)        echo "darwin-amd64" ;;
        aarch64-apple-darwin)       echo "darwin-arm64" ;;
        *) echo "UNKNOWN" ;;
    esac
}

version() {
    sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1
}

# ── Arguments ──────────────────────────────────────────────────────────────
DO_DIST=false
DO_LIST=false
DO_CLEAN=false
FILTER_TARGET=""
FILTER_BIN=""
NO_DARWIN=false

while [[ $# -gt 0 ]]; do
    case "$1" in
        --dist)        DO_DIST=true; shift ;;
        --list)        DO_LIST=true; shift ;;
        --clean)       DO_CLEAN=true; shift ;;
        --target)      FILTER_TARGET="$2"; shift 2 ;;
        --bin)         FILTER_BIN="$2"; shift 2 ;;
        --no-darwin)   NO_DARWIN=true; shift ;;
        --help|-h)
            sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'
            exit 0 ;;
        *) die "unknown option: $1 (try --help)" ;;
    esac
done

# ── --clean ────────────────────────────────────────────────────────────────
if [[ "$DO_CLEAN" == "true" ]]; then
    rm -rf "$DIST"
    ok "dist/ removed"
    exit 0
fi

# ── --list ─────────────────────────────────────────────────────────────────
if [[ "$DO_LIST" == "true" ]]; then
    printf "${B}Target matrix (%d entries)${N}\n\n" "${#MATRIX[@]}"
    printf "${D}%-28s %-16s %-10s %-8s %-7s %-18s${N}\n" \
        "TRIPLE" "BIN" "FEATURES" "GUI" "STAGE" "ARCHIVE-SUFFIX"
    for e in "${MATRIX[@]}"; do
        IFS=':' read -r tr bin feat gui stage <<< "$e"
        printf "%-28s %-16s %-10s %-8s %-7s %s\n" \
            "$tr" "$bin" "${feat:-(none)}" "$gui" "$stage" "$(suffix_for_triple "$tr")"
    done
    echo ""
    if [[ -d "$SDK_DIR" ]]; then
        ok "Apple SDK present at $SDK_DIR — darwin targets will build"
    else
        warn "Apple SDK NOT present at sdks/MacOSX.sdk — darwin targets will be skipped"
    fi
    exit 0
fi

# ── Prerequisites ──────────────────────────────────────────────────────────
ensure_tool podman
[[ -f "$DOCKERFILE" ]] || die "Dockerfile.cross not found at $DOCKERFILE"

# ── SDK availability ───────────────────────────────────────────────────────
SDK_PRESENT=false
if [[ -d "$SDK_DIR" ]]; then
    SDK_PRESENT=true
fi

# ── Build loop ─────────────────────────────────────────────────────────────
TOTAL=${#MATRIX[@]}
BUILT=0
SKIPPED=0
FAILED=0
declare -a RESULTS

rm -rf "$STAGING"
mkdir -p "$STAGING"

for e in "${MATRIX[@]}"; do
    IFS=':' read -r tr bin feat gui stage <<< "$e"

    # Filters
    [[ -n "$FILTER_TARGET" && "$tr" != "$FILTER_TARGET" ]] && continue
    [[ -n "$FILTER_BIN" && "$bin" != "$FILTER_BIN" ]] && continue

    pkg="${bin}-${tr}"

    # macOS gate
    if [[ "$stage" == "darwin" ]]; then
        if [[ "$NO_DARWIN" == "true" ]]; then
            warn "skip $pkg (--no-darwin)"; SKIPPED=$((SKIPPED+1)); continue
        fi
        if [[ "$SDK_PRESENT" != "true" ]]; then
            warn "skip $pkg — Apple SDK missing (run scripts/fetch-macos-sdk.sh)"
            SKIPPED=$((SKIPPED+1)); continue
        fi
    fi

    step "Building ${B}$pkg${N}"

    out_dir="$STAGING/$pkg"
    rm -rf "$out_dir"
    mkdir -p "$out_dir"

    args=(
        --build-arg "TARGET=$tr"
        --build-arg "FEATURES=$feat"
        --build-arg "NEED_GUI=$gui"
        --build-arg "BIN=$bin"
        -f "$DOCKERFILE"
        --output "type=local,dest=$out_dir"
    )
    if [[ "$stage" == "darwin" ]]; then
        args+=(--build-arg "CROSS_STAGE=darwin" --build-context "applesdk=$ROOT_DIR/sdks")
    fi

    if podman build "${args[@]}" "$ROOT_DIR" 2>&1 | sed 's/^/    /'; then
        ok "built $pkg"
        RESULTS+=("${G}✓${N} $pkg")
        BUILT=$((BUILT+1))
    else
        printf "${R}✗${N} failed %s\n" "$pkg"
        RESULTS+=("${R}✗${N} $pkg")
        FAILED=$((FAILED+1))
    fi
done

# ── Summary ────────────────────────────────────────────────────────────────
echo ""
printf "${B}═══ Build summary ═══${N}\n"
for r in "${RESULTS[@]}"; do printf "  %b\n" "$r"; done
printf "  ${G}built %d${N} · ${Y}skipped %d${N} · ${R}failed %d${N} / %d\n" \
    "$BUILT" "$SKIPPED" "$FAILED" "$TOTAL"

if [[ "$FAILED" -gt 0 ]]; then
    die "$FAILED target(s) failed"
fi

# ── --dist: package archives + checksums ────────────────────────────────────
if [[ "$DO_DIST" == "true" ]]; then
    step "Packaging archives"
    V="$(version)"

    declare -A SUF=(
        [x86_64-unknown-linux-gnu]="linux-amd64"
        [aarch64-unknown-linux-gnu]="linux-arm64"
        [x86_64-unknown-linux-musl]="linux-musl-amd64"
        [aarch64-unknown-linux-musl]="linux-musl-arm64"
        [x86_64-pc-windows-gnu]="windows-amd64"
        [aarch64-pc-windows-gnu]="windows-arm64"
        [x86_64-apple-darwin]="darwin-amd64"
        [aarch64-apple-darwin]="darwin-arm64"
    )

    rm -f "$DIST"/*.tar.gz "$DIST"/*.zip "$DIST"/SHA256SUMS
    count=0

    shopt -s nullglob
    for d in "$STAGING"/*/; do
        pkg="$(basename "$d")"
        if [[ "$pkg" == sendme-balloon-* ]]; then
            base=sendme-balloon; triple="${pkg#sendme-balloon-}"
        else
            base=sendme; triple="${pkg#sendme-}"
        fi
        suf="${SUF[$triple]:-UNKNOWN}"
        out="$DIST/${base}-v${V}-${suf}"

        bin_file="$(find "$d" -maxdepth 1 -type f | head -1)"
        [[ -n "$bin_file" ]] || { warn "no binary in $pkg, skipping archive"; continue; }
        fname="$(basename "$bin_file")"

        if [[ "$triple" == *pc-windows* ]]; then
            (cd "$d" && zip -X "${out}.zip" "$fname")
        else
            tar czf "${out}.tar.gz" -C "$d" "$fname"
        fi
        ok "${base}-v${V}-${suf}"
        count=$((count+1))
    done

    step "Checksums"
    (cd "$DIST" && sha256sum *.tar.gz *.zip 2>/dev/null | sort -k2 > SHA256SUMS)
    ok "SHA256SUMS written ($count archives)"

    echo ""
    printf "${B}${G}═══ Artifacts ready in dist/ ═══${N}\n"
    ls -1 "$DIST"/*.tar.gz "$DIST"/*.zip "$DIST"/SHA256SUMS 2>/dev/null | sed 's/^/  /'
fi
