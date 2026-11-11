# Repository Guidelines

## Project Overview

`mihoctl` is a headless macOS, Linux and FreeBSD manager for the [`mihomo`](https://github.com/MetaCubeX/mihomo) proxy core. It replaces GUI clients (FlClash, Clash Verge) with a deterministic, fail-closed CLI driven by a single TOML file (`~/.config/mihoctl/mihoctl.toml`):
- **Subscriptions are node sources only**: only `proxies` are extracted, cached in `providers/<name>.yaml`, and inlined into the config as `type: inline` proxy-providers; upstream groups, rules, DNS, and inbounds are discarded.
- **Template + ordered overrides**: a base YAML template is combined with generated `proxy-providers`, transformed through ordered YAML deep-merge files and sandboxed QuickJS scripts (`function main(config, profileName)`), and locked down by enforcing loopback controller + secret settings.
- **Validated hot reload with rollback**: candidate configs are checked via `mihomo -t`, atomically swapped, hot-reloaded over REST (`PUT /configs?force=true` with the YAML as `payload`, without tearing down `tun:`), and probed via `GET /group/<group>/delay`; failed health checks automatically roll back to the previous config.

## Architecture & Data Flow

```
mihoctl.toml ──► settings::load() ──► Paths { config_dir, core_dir, providers_dir, state_file, lock_file }
                                        │
  ┌─────────────────────────────────────┴──────────────────────────────────────┐
  ▼                                                                            ▼
[update] subscription::update_all()                                  [build / apply] pipeline::build_checked()
  ├─ std::thread::scope parallel HTTP GET (via core mixed-port / direct)       ├─ Load base template YAML
  ├─ Redact subscription URLs on ureq errors                                   ├─ Inject inline proxy-providers (entries only, no nodes)
  ├─ extract_proxies(): drop port <= 1 banners & disabled IPv6 literals        ├─ resolve_overrides(): expand dirs (byte-sorted) + files
  └─ state::atomic_write(0600) -> <core_dir>/providers/<name>.yaml             │   ├─ .yaml/.yml -> merge::deep_merge()
                                                                               │   └─ .js        -> script::run() (QuickJS sandbox)
                                                                               ├─ fill_nodes(): payload <- cached providers/*.yaml (after overrides)
                                                                               ├─ reject_local_files(): no `type: file` proxy/rule-providers
                                                                               ├─ enforce(): loopback controller, secret, external-ui, strip TLS/unix/pipe
                                                                               ├─ state::atomic_write(0600) -> config.candidate.yaml
                                                                               └─ core::test_config() -> `<runtime mihomo> -t -d <core_dir> -f <candidate>`
                                                                                              │
                                                                                              ▼
                                                                               pipeline::apply()
                                                                               ├─ If unchanged: done (nodes are inline)
                                                                               ├─ Backup current config.yaml -> config.last-good.yaml
                                                                               ├─ Atomic rename candidate -> config.yaml
                                                                               └─ If core running: PUT /configs?force=true {payload} + GET /group/<group>/delay
                                                                                   └─ On error: restore old bytes + reload rollback
```

- **Read-only views**: `status` (`src/status.rs`) gathers one `Status` snapshot and renders it as text or `--json` (for scripts and `contrib/swiftbar/`); it never carries subscription URLs or the controller secret. `diff` runs `build_checked`, then `pipeline::diff` masks both sides with `mask_for_diff` (secret → `<masked>`, node credentials reduced to names) and shells out to `diff -u`.

- **Privilege split (`src/service/`)**: the backend is picked by `cfg(target_os)`: `launchd.rs` (macOS), `systemd.rs` (Linux: root `mihoctl-core.service` with a root-owned `/usr/local/libexec/mihoctl/mihomo`, `/var/lib/mihoctl/core` home and `/var/log/mihoctl/core.log`, plus the user `mihoctl-refresh.timer`), `rc.rs` (FreeBSD: root `/usr/local/etc/rc.d/mihoctl_core` running mihomo under `daemon(8) -r` with `/usr/local/libexec/mihoctl/mihomo`, `/var/db/mihoctl/core` and `/var/log/mihoctl/core.log`, plus a user crontab `@<refresh_interval>` entry), `unsupported.rs` elsewhere. Shared invariants and helpers (`check_installable`, `sudo_sh`, `runtime_mihomo`) live in `mod.rs`. On macOS `io.mihoctl.core` runs `mihomo` as a root `LaunchDaemon` (`/Library/LaunchDaemons/io.mihoctl.core.plist`) because macOS TUN requires root. `io.mihoctl.refresh` runs `mihoctl refresh` as an unprivileged user `LaunchAgent` (`~/Library/LaunchAgents/io.mihoctl.refresh.plist`), communicating with the core exclusively over the loopback REST controller.
- **Root core never touches user-writable paths**: otherwise any process running as the user gets root without a password. `service install` copies `settings.mihomo` to root-owned `/Library/Application Support/mihoctl/bin/mihomo`; the core runs with `-d /Library/Application Support/mihoctl/core` (root-owned home: `cache.db`, `ui/`, geodata), `Umask` 077, and logs to root-only `/Library/Logs/mihoctl/core.log`. Its only user-owned input is `<core_dir>/config.yaml`, read via `-f`. Consequences: mihomo refuses `PUT /configs` with a `path` outside its home, so reloads send `payload`; config files can't reference `type: file` providers; `mihomo -t` uses `service::runtime_mihomo()` (the installed copy when present) so validation matches the running version. Never point the plist at a user-owned binary, directory or log, and never stage root-bound files in temp dirs (`sudo_sh` pipes them over stdin).

## Key Directories

- `src/` — Rust crate source (flat module hierarchy, one concern per file, inline `#[cfg(test)] mod tests`).
- `contrib/swiftbar/` — SwiftBar/xbar menu-bar plugin (`mihoctl.5m.sh`, bash + `/usr/bin/jq`) built on `status --json`.
- `.github/workflows/` — GitHub Actions CI (`ci.yml`) and tag-driven multi-arch release (`release.yml`).
- `~/.config/mihoctl/` *(runtime default)* — User configuration directory (`mihoctl.toml`, `template.yaml`, `overrides/`).
- `~/Library/Application Support/mihoctl/core/` *(runtime default `core_dir`)* — user-owned workspace holding `config.yaml`, `config.candidate.yaml`, `config.last-good.yaml`, `providers/*.yaml` (node cache), `mihoctl-state.json`, `mihoctl.lock`, and geodata files for `mihomo -t`.
- `/Library/Application Support/mihoctl/` *(root-owned, created by `service install`)* — `bin/mihomo` (the copy the core runs) and `core/` (the core's `-d` home: `cache.db`, `ui/`, geodata).
- `/Library/Logs/mihoctl/core.log` *(root-only)* and `~/Library/Logs/mihoctl/refresh.log` — written by the launchd units.
- `/var/db/mihoctl/` and `/usr/local/libexec/mihoctl/` *(root-owned, created by `service install` on FreeBSD)* — the core's `-d` home and the mihomo copy; root-only `/var/log/mihoctl/core.log` and user `<core_dir>/refresh.log`.

## Development Commands

```sh
# Run unit tests (26 inline tests across subscription, pipeline, script, merge, service, status)
cargo test --locked
cargo test --locked <test_name_filter>

# Lint (CI enforces zero warnings across all targets)
cargo clippy --locked --all-targets -- -D warnings

# Build release binary (Apple Silicon & Intel macOS targets)
cargo build --release --locked
cargo build --release --locked --target aarch64-apple-darwin
cargo build --release --locked --target x86_64-apple-darwin

# Run CLI locally against a custom config
cargo run -- --config /path/to/mihoctl.toml build
cargo run -- --config /path/to/mihoctl.toml status --json
cargo run -- --config /path/to/mihoctl.toml diff

# Exercise the menu-bar plugin against a dev build
MIHOCTL="$PWD/target/debug/mihoctl" contrib/swiftbar/mihoctl.5m.sh
```

## Code Conventions & Common Patterns

- **Formatting & Style**:
  - Rust 2024 edition. Compact single-line blocks and chains up to ~120 columns are used intentionally across `src/*.rs`. Do **not** run default 100-column `cargo fmt` across unrelated files.
- **Fail-Closed Design**:
  - Never silently fall back to an unmodified config or overwrite a valid cache with broken data.
  - Subscription fetch failure keeps the existing `providers/<name>.yaml` and records `last_error` in `mihoctl-state.json`; `update` fails only if all targeted subscriptions fail (`src/main.rs:192-194`), while `refresh` exits non-zero after applying cached nodes if any subscription failed (`src/main.rs:127-129`).
- **Error Handling & Secret Redaction**:
  - All fallible functions return `anyhow::Result<T>`, chaining `.context(...)` / `.with_context(...)` at I/O and parsing boundaries and printing `{err:#}` in `main`.
  - **Mandatory URL redaction**: `ureq` errors include the request URL, which contains airport subscription tokens. Always strip the URL via `redact(&e.to_string(), url)` (`src/subscription.rs:96,107,111-113`) before propagating errors or storing `last_error` in state.
  - **Display-safe output**: `status --json` and `diff` are meant to be pasted or shown in a menu bar. Never add subscription URLs or the secret to `status::Status`; route anything `diff` prints through `mask_for_diff`. Anything that needs the secret (`mihoctl ui`) is computed on demand, never cached in plugin output.
- **Security & Controller Invariants**:
  - `settings::validate` (`src/settings.rs:181-199`) enforces `#[serde(deny_unknown_fields)]`, non-empty `secret`, and `[A-Za-z0-9_-]+` subscription names (preventing path traversal in `providers/<name>.yaml`).
  - `pipeline::enforce` (`src/pipeline.rs:17-30`) runs **after** all templates and overrides. It rejects non-loopback `controller` hosts (`127.0.0.1`, `[::1]`, `localhost` only), overwrites `external-controller`, `secret`, `external-ui`, and `external-ui-url`, and deletes `external-controller-tls`, `external-controller-unix`, and `external-controller-pipe`.
- **Concurrency & State Management**:
  - **Process locking**: mutating commands (`update`, `apply`, `refresh`), plus `diff` because it shares `config.candidate.yaml` with `apply`, acquire an exclusive non-blocking `File::try_lock()` on `<core_dir>/mihoctl.lock` (`src/state.rs:77-84`) so scheduled refreshes and manual runs never interleave.
  - **Atomic `0600` writes**: `state::atomic_write` (`src/state.rs:12-29`) writes to `.{filename}.tmp` in the same directory with mode `0600`, calls `sync_all()`, and `rename()`s over the target. Always use `atomic_write` for configs, provider caches, and state files.
  - **Parallel HTTP**: subscription downloads use `std::thread::scope` (`src/subscription.rs:35-46`) with synchronous `ureq::Agent` instances (no `tokio` or async Rust runtime).
- **Override Conventions**:
  - **Directory resolution (`src/pipeline.rs:114-135`)**: `overrides` entries run in list order; directories expand non-recursively to non-hidden `.js`, `.yaml`, `.yml` files sorted byte-wise by filename (`05-a.js` before `10-b.yaml`).
  - **YAML Merge Operators (`src/merge.rs:11-61`)**: maps merge recursively; scalars/arrays replace unless suffixed/prefixed with operators: `key!` replaces a map wholesale, `+key` prepends a list, `key+` appends a list, and `<key>` escapes literal keys starting/ending with `+` or `!` (e.g., `<+.example.com>`).
  - **QuickJS Sandbox (`src/script.rs:26-68`)**: scripts run in an isolated `rquickjs::Runtime` with a 128 MiB heap limit (`MEMORY_LIMIT`), wall-clock interrupt handler (`script_timeout`), and only ECMAScript built-ins + `console`. Both sync and `async function main(config, profileName)` are supported (pending jobs drained via `ctx.execute_pending_job()`).

## Important Files

- `src/main.rs` — CLI definition (`clap` derive) and top-level orchestration (`update`, `build`, `diff`, `apply`, `refresh`, `status`, `ui`, `service`).
- `src/status.rs` — `Status` snapshot (`gather`), `CoreState` enum, `subscription-userinfo` parsing (`parse_userinfo`, saturating), and text/JSON renderers (`print_text`, `print_json`).
- `src/settings.rs` — `mihoctl.toml` schema (`Settings`, `HealthCheck`, `Subscription`, `Via`), duration parser, and `Paths` resolution.
- `src/pipeline.rs` — Config assembly (`build`, `build_checked`), override resolution (`resolve_overrides`), node injection (`fill_nodes`), `type: file` rejection (`reject_local_files`), loopback/secret enforcement (`enforce`), masked unified diff (`mask_for_diff`, `diff`), and payload hot-reload + rollback (`apply`).
- `src/subscription.rs` — Concurrent subscription downloader (`update_all`), `Via` routing (`Auto`, `Proxy`, `Direct`), URL redaction, proxy node filtering (`extract_proxies`), and inline `proxy-providers` generation (`providers` → entries + node lists).
- `src/merge.rs` — Recursive YAML object merger supporting `key!`, `+key`, `key+`, and `<key>` escaping.
- `src/script.rs` — Embedded QuickJS runtime, `console` forwarding prelude, promise draining, and sandbox limits.
- `src/core.rs` — `mihomo -t` subprocess validator (`test_config`), REST `Controller` client (`ureq`), and RFC 3986 percent-encoder (`encode`).
- `src/service/` — `mod.rs`: shared install checks, `runtime_mihomo`, single-invocation `sudo_sh` (stdin-fed, `SUDO_ASKPASS` aware); `launchd.rs` / `systemd.rs` / `rc.rs`: unit/script generators, root-owned install layout (`core_bin`, `core_home`, `INSTALL_SH`); `unsupported.rs`: fallback for other OSes.
- `src/state.rs` — `0600` atomic file writer (`atomic_write`), `mihoctl-state.json` persistence (`State`), and advisory file lock (`lock`).
- `contrib/swiftbar/mihoctl.5m.sh` — menu-bar plugin. In its `jq` program, `//` treats `false` like `null` (use `!= false` / `show` for booleans), and every interpolated field must go through `clean`, because SwiftBar splits each line on `|` (real node names contain it). Keep stderr out of the parsed JSON.

## Runtime/Tooling Preferences

- **Toolchain & Target Platform**: stable Rust (`edition = "2024"`), targeting macOS (`aarch64-apple-darwin`, `x86_64-apple-darwin`), Linux (`*-unknown-linux-musl` releases) and FreeBSD; FreeBSD is CI-checked and needs libclang (`pkg install llvm`) because rquickjs generates bindings there.
- **Dependency Constraints (`Cargo.toml`)**:
  - Keep `serde_json` feature `"preserve_order"` enabled so YAML/JSON config keys preserve template/override insertion order.
  - Keep synchronous HTTP (`ureq`) and scoped threads; do not introduce `tokio`/`async-std` for host I/O.
  - Keep `Cargo.lock` committed and pass `--locked` in CI and release builds.
- **Sensitive Local Artifacts (`.gitignore`)**: never commit `mihoctl.toml`, `mihoctl-state.json`, `mihoctl.lock`, `config.yaml`, `config.*.yaml`, or `providers/`. Use RFC 3849 (`2001:db8::/32`) and RFC 2606 (`.example`, `.example.com`) addresses in tests.

## Testing & QA

- **Framework**: standard Rust `#[cfg(test)] mod tests` colocated at the bottom of `src/subscription.rs`, `src/pipeline.rs`, `src/script.rs`, `src/merge.rs`, `src/service/{launchd,systemd,rc}.rs` (each compiled only on its OS), and `src/status.rs`. Build test `Paths` with `Paths::new(settings_file, config_dir, core_dir)`.
- **What to Test**:
  - **Boundary & security invariants**: controller loopback enforcement (`enforce_overrides_template_controller_settings`, `enforce_rejects_non_loopback_controller`), token URL redaction (`redact_strips_token_url`), placeholder port (`<= 1`) and IPv6 literal filtering (`extract_proxies`), root unit referencing only root-owned paths plus the `-f` config (`core_runs_only_root_owned_paths`), nodes filled after overrides (`nodes_fill_inline_providers_after_overrides`), `type: file` rejection (`local_file_providers_are_rejected`), diff masking of secret and node credentials (`diff_masking_keeps_node_names_only`), saturating userinfo parsing (`absurd_userinfo_values_saturate_instead_of_overflowing`), QuickJS sandbox isolation (`sandbox_exposes_no_host_capabilities`), infinite-loop timeout & OOM interruption (`failures_are_errors_not_fallbacks`), and YAML merge operator edge cases.
  - **Filesystem tests**: when creating temporary directories in unit tests (e.g., `src/pipeline.rs:296`), scope paths under `std::env::temp_dir()` with unique suffixes and clean up before asserting.
- **Verification Gate**: run `cargo clippy --locked --all-targets -- -D warnings && cargo test --locked` before committing.
