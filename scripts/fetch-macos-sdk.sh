#!/usr/bin/env bash
#
# sendme — obtain the Apple macOS SDK for darwin cross-builds
#
# darwin targets (x86_64/aarch64-apple-darwin) need the macOS SDK to link
# against system libraries.  The SDK cannot be redistributed, so this helper
# extracts it from an Xcode .xip you download yourself from Apple, using the
# osxcross tooling.  The result lands at ./sdks/MacOSX.sdk, which
# Dockerfile.cross consumes via the `applesdk` build context.
#
# Steps performed:
#   1. Clone osxcross (into ./build/osxcross) if absent.
#   2. Run its gen_sdk_package.sh on your Xcode .xip → MacOSX*.sdk.tar.*
#   3. Extract the SDK into ./sdks/MacOSX.sdk.
#
# Prereqs: git, xar (or pbzx), and an Xcode .xip downloaded from
#   https://developer.apple.com/download/all/  (sign in with an Apple ID).
#
# Usage:
#   ./scripts/fetch-macos-sdk.sh --xip ~/Downloads/Xcode_15.4.xip
#   ./scripts/fetch-macos-sdk.sh --status
#

set -euo pipefail

if [[ -t 1 ]]; then
    B='\033[1m'; G='\033[32m'; R='\033[31m'; Y='\033[33m'; C='\033[36m'; N='\033[0m'
else
    B=''; G=''; R=''; Y=''; C=''; N=''
fi
info() { printf "${C}▶${N} %s\n" "$*"; }
ok()   { printf "${G}✓${N} %s\n" "$*"; }
warn() { printf "${Y}⚠${N} %s\n" "$*"; }
die()  { printf "${R}✗${N} %s\n" "$*" >&2; exit 1; }

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$ROOT_DIR"

OSXCROSS_DIR="$ROOT_DIR/build/osxcross"
SDK_DEST="$ROOT_DIR/sdks/MacOSX.sdk"
XIP=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --xip)    XIP="$2"; shift 2 ;;
        --status)
            if [[ -d "$SDK_DEST" ]]; then
                ok "SDK present at $SDK_DEST"
            else
                warn "SDK not present — run: $0 --xip <Xcode.xip>"
            fi
            exit 0 ;;
        --help|-h)
            sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'
            exit 0 ;;
        *) die "unknown option: $1" ;;
    esac
done

[[ -n "$XIP" ]] || die "usage: $0 --xip <path-to-Xcode.xip>"
[[ -f "$XIP" ]] || die "Xcode .xip not found: $XIP"

command -v git >/dev/null 2>&1 || die "git not found"

# ── 1. Clone osxcross ──────────────────────────────────────────────────────
if [[ ! -d "$OSXCROSS_DIR" ]]; then
    info "Cloning osxcross..."
    mkdir -p "$ROOT_DIR/build"
    git clone --depth 1 https://github.com/tpoechtrager/osxcross "$OSXCROSS_DIR"
    ok "osxcross cloned"
fi

# ── 2. Generate SDK package ────────────────────────────────────────────────
info "Generating SDK package from $(basename "$XIP") (this takes a few minutes)..."
GEN="$OSXCROSS_DIR/tools/gen_sdk_package.sh"
[[ -x "$GEN" ]] || die "gen_sdk_package.sh not found in osxcross"
"$GEN" "$XIP"

# ── 3. Locate and extract the produced SDK tarball ────────────────────────
SDK_TAR=$(ls -t "$OSXCROSS_DIR"/MacOSX*.sdk.tar.* 2>/dev/null | head -1)
[[ -n "$SDK_TAR" ]] || die "no MacOSX*.sdk.tar.* produced — check osxcross output"

info "Extracting $(basename "$SDK_TAR") → sdks/MacOSX.sdk"
mkdir -p "$ROOT_DIR/sdks"
rm -rf "$SDK_DEST"
case "$SDK_TAR" in
    *.tar.xz)  tar xJf "$SDK_TAR" -C "$ROOT_DIR/sdks" ;;
    *.tar.gz)  tar xzf "$SDK_TAR" -C "$ROOT_DIR/sdks" ;;
    *.tar)     tar xf "$SDK_TAR" -C "$ROOT_DIR/sdks" ;;
    *) die "unrecognised tarball format: $SDK_TAR" ;;
esac

EXTRACTED=$(find "$ROOT_DIR/sdks" -maxdepth 1 -type d -name 'MacOSX*.sdk' | head -1)
[[ -n "$EXTRACTED" ]] || die "SDK directory not found after extraction"

# Normalise the name to MacOSX.sdk so Dockerfile.cross can rely on it.
mv "$EXTRACTED" "$SDK_DEST"

ok "SDK ready at $SDK_DEST"
echo ""
printf "You can now build darwin targets:\n"
printf "  ${C}./scripts/build.sh --target aarch64-apple-darwin${N}\n"
