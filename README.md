# mihoctl

A headless manager for the [mihomo](https://github.com/MetaCubeX/mihomo) core on macOS. It replaces GUI clients (FlClash, Clash Verge) with a single settings file:

- **Subscriptions are node sources only.** mihoctl keeps each subscription's `proxies` and drops its groups, rules and DNS. Each subscription becomes an `inline` proxy-provider with a name prefix (`[name] `), its nodes embedded in the generated config. Your template owns groups, rules, DNS and TUN.
- **Overrides** run in the order of the `overrides` list, each one receiving the previous result. A directory entry expands to its `*.js`, `*.yaml` and `*.yml` files, sorted by file name: `10-a.js` runs before `99-b.js`. Directories are not searched recursively and dotfiles are skipped. Sorting is byte-wise, so zero-pad numeric prefixes (`05-`, not `5-`). Two kinds of override are supported:
  - YAML deep-merge using Clash Party operators: `key!` replaces the whole value, `+key` prepends to a list, `key+` appends, and `<key>` escapes a key that would otherwise read as an operator.
  - Clash Verge / Party style JS: `function main(config, profileName) { …; return config }`, sync or async. Scripts run in an embedded QuickJS with no filesystem, network, timers or process access, a 128 MiB heap, and the `script_timeout` limit.
- **Fail closed.** A throwing script, a config rejected by `mihomo -t`, or an unreachable subscription stops the run. The last good config and node cache stay in place; nothing silently falls back to an unmodified config.
- **Hot reload with rollback.** Each apply runs: `mihomo -t` → atomic swap → `PUT /configs?force=true` (config sent as `payload`) → a URL test of the `health_check.group` members. If no member answers, the previous config is restored and reloaded. Because the `tun:` block stays unchanged across reloads, the TUN device is not recreated.
- **launchd.** The core runs as a root LaunchDaemon (`io.mihoctl.core`) because TUN needs root. `mihoctl refresh` runs every `refresh_interval` as a user LaunchAgent (`io.mihoctl.refresh`).
- **Privilege split.** Nothing the root core executes or writes is user-writable, so code running as your user can't use it to get root. `service install` copies `mihomo` to `/Library/Application Support/mihoctl/bin/mihomo` (root-owned). The core's home (`-d`: cache, ui, geodata) is `/Library/Application Support/mihoctl/core`. Both run with umask 077. The core reads only `<core_dir>/config.yaml`, via `-f`. So `type: file` proxy/rule-providers are rejected at build time: use `type: inline` or `type: http`. `mihomo -t` validates with the installed copy, so validation and the core always run the same version. **Rerun `mihoctl service install` after upgrading mihomo**; until then both keep using the old copy.
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
| `mihoctl service install\|uninstall\|restart\|print` | Manage launchd units. `restart` is needed after editing `tun:`. `uninstall` also deletes `/Library/Application Support/mihoctl` |

Root steps (`service install/uninstall/restart`) run as a single `sudo` call. Set `SUDO_ASKPASS` to get a GUI password prompt.

Logs go to `/Library/Logs/mihoctl/core.log` (root-only: `sudo tail -f /Library/Logs/mihoctl/core.log`) and `~/Library/Logs/mihoctl/refresh.log`. State (node counts, `subscription-userinfo`, last apply) is stored in `<core_dir>/mihoctl-state.json`.
