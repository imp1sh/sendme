#!/usr/bin/env bash
#
# sendme — shared shell helpers for the build/release lifecycle scripts.
# Sourced by scripts/build.sh and scripts/release.sh — not meant to be run
# directly.
#
# Provides:
#   info / ok / warn / die   colour-aware logging (set colours before sourcing)
#   version_ge               semver x.y.z comparison
#   detect_pm                echo the system package manager name
#   ensure_tool              auto-install a system tool via the package manager
#   ensure_rust              auto-install/pin the Rust toolchain via rustup
#

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

# Detect the system package manager and echo its name, or empty if none found.
detect_pm() {
    for pm in dnf apt pacman zypper brew; do
        command -v "$pm" >/dev/null 2>&1 && { echo "$pm"; return; }
    done
}

# Install a system tool via the native package manager if it is missing.
#   $1 = executable name (also used as the friendly name in messages)
#   $2 = (optional) override package name; defaults to $1
ensure_tool() {
    local exe="$1"
    local pkg="${2:-$1}"

    command -v "$exe" >/dev/null 2>&1 && return

    info "${exe} not found — attempting to install via package manager..."
    local pm
    pm="$(detect_pm)"
    [[ -n "$pm" ]] || die "${exe} not found and no supported package manager detected — install manually"

    # Root check — we need sudo for system package managers (except brew).
    local sudo=""
    if [[ "$pm" != "brew" ]]; then
        if [[ $EUID -ne 0 ]]; then
            command -v sudo >/dev/null 2>&1 || die "sudo required to install ${exe} — run as root or install sudo"
            sudo="sudo"
        fi
    fi

    case "$pm" in
        dnf)
            $sudo dnf install -y "$pkg"
            ;;
        apt)
            # gh needs the official GitHub CLI repo on Debian/Ubuntu.
            if [[ "$exe" == "gh" ]]; then
                $sudo mkdir -p /etc/apt/keyrings
                $sudo chmod 755 /etc/apt/keyrings
                curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg \
                    | $sudo tee /etc/apt/keyrings/githubcli-archive-keyring.gpg >/dev/null
                $sudo chmod go+r /etc/apt/keyrings/githubcli-archive-keyring.gpg
                echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" \
                    | $sudo tee /etc/apt/sources.list.d/github-cli.list >/dev/null
                $sudo apt-get update
            fi
            $sudo apt-get install -y "$pkg"
            ;;
        pacman)
            $sudo pacman -S --noconfirm "$pkg"
            ;;
        zypper)
            $sudo zypper install -y "$pkg"
            ;;
        brew)
            brew install "$pkg"
            ;;
        *)
            die "unsupported package manager '$pm' for ${exe}"
            ;;
    esac

    command -v "$exe" >/dev/null 2>&1 \
        || die "${exe} installation appeared to succeed but is not on PATH — open a new terminal or check your PATH"
    ok "${exe} installed"
}

# Ensure a suitable Rust toolchain is available on the host.  Installs rustup
# + the pinned toolchain if cargo is absent; otherwise ensures the pinned
# toolchain is installed (via rustup) and selected for this session without
# disturbing the user's global default.  Falls back to a version check for
# non-rustup cargo installations.
#   $1 = required toolchain version (e.g. "1.92.0")
ensure_rust() {
    local required="$1"

    if ! command -v cargo >/dev/null 2>&1; then
        info "cargo not found — installing rustup + Rust ${required}..."
        command -v curl >/dev/null 2>&1 || die "curl not found — install curl first"
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
            | sh -s -- -y --profile minimal --default-toolchain "${required}" \
            || die "rustup installation failed"
        # shellcheck disable=SC1091
        . "$HOME/.cargo/env"
        command -v cargo >/dev/null 2>&1 || die "cargo still not on PATH after install"
        ok "Rust ${required} installed"
        return
    fi

    # cargo present — is it rustup-managed?
    if command -v rustup >/dev/null 2>&1; then
        rustup toolchain install "${required}" --profile minimal >/dev/null 2>&1 \
            || die "failed to install toolchain ${required} via rustup"
        # Pin the toolchain for THIS session only (does not change the user's
        # global default), so every `cargo` call below uses the pinned version.
        export RUSTUP_TOOLCHAIN="${required}"
        ok "using rustup toolchain ${required} for this session"
    else
        # Non-rustup cargo (distro package, etc.) — just verify the version.
        local installed
        installed=$(rustc --version | awk '{print $2}')
        if version_ge "$installed" "${required}"; then
            ok "host cargo ${installed} (>= ${required})"
        else
            die "host cargo ${installed} is older than ${required}; install rustup (https://rustup.rs) or upgrade Rust"
        fi
    fi
}
