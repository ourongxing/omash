```text
 ██████╗ ███╗   ███╗ █████╗ ███████╗██╗  ██╗
██╔═══██╗████╗ ████║██╔══██╗██╔════╝██║  ██║
██║   ██║██╔████╔██║███████║███████╗███████║
██║   ██║██║╚██╔╝██║██╔══██║╚════██║██╔══██║
╚██████╔╝██║ ╚═╝ ██║██║  ██║███████║██║  ██║
 ╚═════╝ ╚═╝     ╚═╝╚═╝  ╚═╝╚══════╝╚═╝  ╚═╝
```

`omash` is forked from
[Clash Verge Rev](https://github.com/clash-verge-rev/clash-verge-rev) and
reworked as a fast, native terminal dashboard for Mihomo, built for
[Omarchy](https://omarchy.org/). It carries the upstream Mihomo management
design into a Rust TUI without a browser runtime.

The TUI is only the control surface. Mihomo runs under a user-level supervisor,
so closing `omash` does not stop your proxy.

<p align="center">
  <img src="screenshots/1.jpg" alt="omash with a blue Omarchy theme" width="49%">
  <img src="screenshots/2.jpg" alt="omash with an orange Omarchy theme" width="49%">
</p>

## Built for Omarchy

- **Uses system packages.** `omash` runs `/usr/bin/mihomo` and reuses the
  `Country.mmdb` provided by `clash-geoip`. Mihomo and GeoIP updates stay with
  pacman and `omarchy update`.
- **Starts with your desktop session.** `omash-supervisor.service` is attached
  to `graphical-session.target`. It starts Mihomo on login, restarts it after a
  crash, and keeps it running when the TUI exits. You can toggle login startup
  from Settings.
- **Follows the active Omarchy theme.** The TUI reads the current Omarchy color
  palette and updates live when the theme changes. No restart or theme setup is
  required.
- **Handles proxy variables for Omarchy apps.** System Proxy updates both
  `gsettings` and the UWSM/systemd user environment. Newly launched apps,
  including Chrome, inherit the current proxy settings.
- **Includes an Omarchy Shell widget.** The optional Quickshell bar widget can
  change the routing mode, select proxies, and run delay tests without opening
  the TUI.

## Install

On Omarchy, install the dependencies first:

```bash
omarchy pkg aur add mihomo clash-geoip

# Run these two commands only when Cargo is not already installed.
omarchy install dev-env rust
source "$HOME/.cargo/env"
```

Then install `omash` into your home directory:

```bash
curl -fsSL https://raw.githubusercontent.com/ourongxing/omash/main/scripts/install | bash
```

The installer verifies that the dependencies are available, builds the release
tag pinned by the installer, and installs the binary and user service under your
home directory. It does not install system packages or use `sudo`. Temporary
source and build files are removed when the installer exits.

The default installation locations have changed from the system-wide `/usr`
layout used by earlier releases. The installer now writes only:

- `~/.local/bin/omash`
- `$XDG_CONFIG_HOME/systemd/user/omash-supervisor.service`, or
  `~/.config/systemd/user/omash-supervisor.service` when `XDG_CONFIG_HOME` is
  unset

### Update

Do not install a new version over an existing installation. Because the default
locations changed, update by uninstalling the current or legacy version first,
then run the installer again:

```bash
curl -fsSL https://raw.githubusercontent.com/ourongxing/omash/main/scripts/uninstall | bash
curl -fsSL https://raw.githubusercontent.com/ourongxing/omash/main/scripts/install | bash
```

The uninstaller preserves your omash configuration, profiles, logs, and
backups. It removes the optional Omarchy Shell widget; if you were using it,
add it again after the update as described in [Omarchy Shell widget](#omarchy-shell-widget).

To build and install from source instead:

```bash
omarchy pkg aur add mihomo clash-geoip

# Run these two commands only when Cargo is not already installed.
omarchy install dev-env rust
source "$HOME/.cargo/env"

git clone https://github.com/ourongxing/omash.git
cd omash
cargo build --locked --release
install -Dm755 target/release/omash "$HOME/.local/bin/omash"
systemd_user_dir="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
install -Dm644 systemd/omash-supervisor.service \
  "$systemd_user_dir/omash-supervisor.service"
sed -i 's|^ExecStart=.*|ExecStart=%h/.local/bin/omash --daemon|' \
  "$systemd_user_dir/omash-supervisor.service"
systemctl --user daemon-reload
```

Run the dashboard:

```bash
omash
```

On first launch, `omash` creates its configuration, enables the user service
when `auto_start` is enabled, and starts the supervisor for the current session.
It also generates a random API secret; you do not need to configure or start
Mihomo separately.

## What it does

- Imports local YAML profiles and remote subscriptions
- Validates profile changes before replacing the running configuration
- Switches Rule, Global, and Direct modes
- Selects proxies and runs delay tests
- Shows and closes active connections
- Supports Merge, prepend/append, Rules, Proxies, and Groups enhancements in
  native Rust
- Controls the system proxy, LAN access, IPv6, and refresh intervals
- Updates remote subscriptions on schedule
- Creates and restores local ZIP backups
- Shows Mihomo logs and package versions

The Mihomo management behavior comes from Clash Verge Rev's Rust
implementation, adapted to a native TUI and Omarchy's system layout.

## Controls

The layout uses a sidebar in wide terminals and switches to a top navigation bar
when space is limited. Press `?` at any time to show all shortcuts.

| Key | Action |
| --- | --- |
| `1`-`8` | Open a page |
| `Up` / `Down`, `j` / `k` | Move the selection |
| `Tab`, `Left` / `Right`, `h` / `l` | Switch between proxy groups and nodes |
| `Enter` | Run the selected action |
| `r` | Refresh now |
| `?` | Toggle shortcut help |
| `q`, `Ctrl-C` | Exit the TUI without stopping Mihomo |

Mouse clicks, scrolling, and double-click actions are also supported.

## Omarchy Shell widget

The widget requires the `omash` binary installed above. Add it to Omarchy Shell
directly from the public repository:

```bash
omarchy plugin add https://github.com/ourongxing/omash.git --enable
```

The widget appears in the bar immediately through Omarchy Shell's hot reload.
Its two-column panel lists selector groups on the left and their proxies on the
right. It preserves the order from your Mihomo profile, shows current
selections and measured delays, and switches between Rule, Global, and Direct
modes.

The widget calls `omash bar` and never reads or copies the Mihomo API secret.
The `omash` binary must be available on the Shell's `PATH`. Its refresh interval
can be changed from 2 to 60 seconds in the plugin settings.

## Remove

Remove the Shell widget, user service, installed binary, and files left by
earlier system-wide installers with:

```bash
curl -fsSL https://raw.githubusercontent.com/ourongxing/omash/main/scripts/uninstall | bash
```

From a source checkout, run `scripts/uninstall` instead. The uninstaller also
uses pacman when `omash` was installed as a package, and requests `sudo` only
when package-managed or legacy system files need removal.

The uninstaller preserves your configuration, profiles, logs, and backups.
To delete those user files too, remove `~/.config/omash` and
`~/.local/share/omash` after making any backup you want to keep.

## Theme overrides

By default, `omash` continuously follows the current Omarchy palette. To use a
separate TUI theme instead, create `~/.config/omash/theme.toml`:

```bash
mkdir -p ~/.config/omash
cp themes/default.toml ~/.config/omash/theme.toml
```

Theme overrides reload while `omash` is running. Remove the file to follow
Omarchy again. All colors use `#RRGGBB`; see
[`themes/default.toml`](themes/default.toml) for the available fields.

## Configuration

The main configuration file is `~/.config/omash/config.toml`. Runtime data,
profiles, logs, and backups live in `~/.local/share/omash/`.

```toml
controller = "http://127.0.0.1:9090"
secret = ""
refresh_ms = 1500
delay_test_url = "https://www.gstatic.com/generate_204"
auto_start = true
mixed_port = 7897
allow_lan = false
ipv6 = true
system_proxy = true
proxy_bypass = "localhost,127.0.0.1,::1,192.168.0.0/16,10.0.0.0/8,172.16.0.0/12"
```

`OMASH_REFRESH_MS` and `--refresh-ms` override the configured refresh interval.
Use `--config <path>` to load a different configuration file.

## Development

```bash
cargo test
cargo build --release
```

## License

As a fork of Clash Verge Rev, `omash` remains licensed under GPL-3.0-only. The
full, unmodified license is retained in [`LICENSE`](LICENSE).
