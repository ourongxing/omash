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

## Features

- Imports local profiles and remote subscriptions, with scheduled updates
- Runs either the system Mihomo or sing-box core, auto-detected per profile
- Supports Rule, Global, and Direct modes, proxy selection, and delay tests
- Manages active connections, Merge enhancements, backups, and logs
- Uses the system Mihomo and GeoIP packages maintained by Omarchy
- Keeps Mihomo running through a user service after the TUI closes
- Updates `gsettings` and the UWSM/systemd environment for newly launched apps
- Follows the active Omarchy palette, with optional theme overrides
- Provides an optional Omarchy Shell widget for common controls

## Install

Install the system dependencies first:

```bash
omarchy pkg aur add mihomo clash-geoip

# Only needed for sing-box profiles:
omarchy pkg pacman add sing-box

# Only needed when Cargo is not already installed:
omarchy install dev-env rust
source "$HOME/.cargo/env"
```

Install `omash`, then launch it once to finish setup:

```bash
curl -fsSL https://raw.githubusercontent.com/ourongxing/omash/main/scripts/install | bash
omash
```

The installer writes the binary and user service under your home directory; it
does not install system packages or use `sudo`. On first launch, `omash` creates
its configuration, starts the supervisor, and enables login startup because
`auto_start = true` by default.

### Install from source

After installing the dependencies above, build and install manually:

```bash
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

omash
```

### Optional Shell widget

The installer does not add the Omarchy Shell widget. Install it from the public
repository:

```bash
omarchy plugin add https://github.com/ourongxing/omash.git --enable
```

Or install it from a source checkout while in the repository root:

```bash
mkdir -p ~/.config/omarchy/plugins
cp -r integrations/omarchy/ourongxing.omash ~/.config/omarchy/plugins/
omarchy plugin enable ourongxing.omash --section right
```

The widget appears immediately and supports mode changes, proxy selection, and
delay tests. It calls `omash bar` and does not access the Mihomo API secret.

### Update

From `0.1.3` or newer, rerun the installer to update in place:

```bash
curl -fsSL https://raw.githubusercontent.com/ourongxing/omash/main/scripts/install | bash
```

Versions before `0.1.3` used a system-wide `/usr` layout and must be uninstalled
first:

```bash
curl -fsSL https://raw.githubusercontent.com/ourongxing/omash/main/scripts/uninstall | bash
curl -fsSL https://raw.githubusercontent.com/ourongxing/omash/main/scripts/install | bash
```

Configuration, profiles, logs, and backups are preserved. The old-version
uninstaller removes the optional widget, so add it again after updating.

## Controls

| Key | Action |
| --- | --- |
| `1`-`8` | Open a page |
| `Up` / `Down`, `j` / `k` | Move the selection |
| `Tab`, `Left` / `Right`, `h` / `l` | Switch between proxy groups and nodes |
| `Enter` | Run the selected action |
| `r` | Refresh now |
| `?` | Toggle shortcut help |
| `q`, `Ctrl-C` | Exit the TUI without stopping Mihomo |

Press `?` for the complete shortcut list. Mouse input is also supported.

## CLI

Besides the TUI, `omash` provides commands to control the proxy core from
scripts and the shell. They go through the supervisor, so the system proxy is
applied and cleared consistently:

```bash
omash stop     # stop the core and clear the system proxy
omash start    # start the core and re-apply the system proxy
omash restart  # restart the core (e.g. after editing config.toml)
```

Use them for controlled configuration changes, for example when moving the
proxy to a different port: `omash stop`, edit `mixed_port` in
`~/.config/omash/config.toml`, then `omash start`. Note that applications
launched through UWSM/systemd keep the proxy port captured at launch time, so
running applications may need a restart after a port change.

## Remove

Remove the widget, user service, binary, and legacy system files with:

```bash
curl -fsSL https://raw.githubusercontent.com/ourongxing/omash/main/scripts/uninstall | bash
```

The uninstaller preserves `~/.config/omash` and `~/.local/share/omash`, which
contain your configuration, profiles, logs, and backups. Remove those
directories manually if you also want to delete user data.

## Configuration

The main configuration file is `~/.config/omash/config.toml`. Runtime data is
stored in `~/.local/share/omash/`.

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

### sing-box profiles

In addition to Clash/Mihomo YAML, profiles may be sing-box JSON configs,
either imported from a local file or served from a subscription URL. omash
detects the format from the profile content and runs the matching system core
(`/usr/bin/mihomo` or `/usr/bin/sing-box`). For sing-box it injects a managed
`experimental.clash_api`, a mixed inbound on `mixed_port`, and `clash_mode`
route rules for Global/Direct (unless the config already defines its own), so
the dashboard, mode switching, delay tests, and the system proxy keep working
the same way. Clash-style merge and prepend/append enhancement chains only
apply to Mihomo profiles.

#### TUN permissions

sing-box profiles with a `tun` inbound need permission to create the TUN
interface and manage routes. Grant capabilities to the system binary:

```bash
sudo setcap cap_net_admin,cap_net_bind_service=+ep /usr/bin/sing-box
getcap /usr/bin/sing-box
```

The verification command should print:

```text
/usr/bin/sing-box cap_net_bind_service,cap_net_admin=ep
```

Then restart the core with `omash restart`. A profile can pass import validation
but fail to start without these permissions; the core log will report
`TUNSETIFF: operation not permitted`. Logs are under
`~/.local/share/omash/logs/`.

Updating or reinstalling the sing-box package may clear these capabilities.
Check them again and reapply the command if needed. If you only need an HTTP
or SOCKS proxy, remove the `tun` inbound from your profile and use a mixed
inbound instead.

### Theme override

The TUI follows the current Omarchy palette by default. To override it:

```bash
mkdir -p ~/.config/omash
cp themes/default.toml ~/.config/omash/theme.toml
```

Changes reload automatically. Remove the file to follow Omarchy again; available
fields are documented in [`themes/default.toml`](themes/default.toml).

## Development

```bash
cargo test
cargo build --locked --release
```

## License

As a fork of Clash Verge Rev, `omash` remains licensed under GPL-3.0-only. The
full, unmodified license is retained in [`LICENSE`](LICENSE).
