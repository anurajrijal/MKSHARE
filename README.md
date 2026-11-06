# MKShare

**Use one keyboard and mouse on two computers.** Plug your keyboard and mouse into one laptop, press a hotkey or click a button, and they control the other laptop over your network. Switch again and they come back.

MKShare is a small, fast desktop app written in Rust. It runs on **Windows** and **Linux** (X11 and Wayland), and any combination works: Windows to Linux, Linux to Windows, Windows to Windows, or Linux to Linux.

[Download the latest release](https://github.com/anurajrijal/MKSHARE/releases/latest)

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

- **Graphical app**: an animated intro when it opens (click to skip), then pick Server or Client, enter a passphrase, press Start.
- **Finds the server for you**: the client lists MKShare servers on your network, so you do not type an IP address (and it re-finds the server by name if its IP changes).
- **Screen-edge switching** (optional): move the mouse off the edge of the screen to cross to the other computer, and push against the opposite edge to come back. You choose which side the other computer is on, and you can turn it off.
- **Your own shortcut**: switch with a keyboard shortcut you choose (default `Ctrl+Alt+K`).
- **Switch from either computer**: a button on the server, a button on the client, the shortcut, or the screen edge.
- **Shared clipboard**: copy text on one computer and paste it on the other (text only, up to 32 KB; you can turn it off).
- **Runs in the background**: on Windows, closing the window keeps MKShare in the system tray; optionally it starts when you sign in and connects by itself.
- **Built-in updates**: an **Update application** button installs new releases from GitHub.
- **Cross-platform**: Windows 10/11 and Linux (Debian/Ubuntu `.deb` provided).
- **Wayland-ready on Linux**: input is captured and injected at the kernel level (evdev/uinput), so it does not depend on X11.
- **Encrypted**: all traffic is authenticated and encrypted with a passphrase you choose.
- **Layout independent**: keys are sent by physical position, so different keyboard layouts and operating systems work together.
- **Full input**: mouse movement, all five buttons, vertical and horizontal scrolling, and the whole keyboard including media volume keys.
- **Safe switching**: keys and buttons held during a switch are released, so nothing gets stuck.
- **Self-contained**: a single executable with no runtime to install.

## How it works

### Data flow between the two computers

```text
                  SERVER                                                   CLIENT
+----------------------------------------+                 +------------------------------------+
| keyboard + mouse                       |                 |                                    |
|       |                                |                 |                                    |
|       v                                |                 |                                    |
| capture                                |                 | inject                             |
|   Windows: low-level hooks             |                 |   Windows: SendInput               |
|   Linux:   evdev (exclusive grab)      |                 |   Linux:   uinput virtual device   |
|       |                                |                 |       ^                            |
|       v                                |                 |       |                            |
| core: local or remote?                 |                 |       |                            |
|   local  -> deliver to this computer   |                 |       |                            |
|   remote -> encrypt and send           |--- input ------>| decrypt and decode                 |
|                                        |                 |                                    |
| switch with Ctrl+Alt+K or the button   |<--- Toggle -----| switch button (asks the server)    |
+----------------------------------------+                 +------------------------------------+
```

- In **local** mode the server behaves like a normal computer.
- In **remote** mode the server stops delivering input to itself and sends every event to the client instead.
- The server tells the client whether it is currently the one receiving input, and the client can ask the server to switch (`Toggle`). That is how the switch button works on both sides.
- With screen-edge switching on, the server detects the cursor reaching the chosen edge and sends `Enter` so the client places its cursor on the opposite edge at the same height. The client watches its return edge and sends `Leave` to hand input back.
- Finding servers: the client broadcasts a small UDP query (port `24801`); servers answer with their name and port. Only the name is revealed. Connecting still needs the passphrase.
- On Linux the server grabs your keyboards and mice and re-emits their events through a virtual device while in local mode. That is what makes it possible to cleanly take input away on a switch.

### One program, two roles

```text
+----------------------------------------------------------------+
| mkshare  (one program)                                         |
|                                                                |
|   no arguments            mkshare server / mkshare client      |
|   +----------------+      +-------------------------------+    |
|   | window (GUI)   | ---> | background process            |    |
|   | gui.rs         | <--- | core.rs  net.rs  platform/*   |    |
|   +----------------+      +-------------------------------+    |
|     starts and stops it      status lines come back as log     |
+----------------------------------------------------------------+
```

The window starts the real server or client as a background process, so **Stop** always releases every hook and device grab, and closing the window never leaves a server running.

## Quick start

1. **Install** MKShare on both computers (see [Installation](#installation)).
2. On the computer with the keyboard and mouse: open MKShare, choose **Server**, type a passphrase, press **Start**. Note the address it shows (for example `192.168.1.79`).
3. On the other computer: open MKShare, choose **Client**. It looks for servers by itself: click your server in the list (or type its address), enter the **same passphrase**, press **Start**.
4. Switch: press the shortcut (default **`Ctrl+Alt+K`**) on the server's keyboard, click the switch button in either window, or, if you turned it on, move the mouse off the screen edge. Your keyboard and mouse now control the other computer. Switch again to return.

Both computers must be on the same network, and the server's TCP port (default `24800`) and UDP port `24801` (for finding servers) must be reachable.

**Both computers must run the same version** of MKShare: version 0.3 added new messages that older versions do not understand.

## Installation

Download the file for your system from the **[latest release](https://github.com/anurajrijal/MKSHARE/releases/latest)**:

| System | File |
|---|---|
| Windows | `mkshare.exe` |
| Debian / Ubuntu (64-bit) | `mkshare_<version>-1_amd64.deb` |
| Other Linux (64-bit) | `mkshare-linux-x86_64` |

After the first install, use **Update application** inside the app to get new versions.

### Windows

Run `mkshare.exe`. It is a standalone program and needs no installer.

- Windows Firewall asks for permission the first time the server starts. Allow it on **private** networks.
- The executable is not code-signed, so Windows SmartScreen may warn on first run. Choose *More info*, then *Run anyway*.

### Debian / Ubuntu (22.04 or newer, 64-bit Intel/AMD)

```bash
sudo apt install ./mkshare_0.3.4-1_amd64.deb
```

This installs the program, an **MKShare** entry in your application menu, and a udev rule that gives the person logged in at the computer access to the input devices (no group changes, no logging out). Start it from the menu or by running `mkshare`. See [Linux permissions](#linux-permissions).

### Other Linux distributions

Use the standalone `mkshare-linux-x86_64` binary (needs glibc 2.31 or newer) and set up permissions as described in [Linux permissions](#linux-permissions).

## Using the app

| Field | Meaning |
|---|---|
| **Server / Client** | Server = the computer the keyboard and mouse are plugged into. Client = the computer that receives them. |
| **Server address** (client only) | IP address or hostname of the server. Use **Find servers** to pick it from a list instead: each computer is listed once, with the address your computer can reach (not a virtual adapter such as WSL or Hyper-V), and the one you click is highlighted. |
| **Port** | TCP port. Default `24800`. Must match on both sides. |
| **Passphrase** | Shared secret. Must match on both sides. |

The status indicator in the top-right corner shows what is happening: *Waiting for the other computer*, *Connected*, or *Keyboard & mouse are on the other computer* (on the client: *Using the other computer's keyboard & mouse*). A collapsible **Log** at the bottom shows details and errors.

Your settings are saved automatically in:

- Windows: `%APPDATA%\mkshare\config.json`
- Linux: `~/.config/mkshare/config.json`

The passphrase is **not** kept in that file. With **Remember passphrase** ticked (the default) it is stored in protected storage:

- Windows: encrypted with your Windows account (DPAPI) in `%APPDATA%\mkshare\pass.dpapi`
- Linux: the system keyring (GNOME Keyring / KWallet) via `secret-tool`, which the `.deb` recommends (`libsecret-tools`). Without it, a file readable only by you (`~/.config/mkshare/pass`, mode 0600).

Untick **Remember passphrase** and nothing is stored; you type it each time. Settings from older versions that held the passphrase in plain text are migrated automatically.

### Switching

You can switch from either computer:

| Where | How |
|---|---|
| Server | The **Send keyboard and mouse to the other computer** button |
| Client | The **Use the other computer's keyboard and mouse** button (it becomes **Send keyboard and mouse back to the other computer** while you are using them) |
| Server's keyboard | The switch shortcut (default `Ctrl+Alt+K`) |
| Screen edge | Optional: move the mouse off the screen edge facing the other computer |

**Changing the shortcut** (server, while stopped): under **Switching**, tick the modifiers you want (Ctrl, Alt, Shift, Win) and pick the key from the list. A shortcut needs at least one modifier, unless it is an F-key, Pause or ScrollLock, so it cannot fire while you type. It is saved and used the next time you press **Start**.

**Screen-edge switching** (server, off by default): tick **Also switch by moving the mouse to the screen edge** and choose which side the other computer is on (Left, Right, Top or Bottom). The cursor then crosses over when it reaches that edge and appears on the matching edge of the other screen at the same height. On the other computer, push the mouse against the opposite edge to come back. It does not trigger while a mouse button is held, so dragging is never interrupted. Untick the option to turn it off; the shortcut and buttons keep working either way.

- Windows uses the real cursor position across all monitors.
- On Linux the position is estimated from mouse movement, so crossing can be slightly early or late until the cursor has touched an edge once. The screen size is read from the display (`/sys/class/drm`); if it cannot be detected, set it yourself, for example `MKSHARE_SCREEN=1920x1080`. On Linux only one screen's size is used.

### Clipboard

With **Share the clipboard** ticked (the default, on both computers), text you copy on one computer can be pasted on the other, in both directions. Only text is shared (not images or files), up to 32 KB, and only text copied after MKShare started. Each computer has its own checkbox, so untick it on a computer to stop sending and receiving there. Anything you copy is sent to the other computer, so turn it off while copying passwords or other private text. The text travels over the same encrypted connection as the keyboard and mouse.

On Linux, clipboard access needs a desktop session (X11, or a Wayland desktop that supports the data-control protocol such as KDE and Sway; GNOME on Wayland does not allow it). If the clipboard cannot be opened, MKShare logs a message and keeps sharing the keyboard and mouse.

### Startup and tray

On the **Settings** page, under **Startup**:

- **Start MKShare when I sign in**: adds MKShare to your own login items (Windows: the `Run` registry key of your user; Linux: `~/.config/autostart/mkshare.desktop`). No administrator rights are needed, and unticking it removes the entry. MKShare then starts with `--background`: hidden in the tray on Windows, minimized on Linux.
- **Connect automatically when MKShare opens**: starts sharing by itself with your saved settings. Tick **Remember passphrase** on the Home page too, otherwise there is no passphrase to connect with.
- **Keep MKShare running in the tray when I close the window** (Windows, on by default): the close button hides the window and sharing continues. The tray icon (near the clock) shows the status when you hover over it; click it to open the window, or right-click it and choose **Quit** to exit completely.

Linux has no tray icon: closing the window quits MKShare.

Move or reinstall the program and the start-on-login entry points at the old location: untick and tick the option again to update it.

### Updating

Click **Update application** at the bottom of the window. MKShare checks the latest release of this project on GitHub (`anurajrijal/MKSHARE`) and, if a newer version exists, offers to install it. Installing stops sharing, replaces the program and restarts it.

- Windows and the standalone Linux binary replace themselves.
- A Debian/Ubuntu install is upgraded with `apt` after a graphical password prompt (`pkexec`). Reopen MKShare afterwards.
- If the release contains `SHA256SUMS.txt` (the provided workflow adds one), the download is verified against it.
- Requires `curl` (included with Windows 10/11 and most Linux distributions).

**Publishing a release** (maintainers): set `version` in `Cargo.toml`, commit and push, then push a matching tag:

```bash
git tag v0.3.4 && git push origin v0.3.4
```

`.github/workflows/release.yml` builds the Windows `.exe`, the Linux binary and the `.deb`, and attaches them to the release together with `SHA256SUMS.txt`.

## Command line

Running `mkshare` with no arguments opens the app. These subcommands run it headless:

```text
mkshare server [--port 24800] --pass <PASSPHRASE> [--hotkey ctrl+alt+k] [--edge left|right|top|bottom] [--no-clipboard]
mkshare client  --host <ADDRESS> [--port 24800] --pass <PASSPHRASE> [--name <SERVER NAME>] [--no-clipboard]
```

- `--no-clipboard`: do not share the clipboard on this computer.

- `--hotkey`: the switch shortcut, for example `ctrl+alt+k`, `ctrl+shift+f9` or `f12` (modifiers: `ctrl`, `alt`, `shift`, `win`).
- `--edge`: turn on screen-edge switching; the value is the side where the other computer sits. Leave it out to keep it off.
- `--name`: if the client can no longer reach the server's address, it looks for a server with this name on the network.

The passphrase can also be supplied with the `MKSHARE_PASS` environment variable, which keeps it out of the process list.

```bash
# server
MKSHARE_PASS=mysecret mkshare server

# client
MKSHARE_PASS=mysecret mkshare client --host 192.168.1.79
```

On Windows the executable is built as a GUI program. In a terminal, output may appear after the prompt returns; run it with `Start-Process -Wait -NoNewWindow` or use the app instead.

## Linux permissions

On Linux MKShare reads `/dev/input/event*` (server) and writes `/dev/uinput` (client and server passthrough). **You do not need to add yourself to any group**: a udev rule tagged `uaccess` gives the person who is logged in at the computer access to these devices (the same mechanism Steam uses for game controllers), with no logging out and in.

- **`.deb` package**: installs the rule, loads the `uinput` kernel module now and at every boot, and applies the rule immediately. It also depends on `curl` (used by **Update application**). Nothing else to do.
- **Standalone binary**: if MKShare reports a permission problem, click **Fix permissions** in the app. It asks for your password once (graphical prompt) and installs the same rule. Or do it from a terminal: `sudo ./mkshare-linux-x86_64 setup-linux`.
- **No desktop session at the machine** (for example over SSH): `uaccess` only applies to the user sitting at the computer. Use the `input` group instead: `sudo usermod -aG input $USER`, then log out and in (the rule also gives the `input` group access to `/dev/uinput`).

To do it completely by hand, copy `packaging/60-mkshare.rules` to `/etc/udev/rules.d/`, then:

```bash
sudo apt install curl            # needed by "Update application"
sudo modprobe uinput             # create /dev/uinput
echo uinput | sudo tee /etc/modules-load.d/mkshare.conf   # load it at every boot
sudo udevadm control --reload && sudo udevadm trigger --subsystem-match=input --subsystem-match=misc
```

For a quick test you can instead start MKShare with `sudo`.

> Access to the input devices lets a program run by that user read every keystroke on the machine. That is inherent to sharing a keyboard; only run MKShare builds you trust.

## Security

- Each connection derives a key from your passphrase with **PBKDF2-HMAC-SHA256** (100,000 rounds) and a random per-connection salt.
- Every message is encrypted and authenticated with **ChaCha20-Poly1305** using a counter nonce, so tampering and replay are rejected. A client that does not know the passphrase cannot complete the handshake.
- Choose a **long, unique passphrase**. Someone who can capture your network traffic could attempt to guess a weak passphrase offline.
- Keystrokes you type, including passwords, travel over this connection while input is on the other computer. Use MKShare on networks you trust.
- The passphrase is stored in protected storage (see [Using the app](#using-the-app)), never in plain text in the settings file. Anything running as your own user account can still ask the system for it, so treat the passphrase as a remote-control credential and do not reuse it elsewhere.
- Updates are downloaded over HTTPS from GitHub and checked against `SHA256SUMS.txt`. The program itself is not code-signed.

## Troubleshooting

| Problem | What to try |
|---|---|
| Client cannot connect | Check the address, that both machines are on the same network, and that the firewall allows TCP port `24800` on the server. |
| *wrong passphrase* in the server log | The passphrases differ. Retype them on both sides. |
| Switching does nothing | The client must be connected first (status *Connected*). The shortcut is read from the server's keyboard. |
| *No servers found* | The server must be running (status *Waiting*) on the same network, and the firewall must allow UDP port `24801`. You can always type the address instead. |
| The client cannot connect after updating one computer | Both computers must run the same MKShare version. Update the other one too. |
| Screen edge does nothing (Linux) | The screen size could not be detected. Set `MKSHARE_SCREEN=1920x1080` (your resolution) before starting MKShare. |
| The cursor crosses early or late (Linux) | The Linux cursor position is estimated. Touch the screen edge once to re-sync, or use the shortcut. |
| Linux: *no readable keyboard/mouse found* | See [Linux permissions](#linux-permissions): join the `input` group and log out and in, or run with `sudo`. |
| Linux: cannot open `/dev/uinput` | The udev rule is not active yet. Reload udev rules and log in again, or use `sudo`. |
| Linux server: a newly plugged-in device is ignored | Press **Stop** then **Start**. Devices are detected when the server starts. |
| Windows server: does not work over an admin window | Windows ignores input hooks for elevated windows. Run MKShare as administrator too. |
| A key seems stuck | Switch twice (`Ctrl+Alt+K` twice). Switching releases every held key. |
| *No release has been published* when updating | The repository has no GitHub release yet. Push a version tag (see [Updating](#updating)). |

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

**Tests**

```bash
cargo test
```

## Project layout

```text
MKSHARE/
|-- Cargo.toml                 package manifest and .deb metadata (version lives here)
|-- Cargo.lock
|-- build.rs                   embeds the app icon into mkshare.exe
|-- README.md
|-- .gitignore
|-- .github/
|   `-- workflows/
|       `-- release.yml        builds and publishes a release when a v* tag is pushed
|-- assets/
|   |-- icon.png               window and Linux app icon
|   `-- icon.ico               Windows executable icon
|-- packaging/
|   |-- 60-mkshare.rules       udev rule (uaccess: logged-in user may use the input devices)
|   |-- mkshare-modules.conf   loads the uinput kernel module at boot
|   |-- debian/postinst        .deb post-install: load uinput, apply the udev rule
|   `-- mkshare.desktop        Linux application-menu entry
`-- src/
    |-- main.rs                entry point: no arguments = GUI; "server" / "client" = headless
    |-- gui.rs                 desktop window (egui): settings, status, buttons, log
    |-- autostart.rs           start on login (Windows Run key / Linux autostart file)
    |-- setup.rs               Linux "setup-linux" command / Fix permissions button (installs the udev rule)
    |-- tray.rs                Windows tray icon (open, quit, status tooltip); no-op on Linux
    |-- clipboard.rs           clipboard text sharing (background thread that watches and sets the clipboard)
    |-- core.rs                switch shortcut, edge detection and local/remote routing on the server
    |-- edge.rs                screen-edge geometry and cursor tracking (platform independent, unit-tested)
    |-- discovery.rs           finds servers on the LAN (UDP broadcast query / reply)
    |-- net.rs                 encrypted, authenticated message framing over TCP
    |-- proto.rs               wire messages, key-code conventions, the switch shortcut type
    |-- secret.rs              protected passphrase storage (Windows DPAPI / Linux keyring)
    |-- update.rs              "Update application": check, download, install from GitHub Releases
    `-- platform/
        |-- mod.rs             picks the backend for the current OS
        |-- windows.rs         low-level hooks (capture) and SendInput (inject)
        `-- linux.rs           evdev exclusive grab (capture) and uinput (inject)
```

Local build output goes to `target/` and `dist/`; both are git-ignored.

## Limitations

- Screen-edge switching on Linux is approximate (see above), and it supports one display there.
- One client at a time.
- Server discovery needs UDP broadcast, which some Wi-Fi networks block (typing the address always works).
- Linux server mode shares keyboards and mice, not touchpads.
- The clipboard shares plain text only (no images, files or formatted text). There is no file sharing.
- Pointer speed on the client follows that machine's own pointer settings.
- The connection is not forward-secret and uses a pre-shared passphrase rather than certificates.

## License

MIT, as declared in `Cargo.toml`.
