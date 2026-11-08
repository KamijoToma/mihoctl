//! systemd units: the core as a root system service and the scheduled refresh as a user timer.

use super::{GEODATA, check_installable, output, sudo_sh};
use crate::settings::{self, Paths, Settings};
use anyhow::{Result, bail};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const MANAGER: &str = "systemd";
pub const CORE_NAME: &str = "mihoctl-core.service";
pub const REFRESH_NAME: &str = "mihoctl-refresh.timer";
const REFRESH_SERVICE: &str = "mihoctl-refresh.service";

/// Root-owned copy of mihomo. Under /usr rather than /var/lib so SELinux lets systemd execute it.
const BIN_DIR: &str = "/usr/local/libexec/mihoctl";
/// Root-owned state: the core's `-d` home `core/`.
const ROOT_DIR: &str = "/var/lib/mihoctl";
/// A root-only file rather than the journal, which `wheel`/`adm` members can read without sudo.
const CORE_LOG_DIR: &str = "/var/log/mihoctl";

/// The core stays root but keeps only what it needs: TUN, auto-route/auto-redirect (netlink,
/// nftables) and ports below 1024; reading the user's 0600 config and, for PROCESS-NAME rules,
/// other users' `/proc/<pid>`.
const CAPS: &str = "CAP_NET_ADMIN CAP_NET_RAW CAP_NET_BIND_SERVICE CAP_DAC_READ_SEARCH CAP_SYS_PTRACE";

fn core_unit_path() -> PathBuf {
    PathBuf::from(format!("/etc/systemd/system/{CORE_NAME}"))
}

fn core_bin() -> PathBuf {
    Path::new(BIN_DIR).join("mihomo")
}

fn core_home() -> PathBuf {
    Path::new(ROOT_DIR).join("core")
}

pub fn installed_mihomo() -> Option<PathBuf> {
    Some(core_bin())
}

fn user_unit_dir() -> Result<PathBuf> {
    Ok(settings::config_home()?.join("systemd/user"))
}

/// One `ExecStart=`/`Environment=` word: quoted, with systemd's specifier (`%`) and, for
/// command lines, variable (`$`) expansion escaped.
fn word(s: &str, command: bool) -> String {
    let mut s = s.replace('\\', "\\\\").replace('"', "\\\"").replace('%', "%%");
    if command {
        s = s.replace('$', "$$");
    }
    format!("\"{s}\"")
}

fn command(args: &[&str]) -> String {
    args.iter().map(|a| word(a, true)).collect::<Vec<_>>().join(" ")
}

/// `UMask` 077: the core's files (and its log, which may quote whatever `-f` points at) stay root-only.
fn core_unit(paths: &Paths) -> String {
    let lossy = |p: &Path| p.to_string_lossy().into_owned();
    let log = lossy(&Path::new(CORE_LOG_DIR).join("core.log"));
    let exec = command(&[&lossy(&core_bin()), "-d", &lossy(&core_home()), "-f", &lossy(&paths.config())]);
    format!(
        "[Unit]
Description=mihomo core (managed by mihoctl)
Wants=network-online.target
After=network-online.target

[Service]
ExecStart={exec}
Restart=always
RestartSec=2
UMask=0077
CapabilityBoundingSet={CAPS}
LimitNOFILE=1048576
StandardOutput=append:{log}
StandardError=append:{log}

[Install]
WantedBy=multi-user.target
"
    )
}

/// `PATH` is passed through so `mihomo` resolves the same way it did at install time.
fn refresh_service(exe: &Path, settings_file: &Path, path: &str) -> String {
    format!(
        "[Unit]
Description=mihoctl refresh: update subscriptions and hot-reload the core

[Service]
Type=oneshot
Environment={cfg}
Environment={path}
ExecStart={exec}
",
        cfg = word(&format!("MIHOCTL_CONFIG={}", settings_file.display()), false),
        path = word(&format!("PATH={path}"), false),
        exec = command(&[&exe.to_string_lossy(), "refresh"]),
    )
}

fn refresh_timer(secs: u64) -> String {
    format!(
        "[Unit]
Description=Run mihoctl refresh every {secs}s

[Timer]
OnActiveSec={secs}
OnUnitActiveSec={secs}

[Install]
WantedBy=timers.target
"
    )
}

/// `(install path, contents)` of every unit, for `service print`.
pub fn units(settings: &Settings, paths: &Paths) -> Result<Vec<(PathBuf, String)>> {
    let exe = std::env::current_exe()?.canonicalize()?;
    let cfg = paths.settings_file.canonicalize()?;
    let secs = settings::parse_duration(&settings.refresh_interval)?.as_secs();
    let path = std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".into());
    let dir = user_unit_dir()?;
    Ok(vec![
        (core_unit_path(), core_unit(paths)),
        (dir.join(REFRESH_SERVICE), refresh_service(&exe, &cfg, &path)),
        (dir.join(REFRESH_NAME), refresh_timer(secs)),
    ])
}

fn user_systemctl(args: &[&str]) -> Result<bool> {
    Ok(Command::new("systemctl").arg("--user").args(args).status()?.success())
}

fn active(user: bool, unit: &str) -> bool {
    let mut cmd = Command::new("systemctl");
    if user {
        cmd.arg("--user");
    }
    cmd.args(["is-active", "--quiet", unit]).status().is_ok_and(|s| s.success())
}

pub fn core_loaded() -> bool {
    active(false, CORE_NAME)
}

pub fn refresh_loaded() -> bool {
    active(true, REFRESH_NAME)
}

/// Root steps of `install`: `$1` unit name, `$2` ROOT_DIR, `$3` BIN_DIR, `$4` log dir, `$5` user
/// mihomo, `$6` unit path, `$7` user core_dir, `$8…` geodata names; the unit arrives on stdin.
/// Ownership and modes are reset recursively (`-R` never follows symlinks) in case the tree
/// predates this layout. Geodata is copied 0600 so a symlink planted in `core_dir` can't expose a
/// root-only file.
const INSTALL_SH: &str = r#"name=$1 root=$2 bin=$3 logs=$4 src=$5 unit=$6 seed=$7; shift 7
umask 077
install -d -o root -g root -m 755 "$root" "$root/core" "$bin" "$logs"
chown -R root:root "$root" "$bin" "$logs"
chmod -R go-w "$root" "$bin" "$logs"
[ -e "$logs/core.log" ] || : > "$logs/core.log"
chmod 600 "$logs/core.log"
install -o root -g root -m 755 "$src" "$bin/mihomo"
for f in "$@"; do
  if [ -f "$seed/$f" ] && [ ! -e "$root/core/$f" ]; then install -o root -g root -m 600 "$seed/$f" "$root/core/$f"; fi
done
cat > "$unit.tmp"
chmod 644 "$unit.tmp"
mv -f "$unit.tmp" "$unit"
systemctl daemon-reload
systemctl enable "$name"
systemctl restart "$name""#;

/// Copies `settings.mihomo` into the root-owned tree, so rerun this after upgrading mihomo.
pub fn install(settings: &Settings, paths: &Paths) -> Result<()> {
    check_installable(settings, paths)?;
    let units = units(settings, paths)?;

    let (unit_path, unit) = &units[0];
    let unit_path = unit_path.to_string_lossy().into_owned();
    let seed = paths.core_dir.to_string_lossy().into_owned();
    let mut args = vec![CORE_NAME, ROOT_DIR, BIN_DIR, CORE_LOG_DIR, settings.mihomo.as_str(), &unit_path, &seed];
    args.extend(GEODATA);
    sudo_sh(INSTALL_SH, &args, unit)?;

    for (path, contents) in &units[1..] {
        fs::create_dir_all(path.parent().unwrap())?;
        fs::write(path, contents)?;
    }
    if !(user_systemctl(&["daemon-reload"])?
        && user_systemctl(&["enable", REFRESH_NAME])?
        && user_systemctl(&["restart", REFRESH_NAME])?)
    {
        bail!("enabling the {REFRESH_NAME} user timer failed");
    }
    let user = output("id", &["-un"])?;
    if output("loginctl", &["show-user", &user, "-p", "Linger", "--value"]).is_ok_and(|v| v != "yes") {
        eprintln!("note: {REFRESH_NAME} only runs while you are logged in; `loginctl enable-linger` keeps it running");
    }
    Ok(())
}

pub fn uninstall() -> Result<()> {
    let dir = user_unit_dir()?;
    user_systemctl(&["disable", "--now", REFRESH_NAME]).ok();
    for name in [REFRESH_NAME, REFRESH_SERVICE] {
        fs::remove_file(dir.join(name)).ok();
    }
    user_systemctl(&["daemon-reload"]).ok();
    // ROOT_DIR and BIN_DIR hold only what install put there or the core regenerates.
    sudo_sh(
        r#"systemctl disable --now "$1" 2>/dev/null || true
rm -f "$2"
rm -rf "$3" "$4"
systemctl daemon-reload"#,
        &[CORE_NAME, &core_unit_path().to_string_lossy(), ROOT_DIR, BIN_DIR],
        "",
    )
}

/// Restart the core process (needed after changing `tun:`; plain reloads keep the TUN).
pub fn restart_core() -> Result<()> {
    sudo_sh(r#"systemctl restart "$1""#, &[CORE_NAME], "")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_and_escapes_words() {
        assert_eq!(command(&["/opt/my dir/mihomo", "-d", "/a\"b%c$d"]), r#""/opt/my dir/mihomo" "-d" "/a\"b%%c$$d""#);
        assert_eq!(word("PATH=/x$y:%h", false), r#""PATH=/x$y:%%h""#);
    }

    #[test]
    fn core_runs_only_root_owned_paths() {
        let unit = core_unit(&Paths::new("/home/u/m.toml".into(), "/home/u".into(), "/home/u/core".into()));
        assert!(unit.contains(
            r#"ExecStart="/usr/local/libexec/mihoctl/mihomo" "-d" "/var/lib/mihoctl/core" "-f" "/home/u/core/config.yaml""#
        ));
        assert!(unit.contains("StandardOutput=append:/var/log/mihoctl/core.log\n"));
        assert!(unit.contains("UMask=0077\n"));
        assert!(!unit.contains("User="));
        // The config, read via `-f`, is the only user path the root unit references.
        assert_eq!(unit.matches("/home/").count(), 1);
    }
}
