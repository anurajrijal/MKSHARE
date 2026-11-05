# MKShare

**Use one keyboard and mouse on two computers.** Plug your keyboard and mouse into one laptop, press a hotkey, and they control the other laptop over your network. Press it again and they come back.

MKShare is a small, fast, open desktop app written in Rust. It runs on **Windows** and **Linux** (X11 and Wayland), and any combination works: Windows to Linux, Linux to Windows, Windows to Windows, or Linux to Linux.

---

## Contents

- [Features](#features)
- [How it works](#how-it-works)
- [Quick start](#quick-start)
- [Installation](#installation)
- [Using the app](#using-the-app)
- [Command line](#command-line)
- [Linux permissions](#linux-permissions)
- [Security](#security)
- [Troubleshooting](#troubleshooting)
- [Building from source](#building-from-source)
- [Project layout](#project-layout)
- [Limitations](#limitations)
- [License](#license)

---

## Features

- **Graphical app**: pick Server or Client, enter a passphrase, press Start.
- **Switch from either computer**: a button on the server, a button on the client, or the `Ctrl+Alt+K` hotkey.
- **Built-in updates**: an **Update application** button installs new releases from GitHub.
- **Cross-platform**: Windows 10/11 and Linux (Debian/Ubuntu `.deb` provided).
- **Wayland-ready on Linux**: input is captured and injected at the kernel level (evdev/uinput), so it does not depend on X11.
- **Encrypted**: all traffic is authenticated and encrypted with a passphrase you choose.
- **Layout independent**: keys are sent by physical position, so different keyboard layouts and operating systems work together.
- **Full input**: mouse movement, all five buttons, vertical and horizontal scrolling, and the whole keyboard including media volume keys.
- **Safe switching**: keys and buttons held during a switch are released, so nothing gets stuck.
- **Self-contained**: a single executable with no runtime to install.

## How it works

```
   SERVER (has the keyboard + mouse)                CLIENT (receives them)
  ┌──────────────────────────────────┐          ┌──────────────────────────────┐
  │ keyboard / mouse                 │          │                              │
  │      │                           │          │                              │
  │      ▼                           │          │                              │
  │ capture                          │  TCP     │  inject                      │
  │  Windows: low-level hooks        │ ───────► │   Windows: SendInput         │
  │  Linux:   evdev (exclusive grab) │ encrypted│   Linux:   uinput device     │
  │      │                           │          │                              │
  │  Ctrl+Alt+K toggles local/remote │          │                              │
  └──────────────────────────────────┘          └──────────────────────────────┘
```

- In **local** mode the server behaves like a normal computer.
- In **remote** mode the server stops delivering input to itself and sends every event to the client instead.
- On Linux the server grabs your keyboards and mice and re-emits their events through a virtual device while in local mode. That is what makes it possible to cleanly take input away on switch.
- The GUI starts the actual server or client as a background process, so **Stop** always releases every hook and device grab.

## Quick start

1. **Install** MKShare on both computers (see [Installation](#installation)).
2. On the computer with the keyboard and mouse: open MKShare, choose **Server**, type a passphrase, press **Start**. Note the address it shows (for example `192.168.1.79`).
3. On the other computer: open MKShare, choose **Client**, enter that address and the **same passphrase**, press **Start**.
4. On the server, press **`Ctrl+Alt+K`** (or the *Send keyboard and mouse to the other computer* button). Your keyboard and mouse now control the other computer. Press `Ctrl+Alt+K` again to return.

Both computers must be on the same network, and the server's TCP port (default `24800`) must be reachable.

## Installation

### Windows

Run `mkshare.exe`. It is a standalone program and needs no installer.

- Windows Firewall asks for permission the first time the server starts. Allow it on **private** networks.
- The executable is not code-signed, so Windows SmartScreen may warn on first run. Choose *More info*, then *Run anyway*.

### Debian / Ubuntu (22.04 or newer, 64-bit Intel/AMD)

```bash
sudo apt install ./mkshare_0.1.0-1_amd64.deb
sudo usermod -aG input $USER      # then log out and back in
```

This installs the program, an **MKShare** entry in your application menu, and a udev rule that lets members of the `input` group create virtual input devices. Start it from the menu or by running `mkshare`.

### Other Linux distributions

Use the standalone `mkshare-linux-x86_64` binary (needs glibc 2.31 or newer) and set up permissions as described in [Linux permissions](#linux-permissions).

## Using the app

| Field | Meaning |
|---|---|
| **Server / Client** | Server = the computer the keyboard and mouse are plugged into. Client = the computer that receives them. |
| **Server address** (client only) | IP address or hostname of the server. |
| **Port** | TCP port. Default `24800`. Must match on both sides. |
| **Passphrase** | Shared secret. Must match on both sides. |

The status indicator in the top-right corner shows what is happening: *Waiting for the other computer*, *Connected*, or *Keyboard & mouse are on the other computer*. A collapsible **Log** at the bottom shows details and errors.

Your settings are saved automatically in:

- Windows: `%APPDATA%\mkshare\config.json`
- Linux: `~/.config/mkshare/config.json`

> The passphrase is stored in that file in plain text.

### Switching

You can switch from either computer:

| Where | How |
|---|---|
| Server | The **Send keyboard and mouse to the other computer** button |
| Client | The **Use the other computer's keyboard and mouse** button (it becomes **Send keyboard and mouse back to the other computer** while you are using them) |
| Server's keyboard | `Ctrl` + `Alt` + `K`, from the keyboard that is plugged into the server |

### Updating

Click **Update application** at the bottom of the window. MKShare checks the latest release of the GitHub repository set under **Update settings** (`owner/name`) and, if a newer version exists, offers to install it. Installing stops sharing, replaces the program and restarts it.

- Windows and the standalone Linux binary replace themselves.
- A Debian/Ubuntu install is upgraded with `apt` after a graphical password prompt (`pkexec`). Reopen MKShare afterwards.
- If the release contains `SHA256SUMS.txt` (the provided workflow adds one), the download is verified against it.
- Requires `curl` (included with Windows 10/11 and most Linux distributions).

**Publishing a release** (maintainers): set `version` in `Cargo.toml`, then push a matching tag:

```bash
git tag v0.2.0 && git push origin v0.2.0
```

`.github/workflows/release.yml` builds the Windows `.exe`, the Linux binary and the `.deb`, and attaches them to the release together with `SHA256SUMS.txt`.

## Command line

Running `mkshare` with no arguments opens the app. These subcommands run it headless:

```text
mkshare server [--port 24800] --pass <PASSPHRASE>
mkshare client  --host <ADDRESS> [--port 24800] --pass <PASSPHRASE>
```

The passphrase can also be supplied with the `MKSHARE_PASS` environment variable, which keeps it out of the process list.

```bash
# server
MKSHARE_PASS=mysecret mkshare server

# client
MKSHARE_PASS=mysecret mkshare client --host 192.168.1.79
```

On Windows the executable is built as a GUI program. In a terminal, output may appear after the prompt returns; run it with `Start-Process -Wait -NoNewWindow` or use the app instead.

## Linux permissions

On Linux MKShare reads `/dev/input/event*` (server) and writes `/dev/uinput` (client and server passthrough). The `.deb` package installs the udev rule for you. To do it by hand:

```bash
sudo usermod -aG input $USER
echo 'KERNEL=="uinput", GROUP="input", MODE="0660", OPTIONS+="static_node=uinput"' \
  | sudo tee /etc/udev/rules.d/99-mkshare.rules
sudo udevadm control --reload && sudo udevadm trigger
# log out and back in
```

For a quick test you can instead start MKShare with `sudo`.

> Membership of the `input` group lets a program read every keystroke on the machine. Only add users you trust.

## Security

- Each connection derives a key from your passphrase with **PBKDF2-HMAC-SHA256** (100,000 rounds) and a random per-connection salt.
- Every message is encrypted and authenticated with **ChaCha20-Poly1305** using a counter nonce, so tampering and replay are rejected. A client that does not know the passphrase cannot complete the handshake.
- Choose a **long, unique passphrase**. Someone who can capture your network traffic could attempt to guess a weak passphrase offline.
- Keystrokes you type, including passwords, travel over this connection while input is on the other computer. Use MKShare on networks you trust.
- The passphrase is stored unencrypted in the local config file.

## Troubleshooting

| Problem | What to try |
|---|---|
| Client cannot connect | Check the address, that both machines are on the same network, and that the firewall allows TCP port `24800` on the server. |
| *wrong passphrase* in the server log | The passphrases differ. Retype them on both sides. |
| Hotkey does nothing | The client must be connected first (status *Connected*). The hotkey is read from the server's keyboard. |
| Linux: *no readable keyboard/mouse found* | See [Linux permissions](#linux-permissions): join the `input` group and log out and in, or run with `sudo`. |
| Linux: cannot open `/dev/uinput` | The udev rule is not active yet. Reload udev rules and log in again, or use `sudo`. |
| Linux server: a newly plugged-in device is ignored | Press **Stop** then **Start**. Devices are detected when the server starts. |
| Windows server: does not work over an admin window | Windows ignores input hooks for elevated windows. Run MKShare as administrator too. |
| A key seems stuck | Press `Ctrl+Alt+K` twice. Switching releases every held key. |

## Building from source

Requires [Rust](https://rustup.rs) (stable).

**Windows**: also install the *Desktop development with C++* tools (Visual Studio Build Tools).

```powershell
cargo build --release
# target\release\mkshare.exe
```

**Linux**: install a C toolchain (`build-essential`), then:

```bash
cargo build --release
# target/release/mkshare
```

**Debian package**

```bash
cargo install cargo-deb
cargo build --release
cargo deb --no-build
# target/debian/mkshare_<version>_amd64.deb
```

To make a binary that runs on older distributions, link against an older glibc with [`cargo-zigbuild`](https://github.com/rust-cross/cargo-zigbuild):

```bash
cargo zigbuild --release --target x86_64-unknown-linux-gnu.2.31
```

## Project layout

```text
MKSHARE/
├── Cargo.toml
├── build.rs                  # embeds the icon into mkshare.exe
├── assets/                   # app icon (png, ico)
├── packaging/                # Debian: udev rule, .desktop launcher
└── src/
    ├── main.rs               # entry point: GUI, or server/client subcommands
    ├── gui.rs                # desktop window (egui)
    ├── core.rs               # hotkey handling and local/remote routing
    ├── net.rs                # encrypted, authenticated framing over TCP
    ├── proto.rs              # wire messages and key code conventions
    ├── update.rs             # self-update from GitHub Releases
    └── platform/
        ├── windows.rs        # low-level hooks + SendInput
        └── linux.rs          # evdev grab + uinput
```

## Limitations

- Switching is by hotkey or button only (no moving the cursor off the screen edge).
- One client at a time.
- Linux server mode shares keyboards and mice, not touchpads.
- No clipboard or file sharing.
- Pointer speed on the client follows that machine's own pointer settings.
- The connection is not forward-secret and uses a pre-shared passphrase rather than certificates.

## License

MIT, as declared in `Cargo.toml`.
