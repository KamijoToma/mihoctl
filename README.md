# mihoctl

A headless manager for the [mihomo](https://github.com/MetaCubeX/mihomo) core on macOS, Linux and FreeBSD. It replaces GUI clients (FlClash, Clash Verge) with a single settings file:

- **Subscriptions are node sources only.** mihoctl keeps each subscription's `proxies` and drops its groups, rules and DNS. Each subscription becomes an `inline` proxy-provider with a name prefix (`[name] `), its nodes embedded in the generated config. Your template owns groups, rules, DNS and TUN.
- **Overrides** run in the order of the `overrides` list, each one receiving the previous result. A directory entry expands to its `*.js`, `*.yaml` and `*.yml` files, sorted by file name: `10-a.js` runs before `99-b.js`. Directories are not searched recursively and dotfiles are skipped. Sorting is byte-wise, so zero-pad numeric prefixes (`05-`, not `5-`). Two kinds of override are supported:
  - YAML deep-merge using Clash Party operators: `key!` replaces the whole value, `+key` prepends to a list, `key+` appends, and `<key>` escapes a key that would otherwise read as an operator.
  - Clash Verge / Party style JS: `function main(config, profileName) { …; return config }`, sync or async. Scripts run in an embedded QuickJS with no filesystem, network, timers or process access, a 128 MiB heap, and the `script_timeout` limit.
- **Fail closed.** A throwing script, a config rejected by `mihomo -t`, or an unreachable subscription stops the run. The last good config and node cache stay in place; nothing silently falls back to an unmodified config.
- **Hot reload with rollback.** Each apply runs: `mihomo -t` → atomic swap → `PUT /configs?force=true` (config sent as `payload`) → a URL test of the `health_check.group` members. If no member answers, the previous config is restored and reloaded. Because the `tun:` block stays unchanged across reloads, the TUN device is not recreated.
- **Service units.** The core runs as a root system service because TUN needs root: a LaunchDaemon (`io.mihoctl.core`) on macOS, `mihoctl-core.service` on Linux, and `mihoctl_core` supervised by `daemon(8)` on FreeBSD. `mihoctl refresh` runs every `refresh_interval` as a user job: a LaunchAgent (`io.mihoctl.refresh`), a user timer (`mihoctl-refresh.timer`), or a user crontab `@N` entry. See [Platforms](#platforms).
- **Privilege split.** Nothing the root core executes or writes is user-writable, so code running as your user can't use it to get root. `service install` copies `mihomo` to a root-owned path (macOS: `/Library/Application Support/mihoctl/bin/mihomo`). The core's home (`-d`: cache, ui, geodata) is root-owned too (macOS: `/Library/Application Support/mihoctl/core`). Both run with umask 077. The core reads only `<core_dir>/config.yaml`, via `-f`. So `type: file` proxy/rule-providers are rejected at build time: use `type: inline` or `type: http`. `mihomo -t` validates with the installed copy, so validation and the core always run the same version. **Rerun `mihoctl service install` after upgrading mihomo**; until then both keep using the old copy.
- **zashboard** is served by the core from its root-owned home and is reachable only on loopback, behind the controller secret.

## Install

```sh
brew install mihomo
cargo install --path .
mkdir -p ~/.config/mihoctl && chmod 700 ~/.config/mihoctl
# write ~/.config/mihoctl/mihoctl.toml (mode 0600) and template.yaml, see below
mihoctl update            # download subscriptions into the node cache
mihoctl apply             # build + validate + install <core_dir>/config.yaml
mihoctl service install   # root LaunchDaemon for the core + user LaunchAgent for refresh
open "$(mihoctl ui)"
```

The geodata files (`GEOSITE.dat`, `geoip.metadb`, `ASN.mmdb`) are needed in `core_dir` for `mihomo -t`. `service install` copies them into the core's home if they are missing there. Both download missing files on first use. If GitHub is blocked, copy them into `core_dir` from an existing client before `service install`. Install never overwrites geodata the core already has. To replace a bad copy, delete it first: `sudo rm "/Library/Application Support/mihoctl/core/GEOSITE.dat"`, then reinstall.

**Upgrading from the old layout** (core ran with `-d <core_dir>`): run `mihoctl apply` to rebuild with inline providers, then `mihoctl service install`. Install refuses a stale `config.yaml`. The old core left root-owned files behind, which you can remove: `sudo rm -rf "<core_dir>/cache.db" "<core_dir>/ui" ~/Library/Logs/mihoctl/core.log`.

## mihoctl.toml

```toml
template = "template.yaml"                       # relative to this file
overrides = ["overrides"]                        # dir: *.js/*.yaml sorted by name; files and dirs can be mixed
secret = "long-random-string"                    # required; controller auth
# controller = "127.0.0.1:9090"                  # must be loopback
# core_dir = "~/Library/Application Support/mihoctl/core"   # Linux/FreeBSD: $XDG_DATA_HOME/mihoctl/core
# mihomo = "/opt/homebrew/opt/mihomo/bin/mihomo"            # Linux/FreeBSD: first `mihomo` on PATH
# user_agent = "clash.meta"                      # airports pick the response format from this
# refresh_interval = "6h"
# script_timeout = "5s"
# exclude_filter = "剩余|流量|到期|官网|套餐|…"   # banner "nodes" dropped from every provider
# ui_url = "https://github.com/Zephyruso/zashboard/releases/latest/download/dist.zip"

[health_check]
group = "自动选择"                                # url-test group probed after each reload
# url = "https://www.gstatic.com/generate_204"
# timeout_ms = 5000

[[subscription]]
name = "airport"                                 # [A-Za-z0-9_-]+, becomes the provider name
url = "https://…"
# via = "auto"      # auto: through the running core's mixed port, then direct; or "proxy" / "direct"
# user_agent = "…"  # per-subscription override
# prefix = "[airport] "
# expires = false   # pay-as-you-go plan: ignore the airport's `expire`, never warn about it
```

Whatever the template or overrides say, mihoctl itself always sets `external-controller`, `secret`, `external-ui` and `external-ui-url`. This keeps the controller on loopback with a secret.

Subscriptions must return mihomo YAML when requested with `user_agent`. Placeholder nodes that point at port 0 or 1 are dropped. Responses in any other format are rejected, and the previous cache is kept.

## Commands

| Command | Effect |
|---|---|
| `mihoctl update [name…] [--proxy URL]` | Download subscriptions concurrently and replace each cache file that validates |
| `mihoctl history [subscription]` | List retained node-content hashes, first-seen Unix timestamps, node counts and the current marker |
| `mihoctl restore <subscription> [version]` | Restore one subscription's retained nodes, rebuild with current settings, validate, install and, if running, reload/probe |
| `mihoctl build [-o file]` | Generate the config and run it through `mihomo -t`; print it or write it to a file |
| `mihoctl diff` | Build and validate, then print a unified diff against the installed `config.yaml`. The secret is masked and nodes appear by name only (no servers or credentials), so the output is safe to paste |
| `mihoctl apply` | Build, validate, install and hot-reload, then health-check; roll back on failure |
| `mihoctl refresh` | `update` + `apply` (the scheduled job) |
| `mihoctl status [--json]` | service state, core version/mode/TUN, per-subscription node counts, traffic/expiry, last errors. `--json` is for scripts and never includes subscription URLs or the secret |
| `mihoctl ui [--update]` | zashboard URL pre-filled with controller and secret |
| `mihoctl ui --tailscale [--path P] [--port N] [--unpublish]` | Publish the dashboard on your tailnet with Tailscale Serve and print that URL; `--unpublish` removes it |
| `mihoctl service install\|uninstall\|restart\|print` | Manage the launchd/systemd/rc.d units. `restart` is needed after editing `tun:`. `uninstall` also deletes the root-owned binary and core home |

Root steps (`service install/uninstall/restart`) run as a single `sudo` call. Set `SUDO_ASKPASS` to get a GUI password prompt.

`contrib/swiftbar/mihoctl.5m.sh` is a SwiftBar/xbar menu-bar plugin built on `status --json`. Its title is just the mihoctl icon (an M routed through three nodes; a ! badge when something needs attention, crossed out when the core is down), so it isn't folded away behind the notch of a full menu bar. Set `MIHOCTL_TITLE=node` to also show the selected node. `contrib/swiftbar/icon.py` (Pillow) draws the icon. The menu shows the selected node, warns about failed updates/applies, quota and expiry, and has menu items for refresh, diff, zashboard and logs. Install it with `ln -s "$PWD/contrib/swiftbar/mihoctl.5m.sh" <SwiftBar plugin folder>/`.

Logs go to `/Library/Logs/mihoctl/core.log` (root-only: `sudo tail -f /Library/Logs/mihoctl/core.log`) and `~/Library/Logs/mihoctl/refresh.log` on macOS; see [Platforms](#platforms) for Linux. State (node counts, `subscription-userinfo`, last apply) is stored in `<core_dir>/mihoctl-state.json`.

### Node history and restore

```sh
mihoctl history airport
mihoctl restore airport             # newest retained version different from the current cache
mihoctl restore airport ab12cd       # full SHA-256 or unique hexadecimal prefix
```

Each subscription retains **at most five distinct node-content versions, including the current cache**, after a successful update or restore. Identity hashes the actual persisted `proxies` array as canonical JSON: object-key order, YAML formatting, comments and wrapper metadata do not matter; node order, nested array order, scalar types, credentials and protocol options do. Repeated content reuses the same version and first-seen timestamp; revisiting a version makes it recent without adding a slot. Invalid/ambiguous hashes and unknown subscriptions fail. Existing caches appear as unrecorded legacy versions until the next update/restore safely seeds them.

Snapshots live under `<core_dir>/history/<subscription>/<sha256>.json` (directories `0700`, files `0600`), **contain node credentials**, and are not safe to share. The CLI lists metadata only; snapshot schema and content hashes are checked before restoring. The previous cache is durably saved before replacement; a history write failure keeps it unchanged. Pruning happens only after installation, always protecting the pre-operation recovery version on failure.

Restore recovers **one subscription's nodes, not a historical whole runtime configuration**. It uses today's template, overrides, other cached providers and controller settings. Retained updates are **not guaranteed healthy snapshots**. Restore performs no subscription HTTP request or subscription DNS lookup and does not run `update`; it is not promised to be network-free: `mihomo -t` may need geodata, and a running core's group health check intentionally uses the network. Its probe covers `health_check.group`, not specifically the restored provider; a zero-alive response fails even with HTTP 200. Override console output and detailed source/controller error text are withheld during restore to avoid exposing credentials.

The shared non-blocking process lock covers history/restore. Restore protects both the selected provider cache and installed config, then validates and uses payload hot reload plus the group probe. A build, validation, install, reload, probe, history-commit or state-save failure attempts cache, config, runtime (when changed), and state recovery independently and reports rollback failures. A live core requires an existing validated rollback config before changing nodes. If no core is running, success means **installed for next start, not reloaded or probed**. Failed candidate files are removed. Restore updates node counts and records `restored_at`/last-apply information without pretending to download a subscription or clearing its last download-failure diagnostics; traffic/expiry metadata remains from the last download.

### Remote control over Tailscale

The core's controller stays on loopback, so reaching the dashboard from another machine needs a
reverse proxy. With Tailscale installed and logged in, `mihoctl ui --tailscale` publishes the
dashboard on your tailnet and prints the URL to open:

```sh
mihoctl ui --tailscale                 # publish and print the tailnet URL
mihoctl ui --tailscale --unpublish     # remove the publication
```

Tailscale Serve terminates TLS on the node's MagicDNS name and forwards to the core on loopback,
so the dashboard is reachable only by tailnet members, with a real certificate and no port opened
on the local network. The published URL carries the controller secret, exactly like the local
`mihoctl ui` URL — treat it as a credential.

The dashboard is mounted under a path prefix (`/mihoctl` by default) so the node's HTTPS root stays
free for other services. `--path /` claims the root instead; `--port` picks another tailnet port.
Both can be set in `mihoctl.toml`:

```toml
[tailscale]
path = "/mihoctl"
port = 443
```

Publishing uses the `tailscale` CLI that Tailscale installs, because that is the supported
interface for Serve and it handles MagicDNS discovery, certificate provisioning and the
configuration update for you. On Linux the `tailscaled` socket only grants write access to root and
to the operator user (`tailscale set --operator=$USER`), so mihoctl retries under `sudo` when the
daemon refuses the change.

The publication survives reboots (Serve keeps running in the background) and is removed with
`mihoctl ui --tailscale --unpublish`. Other Serve configuration on the same node is left alone.

## Platforms

| | macOS | Linux | FreeBSD |
|---|---|---|---|
| `update` / `history` / `restore` / `build` / `diff` / `apply` / `refresh` / `status` / `ui` | yes | yes | yes |
| `ui --tailscale` | yes, needs the `tailscale` CLI; the App Store/system-extension build authenticates over a loopback TCP port | yes, needs the `tailscale` CLI; publishing as a non-root user needs `tailscale set --operator=$USER` or `sudo` | yes, needs the `tailscale` CLI |
| `service` | launchd | systemd | rc.d + cron |
| Default settings file | `~/.config/mihoctl/mihoctl.toml` | `$XDG_CONFIG_HOME/mihoctl/mihoctl.toml` | same as Linux |
| Default `core_dir` | `~/Library/Application Support/mihoctl/core` | `$XDG_DATA_HOME/mihoctl/core` | same as Linux |

**Linux (systemd).** The privilege split matches macOS:

| | Path |
|---|---|
| Core unit | `/etc/systemd/system/mihoctl-core.service` (root, umask 077) |
| mihomo copy | `/usr/local/libexec/mihoctl/mihomo`, which SELinux lets systemd execute; a binary in `$HOME` it would not |
| Core home (`-d`) | `/var/lib/mihoctl/core` |
| Core log | `/var/log/mihoctl/core.log` (root-only, so `sudo tail -f`; not the journal, which `wheel`/`adm` members can read) |
| Refresh | user timer `~/.config/systemd/user/mihoctl-refresh.timer`; logs: `journalctl --user -u mihoctl-refresh` |

The core keeps only the capabilities it needs: `CAP_NET_ADMIN CAP_NET_RAW CAP_NET_BIND_SERVICE` for TUN, auto-route and low ports, and `CAP_DAC_READ_SEARCH CAP_SYS_PTRACE` to read your 0600 config and support `PROCESS-NAME` rules. The refresh timer only runs while you are logged in unless you run `loginctl enable-linger`.

**FreeBSD (rc.d).** The privilege split matches macOS and Linux:

| | Path |
|---|---|
| Core service | `/usr/local/etc/rc.d/mihoctl_core`, enabled by `/etc/rc.conf.d/mihoctl_core` |
| mihomo copy | `/usr/local/libexec/mihoctl/mihomo` |
| Core home (`-d`) | `/var/db/mihoctl/core` |
| Core log | `/var/log/mihoctl/core.log` (root-only, so `sudo tail -f`) |
| Refresh | a marked block in your user crontab: `@<refresh_interval>` seconds after the previous run completes (FreeBSD cron's `@N` form, which does not overlap runs); log: `<core_dir>/refresh.log` |

Root steps use `sudo` (`pkg install sudo` on a minimal install). The rc.d script runs mihomo under `daemon(8) -r`, which restarts it if it exits, with the supervisor PID in `/var/run/mihoctl_core.pid`. `mihoctl status` finds the root core through `pgrep`, because that pidfile is root-only. Cron must be running for scheduled refreshes. Building needs libclang, because rquickjs generates its QuickJS bindings at build time there: `pkg install llvm`, and set `LIBCLANG_PATH=/usr/local/llvm<N>/lib` if it is not found. Check that your mihomo build supports TUN on FreeBSD before you rely on it.
