# sendme-balloon

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg?style=flat-square)](LICENSE-MIT)
[![License: Apache 2.0](https://img.shields.io/badge/License-Apache%202.0-blue.svg?style=flat-square)](LICENSE-APACHE)
[![CI](https://img.shields.io/github/actions/workflow/status/imp1sh/sendme-balloon/ci.yml?branch=main&style=flat-square&label=CI)](https://github.com/imp1sh/sendme-balloon/actions/workflows/ci.yml)
[![container](https://img.shields.io/badge/container-ghcr.io-blue.svg?style=flat-square)](https://github.com/imp1sh/sendme-balloon/pkgs/container/sendme-balloon)


**sendme-balloon** is a fork of [`n0-computer/sendme`](https://github.com/n0-computer/sendme). **sendme-balloon** is 
maintained by [Jochen Demmer](mailto:jochen@winteltosh.de). 

BE WARNED
All adjustments are fully vibe coded as an experiment. So far it seems to be working just fine.
BE WARNED

The original project
is an example application using [iroh](https://crates.io/crates/iroh) with the
[iroh-blobs](https://crates.io/crates/iroh-blobs) protocol to send files and
directories over the internet; this fork focuses on the **balloon**, a tiny
desktop companion for quick copy jobs, while keeping the original command-line
tool available.

Iroh takes care of hole punching and NAT traversal whenever possible, and falls
back to a relay if hole punching does not succeed.

Iroh-blobs takes care of [blake3](https://crates.io/crates/blake3) verified
streaming, including resuming interrupted downloads.

sendme-balloon works with 256 bit node ids and is, therefore, location
transparent. A ticket remains valid if the IP address changes. Connections are
encrypted using TLS.

# Installation

## One-line install (Fedora Linux)

```bash
curl -fsSL https://raw.githubusercontent.com/imp1sh/sendme-balloon/main/scripts/install.sh | bash
```

This downloads the latest release, verifies SHA256 checksums, and installs both
binaries to `~/.local/bin` (or `/usr/local/bin` if run with `sudo`). A desktop
entry and icon are created so `sendme-balloon` appears in your application
launcher.

For system-wide installation:

```bash
curl -fsSL https://raw.githubusercontent.com/imp1sh/sendme-balloon/main/scripts/install.sh | sudo bash
```

To uninstall:

```bash
curl -fsSL https://raw.githubusercontent.com/imp1sh/sendme-balloon/main/scripts/install.sh | bash -s -- --uninstall
```

To install a specific version:

```bash
curl -fsSL https://raw.githubusercontent.com/imp1sh/sendme-balloon/main/scripts/install.sh | bash -s -- --version 0.1.1
```

To install and enable start-at-login:

```bash
curl -fsSL https://raw.githubusercontent.com/imp1sh/sendme-balloon/main/scripts/install.sh | bash -s -- --autostart
```

Autostart can also be toggled at any time from inside the app — open the
address book (round button on the balloon) and use the "Start at login"
checkbox. This uses the XDG autostart standard (`~/.config/autostart/`) and
works on GNOME, KDE Plasma, Cinnamon, MATE, XFCE, LXQt, and Sway. On bare
tiling window managers like i3 or Hyprland, add `exec sendme-balloon` to your
WM config manually.

## Build from source

### Prerequisites

| Platform | Requirements |
|----------|--------------|
| **macOS** | Xcode Command Line Tools (`xcode-select --install`), Rust 1.92+ ([rustup.rs](https://rustup.rs)) |
| **Linux** | Rust 1.92+, plus for the GUI: `libgtk-3-dev libxcb-shape0-dev libxcb-xfixes0-dev libxkbcommon-dev libssl-dev` (apt) or `gtk3-devel libxcb-devel libxkbcommon-devel openssl-devel` (dnf) |
| **Windows** | Visual Studio Build Tools (MSVC), Rust 1.92+ |

### macOS

On macOS the build is straightforward — no extra system libraries are needed
because the GUI stack (Metal, AppKit) ships with the system SDK via Xcode
Command Line Tools.

```bash
# One-time: install Xcode Command Line Tools
xcode-select --install

# One-time: install Rust (if not already installed)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source ~/.cargo/env

# Clone
git clone https://github.com/imp1sh/sendme-balloon.git
cd sendme-balloon

# Build the CLI only
cargo build --release
# → target/release/sendme

# Build the CLI + desktop app (balloon GUI)
cargo build --release --features balloon
# → target/release/sendme
# → target/release/sendme-balloon

# Or install directly to ~/.cargo/bin
cargo install sendme --features balloon
```

The resulting binaries are native Mach-O executables for your Mac's
architecture (Apple Silicon or Intel). To cross-compile for the other
macOS architecture from the same machine:

```bash
# On an Apple Silicon Mac, also build for Intel:
rustup target add x86_64-apple-darwin
cargo build --release --target x86_64-apple-darwin --features balloon

# On an Intel Mac, also build for Apple Silicon:
rustup target add aarch64-apple-darwin
cargo build --release --target aarch64-apple-darwin --features balloon
```

### Linux

```bash
# Install GUI build dependencies (needed for the balloon feature)
# Fedora:
sudo dnf install gtk3-devel libxcb-devel libxkbcommon-devel openssl-devel
# Debian/Ubuntu:
sudo apt-get install libgtk-3-dev libxcb-shape0-dev libxcb-xfixes0-dev libxkbcommon-dev libssl-dev

# Build the CLI only
cargo build --release

# Build the CLI + desktop app
cargo build --release --features balloon
```

### Cross-compiling for macOS from Linux

If you are on Linux and want to produce macOS binaries (e.g. as the maintainer
doing a release), see [Cross-platform builds](#cross-platform-builds) below.
That path uses hermetic containers and requires the Apple SDK to be fetched
once via `make fetch-sdk`. If you are on a Mac, ignore this — build natively
as described above.

# Usage

## sendme-balloon (desktop app)

Launch it with:

```
sendme-balloon
```

A tiny frameless, transparent, always-on-top balloon hovers over the desktop
and lets you send or receive a file with a single gesture. It runs the same
[iroh](https://crates.io/crates/iroh) /
[iroh-blobs](https://crates.io/crates/iroh-blobs) stack under the hood, so
transfers behave just like the command-line tool.

### Sending a file

- **Click** the blue upper half of the balloon to open a file chooser, or
  **drag and drop** a file straight onto the balloon.
- The file is imported and a ticket is shown. Press **Copy ticket** to copy it
  to the clipboard so you can hand it to the receiver.
- Once a peer connects, a progress bar reports how many bytes have been
  transferred. Press **Cancel** (or the ✕ in the title bar) to abort and
  return to the idle balloon.

### Receiving a file

- **Click** the green lower half of the balloon and paste the ticket (the whole
  `sendme receive ...` command is accepted too).
- Choose where to save in the folder dialog that appears.
- A progress bar tracks the download. On success the saved location is shown.

### Address book & direct ticket sharing

The balloon keeps a persistent iroh endpoint with a **stable node id** (a 256-bit
public key), so other people can reach you by that id alone. The round button in
the middle of the balloon opens the **address book**, where you manage contacts
by nickname + node id.

- **Your node id** is shown at the top of the address book with a copy button —
  share it with whoever wants to add you.
- **Add a contact** with a name and their node id. The address book is persisted
  as JSON in your user config directory (`~/.config/sendme-balloon/` on Linux).
- **Send to a contact**: once a file is prepared and its ticket is shown, press
  *📤 Send to a contact…* and pick a contact. The balloon pushes the transfer
  ticket to that contact over iroh (a dedicated `sendme-balloon/offer/1` ALPN),
  so neither side has to copy/paste the ticket by hand.
- **Receive from a contact**: when somebody sends you a ticket this way, your
  balloon prompts *"X wants to send you …"* with **Accept / Decline** buttons.
  Accepting opens a folder picker and then fetches the data automatically.

Direct contact connectivity relies on iroh's address discovery: the contact
endpoint publishes its address via n0's pkarr/DNS service, so a bare node id is
enough to connect (relays/NAT traversal are handled by iroh as usual). Both
peers must be online at the same time, since iroh is a live point-to-point link
rather than a store-and-forward mailbox.

### Notes

- The balloon hides itself while a native file or folder dialog is open and
  reappears once you have made your choice.
- The ✕ in the title bar cancels the current operation and falls back to the
  idle balloon rather than quitting the app; the small ✕ on the idle balloon
  quits it.
- The balloon can be moved around by dragging it.
- On Linux the balloon runs under both X11 and Wayland. Because winit 0.30 has
  no Wayland data-device support, drag-and-drop is routed through XWayland
  (X11) when both `WAYLAND_DISPLAY` and `DISPLAY` are present, which covers
  compositors such as Sway/wlroots.

### Data storage

The balloon persists its state in the user's configuration directory following
the XDG Base Directory Specification (`$XDG_CONFIG_HOME` or `~/.config` on
Linux):

| Path | Purpose |
|------|---------|
| `~/.config/sendme-balloon/secret.key` | Your persistent iroh secret key (32 bytes, hex-encoded). Determines your stable 256-bit node id that others add to their address book. Generated automatically on first run. |
| `~/.config/sendme-balloon/addressbook.json` | Your address book — a JSON array of contacts (nickname + node id). Created on first save. |

The directory is created automatically on first use. To reset the app to a
fresh state, delete the directory:

```bash
rm -rf ~/.config/sendme-balloon
```

Note that deleting `secret.key` changes your node id, so existing contacts
would need to re-add you.

## Command-line usage

### Send side

```
sendme send <file or directory>
```

This creates a temporary [iroh](https://crates.io/crates/iroh) node that serves
the content in the given file or directory. It outputs a ticket that can be
used to get the data.

The provider runs until it is terminated using `Control-C`. On termination, it
deletes the temporary directory.

This currently creates a temporary directory in the current directory. In the
future this won't be needed anymore.

### Receive side

```
sendme receive <ticket>
```

This downloads the data and creates a file or directory named like the source
in the **current directory**.

It creates a temporary directory in the current directory, downloads the data
(single file or directory), and only then moves these files to the target
directory.

On completion, it deletes the temp directory.

All temp directories start with `.sendme-`.

# Development & release workflow

## Prerequisites

You need the following tools installed and configured before you can release:

| Tool | Purpose | Setup |
|------|---------|-------|
| **Rust** | compile binaries | [rustup.rs](https://rustup.rs) |
| **Podman** | build & push container image | [podman.io](https://podman.io) |
| **gh CLI** | create GitHub releases with downloads | [cli.github.com](https://cli.github.com/) |

One-time authentication (credentials are cached):

```bash
gh auth login                                  # for GitHub releases
podman login ghcr.io -u <github-username>      # for container registry
```

## Step 1 — develop on a feature branch

```bash
git checkout main
git pull origin main
git checkout -b feat/my-new-feature

# develop, test, iterate ...
make build              # debug CLI
make build-balloon      # debug balloon GUI (needs GUI libs on Linux)
make test               # all tests
make lint               # fmt + clippy
```

## Step 2 — merge to main

```bash
git checkout main
git pull origin main
git merge --no-ff feat/my-new-feature
git push origin main
```

## Step 3 — release

From a clean `main` that is in sync with `origin`, just run:

```bash
make release
```

The script is interactive — it shows you the current version, suggests the next
patch version, and asks you to confirm:

```
sendme-balloon release
  Current version: 0.1.0
  Suggested next:   0.1.1

Release version [0.1.1]: _
```

Type a version (or press Enter to accept the suggestion), confirm, and the
script does everything else:

1. Bumps version in `Cargo.toml` and `Cargo.lock`
2. Lints (`cargo fmt --check`, `cargo clippy -D warnings`) and tests — stops if
   anything fails
3. Builds **every target platform** via hermetic containers (see
   [Cross-platform builds](#cross-platform-builds)): linux (gnu/musl, amd64 +
   arm64), windows, macOS, FreeBSD
4. Packages archives + a unified `SHA256SUMS` into `dist/`
5. Builds and pushes the container image to GHCR (`:version`, `:latest`,
   `:sha-<hash>`)
6. Commits, tags, and pushes to `origin`
7. Creates a GitHub Release with auto-generated changelog and all archives as
   downloads

When it finishes, the release is live:

- **Downloads**: https://github.com/imp1sh/sendme-balloon/releases
- **Container**: `podman pull ghcr.io/imp1sh/sendme-balloon:<version>`

## Cross-platform builds

> **Building on your own platform?** See [Build from source](#build-from-source)
> above — native builds are simpler and need no SDK extraction. This section
> is for **building every target from an amd64 Linux host** (the maintainer's
> release workflow).

All release targets are cross-compiled from an amd64 Linux host using hermetic
containers — no target-platform toolchains need to be installed on the host.
Each target builds inside a pinned container (`Dockerfile.cross`, Rust 1.92.0 +
Zig 0.13.0 via `cargo-zigbuild`), so every build is reconstructible from the
committed `Cargo.lock` + `Dockerfile.cross`, and BuildKit cache mounts keep
incremental builds fast.

### Target matrix

| Target | Binary | Archive suffix |
|--------|--------|-----------------|
| `x86_64-unknown-linux-gnu` | CLI + GUI | `linux-amd64` |
| `aarch64-unknown-linux-gnu` | CLI + GUI | `linux-arm64` |
| `x86_64-unknown-linux-musl` | CLI only | `linux-musl-amd64` |
| `aarch64-unknown-linux-musl` | CLI only | `linux-musl-arm64` |
| `x86_64-pc-windows-gnu` | CLI + GUI | `windows-amd64` |
| `x86_64-apple-darwin` | CLI + GUI | `darwin-amd64` |
| `aarch64-apple-darwin` | CLI + GUI | `darwin-arm64` |

List the matrix at any time:

```bash
make build-list
```

### Build commands

```bash
make build-all                              # build every target → dist/staging/
make dist                                   # build all + package archives + SHA256SUMS
make build-target T=x86_64-unknown-linux-musl   # build one target
```

### macOS (darwin) targets

darwin targets need the Apple macOS SDK, which you must obtain yourself (it
cannot be redistributed). One-time setup:

```bash
make fetch-sdk X=~/Downloads/Xcode_15.4.xip
```

This extracts the SDK into `./sdks/MacOSX.sdk` (gitignored). If it is absent,
darwin targets are skipped with a warning and the remaining platforms build
normally.

## Using the released binaries

Download the archive for your platform from the GitHub Releases page and
extract it:

```bash
# Linux (amd64, glibc)
tar xzf sendme-v0.1.0-linux-amd64.tar.gz
./sendme send myfile.txt

# Linux (amd64, static musl)
tar xzf sendme-v0.1.0-linux-musl-amd64.tar.gz

# macOS (Apple Silicon)
tar xzf sendme-v0.1.0-darwin-arm64.tar.gz

# Windows
unzip sendme-v0.1.0-windows-amd64.zip
```

Always verify the checksum against the published `SHA256SUMS`:

```bash
sha256sum -c --ignore-missing SHA256SUMS
```

## Using the container image

The container packages the `sendme` CLI only — the balloon GUI needs a display
server and is not included. The image is based on `debian:bookworm-slim`,
runs as a non-root user, and weighs about 99 MB.

### Pull

```bash
podman pull ghcr.io/imp1sh/sendme-balloon:latest
# or a specific version:
podman pull ghcr.io/imp1sh/sendme-balloon:0.1.1
```

### Send a file

```bash
podman run --rm -v "$PWD:/data" ghcr.io/imp1sh/sendme-balloon send /data/myfile.txt
```

The ticket is printed to stdout. Share it with the receiver.

### Receive a file

```bash
podman run --rm -v "$PWD:/data" ghcr.io/imp1sh/sendme-balloon receive <ticket> /data
```

The downloaded file lands in `/data` (mapped to `$PWD` on the host).

### Environment variables

| Variable | Default | Description |
|----------|---------|-------------|
| `IROH_SECRET` | *(generated)* | The iroh secret key as 64 hex characters (32 bytes). Determines your node identity. If unset, a random one is generated for each run — set this for a stable identity. |
| `RUST_LOG` | `warn` | Logging level (`error`, `warn`, `info`, `debug`, `trace`). |

### Keeping a stable identity

By default each container run generates a new random secret key, meaning a
different node id every time. To keep a stable identity across runs, generate
a key once and pass it via `IROH_SECRET`:

```bash
# Generate a key (do this once, save the output)
KEY=$(openssl rand -hex 32)

# Send with a stable identity
podman run --rm -e IROH_SECRET="$KEY" -v "$PWD:/data" \
    ghcr.io/imp1sh/sendme-balloon send /data/myfile.txt
```

### Passing CLI flags

All CLI flags can be passed after the subcommand. The container's entrypoint
is `sendme`, so the first argument is the subcommand:

```bash
# Disable relay servers (direct connections only)
podman run --rm -v "$PWD:/data" \
    ghcr.io/imp1sh/sendme-balloon send --relay disabled /data/myfile.txt

# Use a custom relay server
podman run --rm -v "$PWD:/data" \
    ghcr.io/imp1sh/sendme-balloon send --relay https://my-relay.example.com /data/myfile.txt

# Bind to a fixed port (useful for firewall rules)
podman run --rm -p 1124:1124/udp -v "$PWD:/data" \
    ghcr.io/imp1sh/sendme-balloon send --magic-ipv4-addr 0.0.0.0:1124 /data/myfile.txt

# Suppress progress bars (useful in scripts/CI)
podman run --rm -v "$PWD:/data" \
    ghcr.io/imp1sh/sendme-balloon send --no-progress /data/myfile.txt

# Verbose output
podman run --rm -v "$PWD:/data" \
    ghcr.io/imp1sh/sendme-balloon send -vv /data/myfile.txt
```

### Network considerations

The CLI uses QUIC over UDP for peer-to-peer connections with NAT hole-punching.
In a container this means:

- **UDP must be allowed** — if you bind a fixed port with `--magic-ipv4-addr`,
  map it with `-p <port>:<port>/udp`
- **Relay fallback** — if direct hole-punching fails, traffic is relayed over
  TCP (port 443), which works without any special port mapping
- **No port needed for receiving** — the receiver connects outbound to the
  sender, so no inbound ports are required

### Complete reference

```bash
# Show help
podman run --rm ghcr.io/imp1sh/sendme-balloon --help

# Show version
podman run --rm ghcr.io/imp1sh/sendme-balloon --version

# Send a directory
podman run --rm -v "$PWD:/data" \
    ghcr.io/imp1sh/sendme-balloon send /data/myfolder/

# Receive with a specific relay
podman run --rm -e IROH_SECRET="$KEY" -v "$PWD:/data" \
    ghcr.io/imp1sh/sendme-balloon receive --relay default <ticket> /data
```

## License

sendme-balloon is a **derivative work** based on
[`sendme`](https://github.com/n0-computer/sendme) by
[N0, INC.](https://n0.computer).

Original work Copyright 2026 N0, INC.
Modifications and additions for sendme-balloon Copyright 2026 [Jochen Demmer](mailto:jochen@winteltosh.de>)

Both copyright holders license this project under either of

 * Apache License, Version 2.0, ([LICENSE-APACHE](LICENSE-APACHE) or
   <http://www.apache.org/licenses/LICENSE-2.0>)
 * MIT license ([LICENSE-MIT](LICENSE-MIT) or
   <http://opensource.org/licenses/MIT>)

at your option. The full texts of both licenses are included in this
repository as `LICENSE-APACHE` and `LICENSE-MIT`, unmodified from the upstream
project.

Because this is a fork, redistribution must retain both the upstream copyright
notice (N0, INC.) and the notice for the modifications (Jochen Demmer), as
required by the Apache License, Version 2.0, §4 and the MIT license.

## Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this project by you, as defined in the Apache-2.0 license,
shall be dual licensed as above, without any additional terms or conditions.
