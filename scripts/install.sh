#!/usr/bin/env bash
#
# sendme-balloon installer
#
# Installs the sendme CLI and sendme-balloon desktop app from GitHub Releases.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/imp1sh/sendme-balloon/main/scripts/install.sh | bash
#   curl -fsSL ... | bash -s -- --user          # install to ~/.local/bin (no root)
#   curl -fsSL ... | sudo bash                  # system-wide to /usr/local/bin
#   curl -fsSL ... | bash -s -- --uninstall     # remove
#   curl -fsSL ... | bash -s -- --version 0.1.1 # specific version
#
# Supported: Fedora Linux (x86_64)
#

set -euo pipefail

REPO_OWNER="imp1sh"
REPO_NAME="sendme-balloon"
GITHUB="https://github.com/${REPO_OWNER}/${REPO_NAME}"

# ── Colours ─────────────────────────────────────────────────────────────────
if [[ -t 1 ]]; then
    B='\033[1m'; G='\033[32m'; R='\033[31m'; Y='\033[33m'; C='\033[36m'; N='\033[0m'
else
    B=''; G=''; R=''; Y=''; C=''; N=''
fi
# %b interprets backslash escapes in the argument (so ${G} etc. render)
info()  { printf "${C}▶${N} %b\n" "$*"; }
ok()    { printf "${G}✓${N} %b\n" "$*"; }
die()   { printf "${R}✗${N} %b\n" "$*" >&2; exit 1; }

# ── Arguments ──────────────────────────────────────────────────────────────
ACTION="install"
SCOPE=""
INSTALL_VERSION=""
FORCE=false

while [[ $# -gt 0 ]]; do
    case "$1" in
        --uninstall) ACTION="uninstall"; shift ;;
        --user)      SCOPE="user"; shift ;;
        --system)    SCOPE="system"; shift ;;
        --version)   INSTALL_VERSION="$2"; shift 2 ;;
        --force)     FORCE=true; shift ;;
        --help|-h)
            cat <<USAGE
sendme-balloon installer

Usage:
  bash install.sh [OPTIONS]

Options:
  --uninstall       Remove sendme and sendme-balloon
  --user            Install to ~/.local/bin (no root required)
  --system          Install to /usr/local/bin (requires root, default)
  --version <ver>   Install a specific version (default: latest)
  --force           Skip confirmation prompts
  --help            Show this help
USAGE
            exit 0 ;;
        *) die "unknown option: $1 (try --help)" ;;
    esac
done

# ── OS / arch detection ────────────────────────────────────────────────────
ARCH=$(uname -m)
case "$ARCH" in
    x86_64|amd64) ARCH_TAG="linux-amd64" ;;
    aarch64|arm64) ARCH_TAG="linux-arm64" ;;
    *) die "unsupported architecture: $ARCH" ;;
esac

# Source /etc/os-release in a subshell to avoid clobbering our variables.
OS_ID=""
if [[ -f /etc/os-release ]]; then
    OS_ID=$(bash -c '. /etc/os-release; echo "${ID:-}"')
fi

# ── Determine install scope and paths ──────────────────────────────────────
if [[ -z "$SCOPE" ]]; then
    if [[ $EUID -eq 0 ]]; then
        SCOPE="system"
    else
        SCOPE="user"
        printf "${Y}⚠${N} Not running as root — installing to ~/.local/bin\n"
        printf "  For system-wide install: curl ... | sudo bash\n\n"
    fi
fi

if [[ "$SCOPE" == "system" ]]; then
    [[ $EUID -eq 0 ]] || die "system install requires root — re-run with sudo or use --user"
    PREFIX="/usr/local"
else
    PREFIX="${HOME}/.local"
fi

BIN_DIR="${PREFIX}/bin"
APP_DIR="${PREFIX}/share/applications"
ICON_DIR="${PREFIX}/share/icons/hicolor/scalable/apps"

# ── Uninstall ──────────────────────────────────────────────────────────────
do_uninstall() {
    info "Removing sendme-balloon..."
    local removed=false

    for f in "${BIN_DIR}/sendme" "${BIN_DIR}/sendme-balloon"; do
        if [[ -f "$f" ]]; then
            rm -f "$f"
            ok "removed $f"
            removed=true
        fi
    done

    for f in "${APP_DIR}/sendme-balloon.desktop" "${ICON_DIR}/sendme-balloon.svg"; do
        if [[ -f "$f" ]]; then
            rm -f "$f"
            ok "removed $f"
            removed=true
        fi
    done

    # Refresh desktop database
    update-desktop-database -q "${APP_DIR}" 2>/dev/null || true
    if [[ "$SCOPE" == "system" ]]; then
        command -v restorecon >/dev/null 2>&1 && restorecon -R "${BIN_DIR}" 2>/dev/null || true
    fi

    if [[ "$removed" == true ]]; then
        ok "uninstall complete"
    else
        printf "${Y}sendme-balloon was not installed in ${PREFIX}${N}\n"
    fi
}

if [[ "$ACTION" == "uninstall" ]]; then
    do_uninstall
    exit 0
fi

# ── Install ────────────────────────────────────────────────────────────────

# Resolve version
if [[ -z "$INSTALL_VERSION" ]]; then
    info "Finding latest release..."
    TAG=$(curl -fsSI "${GITHUB}/releases/latest" | grep -i "^location:" | sed 's|.*/tag/||' | tr -d '\r\n')
    [[ -n "$TAG" ]] || die "could not determine latest release"
    INSTALL_VERSION="${TAG#v}"
else
    TAG="v${INSTALL_VERSION}"
fi

info "Installing sendme-balloon ${G}${INSTALL_VERSION}${N} (${ARCH_TAG})"

# Create directories
mkdir -p "$BIN_DIR" "$APP_DIR" "$ICON_DIR"

# Temp working directory
TMP=$(mktemp -d)
cleanup() { rm -rf "$TMP"; }
trap cleanup EXIT

# Download tarballs and checksums
BASE_URL="${GITHUB}/releases/download/${TAG}"
CLI_TARBALL="sendme-v${INSTALL_VERSION}-${ARCH_TAG}.tar.gz"
GUI_TARBALL="sendme-balloon-v${INSTALL_VERSION}-${ARCH_TAG}.tar.gz"
CHECKSUM_FILE="SHA256SUMS"

info "Downloading..."
curl -fsSL "${BASE_URL}/${CLI_TARBALL}" -o "${TMP}/${CLI_TARBALL}" \
    || die "failed to download ${CLI_TARBALL}"
curl -fsSL "${BASE_URL}/${GUI_TARBALL}" -o "${TMP}/${GUI_TARBALL}" \
    || die "failed to download ${GUI_TARBALL}"

# Verify checksums
if curl -fsSL "${BASE_URL}/${CHECKSUM_FILE}" -o "${TMP}/${CHECKSUM_FILE}" 2>/dev/null; then
    info "Verifying checksums..."
    (cd "$TMP" && sha256sum -c --ignore-missing "${CHECKSUM_FILE}") \
        || die "checksum verification failed — download may be corrupted"
    ok "checksums verified"
else
    printf "${Y}⚠${N} No checksum file available — skipping verification\n"
fi

# Extract
info "Extracting..."
tar xzf "${TMP}/${CLI_TARBALL}" -C "$TMP"
tar xzf "${TMP}/${GUI_TARBALL}" -C "$TMP"

# Install binaries
info "Installing to ${G}${BIN_DIR}/${N}..."

install -m 0755 "${TMP}/sendme" "${BIN_DIR}/sendme" \
    || die "failed to install sendme"
ok "sendme → ${BIN_DIR}/sendme"

install -m 0755 "${TMP}/sendme-balloon" "${BIN_DIR}/sendme-balloon" \
    || die "failed to install sendme-balloon"
ok "sendme-balloon → ${BIN_DIR}/sendme-balloon"

# Install icon
info "Installing desktop integration..."

cat > "${ICON_DIR}/sendme-balloon.svg" <<'ICON'
<?xml version="1.0" encoding="UTF-8"?>
<svg xmlns="http://www.w3.org/2000/svg" width="128" height="128" viewBox="0 0 128 128">
  <ellipse cx="64" cy="52" rx="44" ry="50" fill="#4285f4"/>
  <ellipse cx="64" cy="52" rx="44" ry="50" fill="none" stroke="#1a73e8" stroke-width="2"/>
  <ellipse cx="48" cy="36" rx="8" ry="12" fill="white" opacity="0.25"/>
  <polygon points="56,100 72,100 64,112" fill="#34a853"/>
</svg>
ICON
ok "icon installed"

# Desktop entry
cat > "${APP_DIR}/sendme-balloon.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=sendme balloon
Comment=Send and receive files over the internet
Exec=${BIN_DIR}/sendme-balloon
Icon=sendme-balloon
Terminal=false
Categories=Network;FileTransfer;
StartupWMClass=sendme-balloon
DESKTOP
ok "desktop entry created"

# Fedora-specific: restore SELinux contexts
if [[ "$OS_ID" == "fedora" ]] && [[ "$SCOPE" == "system" ]]; then
    command -v restorecon >/dev/null 2>&1 && {
        restorecon -R "${BIN_DIR}/sendme" "${BIN_DIR}/sendme-balloon" 2>/dev/null || true
        restorecon "${APP_DIR}/sendme-balloon.desktop" 2>/dev/null || true
        restorecon "${ICON_DIR}/sendme-balloon.svg" 2>/dev/null || true
        ok "SELinux contexts restored"
    }
fi

# Refresh desktop database
update-desktop-database -q "${APP_DIR}" 2>/dev/null || true
ok "desktop database refreshed"

# Verify
info "Verifying..."
[[ -x "${BIN_DIR}/sendme" ]] || die "sendme not found after install"
[[ -x "${BIN_DIR}/sendme-balloon" ]] || die "sendme-balloon not found after install"
ok "verification passed"

# ── Summary ────────────────────────────────────────────────────────────────
echo ""
printf "${B}${G}═══ Installation complete ═══${N}\n"
printf "  sendme         → ${BIN_DIR}/sendme\n"
printf "  sendme-balloon → ${BIN_DIR}/sendme-balloon\n"
echo ""

# Check if bin dir is in PATH
if [[ ":${PATH}:" != *":${BIN_DIR}:"* ]]; then
    printf "${Y}⚠${N} ${BIN_DIR} is not in your PATH.\n"
    if [[ "$SCOPE" == "user" ]]; then
        printf "  Add this to your shell profile:\n"
        printf "    export PATH=\"\$HOME/.local/bin:\$PATH\"\n"
    fi
    echo ""
fi

printf "Quick start:\n"
printf "  ${C}sendme send myfile.txt${N}          # send a file\n"
printf "  ${C}sendme-balloon${N}                   # launch the desktop app\n"
echo ""
