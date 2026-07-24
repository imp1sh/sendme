# sendme-balloon

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg?style=flat-square)](LICENSE-MIT)
[![License: Apache 2.0](https://img.shields.io/badge/License-Apache%202.0-blue.svg?style=flat-square)](LICENSE-APACHE)


**sendme-balloon** is a fork of [`n0-computer/sendme`](https://github.com/n0-computer/sendme). **sendme-balloon** is 
maintained by [Jochen Demmer](mailto:jochen@winteltosh.de). The original project
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

The desktop app is gated behind the `balloon` feature, which is not enabled by
default, so it has to be enabled explicitly:

```
cargo install sendme --features balloon
```

This installs both the `sendme` command-line tool and the `sendme-balloon`
desktop app. Building without `--features balloon` yields only the CLI.

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
