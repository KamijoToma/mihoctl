# mihoctl

A headless manager for the [mihomo](https://github.com/MetaCubeX/mihomo) core on macOS. It replaces GUI clients (FlClash, Clash Verge) with a single settings file:

- **Subscriptions are node sources only.** mihoctl keeps each subscription's `proxies` and drops its groups, rules and DNS. Each subscription becomes a `file` proxy-provider with a name prefix (`[name] `). Your template owns groups, rules, DNS and TUN.
- **Overrides** run in the order of the `overrides` list, each one receiving the previous result. A directory entry expands to its `*.js`, `*.yaml` and `*.yml` files, sorted by file name: `10-a.js` runs before `99-b.js`. Directories are not searched recursively and dotfiles are skipped. Sorting is byte-wise, so zero-pad numeric prefixes (`05-`, not `5-`). Two kinds of override are supported:
  - YAML deep-merge using Clash Party operators: `key!` replaces the whole value, `+key` prepends to a list, `key+` appends, and `<key>` escapes a key that would otherwise read as an operator.
  - Clash Verge / Party style JS: `function main(config, profileName) { …; return config }`, sync or async. Scripts run in an embedded QuickJS with no filesystem, network, timers or process access, a 128 MiB heap, and the `script_timeout` limit.
- **Fail closed.** A throwing script, a config rejected by `mihomo -t`, or an unreachable subscription stops the run. The last good config and node cache stay in place; nothing silently falls back to an unmodified config.
- **Hot reload with rollback.** Each apply runs: `mihomo -t` → atomic swap → `PUT /configs?force=true` → a URL test of the `health_check.group` members. If no member answers, the previous config is restored and reloaded. Because the `tun:` block stays unchanged across reloads, the TUN device is not recreated.
- **launchd.** The core runs as a root LaunchDaemon (`io.mihoctl.core`) because TUN needs root. `mihoctl refresh` runs every `refresh_interval` as a user LaunchAgent (`io.mihoctl.refresh`).
- **zashboard** is served by the core from `<core_dir>/ui` and is reachable only on loopback, behind the controller secret.

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

The core needs the geodata files (`GEOSITE.dat`, `geoip.metadb`, `ASN.mmdb`) in `core_dir`. It downloads them on first start if they are missing. If GitHub is blocked, copy them from an existing client instead.

## mihoctl.toml

```toml
template = "template.yaml"                       # relative to this file
overrides = ["overrides"]                        # dir: *.js/*.yaml sorted by name; files and dirs can be mixed
secret = "long-random-string"                    # required; controller auth
# controller = "127.0.0.1:9090"                  # must be loopback
# core_dir = "~/Library/Application Support/mihoctl/core"
# mihomo = "/opt/homebrew/opt/mihomo/bin/mihomo"
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
```

Whatever the template or overrides say, mihoctl itself always sets `external-controller`, `secret`, `external-ui` and `external-ui-url`. This keeps the controller on loopback with a secret.

Subscriptions must return mihomo YAML when requested with `user_agent`. Placeholder nodes that point at port 0 or 1 are dropped. Responses in any other format are rejected, and the previous cache is kept.

## Commands

| Command | Effect |
|---|---|
| `mihoctl update [name…] [--proxy URL]` | Download subscriptions concurrently and replace each cache file that validates |
| `mihoctl build [-o file]` | Generate the config and run it through `mihomo -t`; print it or write it to a file |
| `mihoctl apply` | Build, validate, install and hot-reload, then health-check; roll back on failure |
| `mihoctl refresh` | `update` + `apply` (the scheduled job) |
| `mihoctl status` | launchd state, core version/mode/TUN, per-subscription node counts, traffic/expiry, last errors |
| `mihoctl ui [--update]` | zashboard URL pre-filled with controller and secret |
| `mihoctl service install\|uninstall\|restart\|print` | Manage launchd units. `restart` is needed after editing `tun:` |

Root steps (`service install/uninstall/restart`) run as a single `sudo` call. Set `SUDO_ASKPASS` to get a GUI password prompt.

Logs go to `~/Library/Logs/mihoctl/{core,refresh}.log`. State (node counts, `subscription-userinfo`, last apply) is stored in `<core_dir>/mihoctl-state.json`.
