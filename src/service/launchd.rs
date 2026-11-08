//! launchd units: the core as a root LaunchDaemon and the scheduled refresh as a user LaunchAgent.

use super::{GEODATA, check_installable, output, sudo_sh};
use crate::settings::{self, Paths, Settings};
use anyhow::{Result, bail};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const MANAGER: &str = "launchd";
pub const CORE_NAME: &str = "io.mihoctl.core";
pub const REFRESH_NAME: &str = "io.mihoctl.refresh";

/// Root-owned install root: `bin/mihomo` and the core's `-d` home `core/`.
const ROOT_DIR: &str = "/Library/Application Support/mihoctl";
const CORE_LOG_DIR: &str = "/Library/Logs/mihoctl";

fn core_plist_path() -> PathBuf {
    PathBuf::from(format!("/Library/LaunchDaemons/{CORE_NAME}.plist"))
}

fn core_bin() -> PathBuf {
    Path::new(ROOT_DIR).join("bin/mihomo")
}

fn core_home() -> PathBuf {
    Path::new(ROOT_DIR).join("core")
}

pub fn installed_mihomo() -> Option<PathBuf> {
    Some(core_bin())
}

fn refresh_plist_path() -> Result<PathBuf> {
    Ok(settings::home()?.join(format!("Library/LaunchAgents/{REFRESH_NAME}.plist")))
}

fn log_dir() -> Result<PathBuf> {
    Ok(settings::home()?.join("Library/Logs/mihoctl"))
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn plist(label: &str, args: &[String], extra: &str) -> String {
    let args: String = args.iter().map(|a| format!("\n    <string>{}</string>", esc(a))).collect();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>
  <key>ProgramArguments</key>
  <array>{args}
  </array>
{extra}</dict>
</plist>
"#
    )
}

/// `Umask` 077: the core's files (and its log, which may quote whatever `-f` points at) stay root-only.
fn core_plist(paths: &Paths) -> String {
    let log = esc(&Path::new(CORE_LOG_DIR).join("core.log").to_string_lossy());
    let extra = format!(
        "  <key>RunAtLoad</key>\n  <true/>\n  <key>KeepAlive</key>\n  <true/>\n  <key>ProcessType</key>\n  <string>Interactive</string>\n  <key>Umask</key>\n  <integer>63</integer>\n  <key>StandardOutPath</key>\n  <string>{log}</string>\n  <key>StandardErrorPath</key>\n  <string>{log}</string>\n"
    );
    let lossy = |p: &Path| p.to_string_lossy().into_owned();
    let args = [lossy(&core_bin()), "-d".into(), lossy(&core_home()), "-f".into(), lossy(&paths.config())];
    plist(CORE_NAME, &args, &extra)
}

fn refresh_plist(settings: &Settings, paths: &Paths) -> Result<String> {
    let exe = std::env::current_exe()?.canonicalize()?;
    let secs = settings::parse_duration(&settings.refresh_interval)?.as_secs();
    let log = esc(&log_dir()?.join("refresh.log").to_string_lossy());
    let cfg = esc(&paths.settings_file.canonicalize()?.to_string_lossy());
    let extra = format!(
        "  <key>StartInterval</key>\n  <integer>{secs}</integer>\n  <key>EnvironmentVariables</key>\n  <dict>\n    <key>MIHOCTL_CONFIG</key>\n    <string>{cfg}</string>\n  </dict>\n  <key>StandardOutPath</key>\n  <string>{log}</string>\n  <key>StandardErrorPath</key>\n  <string>{log}</string>\n"
    );
    Ok(plist(REFRESH_NAME, &[exe.to_string_lossy().into_owned(), "refresh".into()], &extra))
}

/// `(install path, contents)` of every unit, for `service print`.
pub fn units(settings: &Settings, paths: &Paths) -> Result<Vec<(PathBuf, String)>> {
    Ok(vec![(core_plist_path(), core_plist(paths)), (refresh_plist_path()?, refresh_plist(settings, paths)?)])
}

fn launchctl(args: &[&str]) -> Result<bool> {
    Ok(Command::new("launchctl").args(args).output()?.status.success())
}

fn uid() -> Result<String> {
    output("id", &["-u"])
}

pub fn core_loaded() -> bool {
    launchctl(&["print", &format!("system/{CORE_NAME}")]).unwrap_or(false)
}

pub fn refresh_loaded() -> bool {
    uid().and_then(|u| launchctl(&["print", &format!("gui/{u}/{REFRESH_NAME}")])).unwrap_or(false)
}

/// Root steps of `install`: `$1` label, `$2` ROOT_DIR, `$3` log dir, `$4` user mihomo, `$5` plist
/// path, `$6` user core_dir, `$7…` geodata names; the plist arrives on stdin. Ownership and modes
/// are reset recursively (`-R` never follows symlinks) in case the tree predates this layout.
/// Geodata is copied 0600 so a symlink planted in `core_dir` can't expose a root-only file. The log
/// is created 0600 here rather than trusting launchd to apply `Umask` when it opens stdio.
const INSTALL_SH: &str = r#"label=$1 root=$2 logs=$3 src=$4 plist=$5 seed=$6; shift 6
umask 077
launchctl bootout "system/$label" 2>/dev/null || true
install -d -o root -g wheel -m 755 "$root" "$root/bin" "$root/core" "$logs"
chown -R root:wheel "$root" "$logs"
chmod -R go-w "$root" "$logs"
[ -e "$logs/core.log" ] || : > "$logs/core.log"
chmod 600 "$logs/core.log"
install -o root -g wheel -m 755 "$src" "$root/bin/mihomo"
for f in "$@"; do
  if [ -f "$seed/$f" ] && [ ! -e "$root/core/$f" ]; then install -o root -g wheel -m 600 "$seed/$f" "$root/core/$f"; fi
done
cat > "$plist.tmp"
chown root:wheel "$plist.tmp"
chmod 644 "$plist.tmp"
mv -f "$plist.tmp" "$plist"
launchctl bootstrap system "$plist""#;

/// Copies `settings.mihomo` into the root-owned tree, so rerun this after upgrading mihomo.
pub fn install(settings: &Settings, paths: &Paths) -> Result<()> {
    check_installable(settings, paths)?;
    fs::create_dir_all(log_dir()?)?;

    let plist_path = core_plist_path().to_string_lossy().into_owned();
    let seed = paths.core_dir.to_string_lossy().into_owned();
    let mut args = vec![CORE_NAME, ROOT_DIR, CORE_LOG_DIR, settings.mihomo.as_str(), &plist_path, &seed];
    args.extend(GEODATA);
    sudo_sh(INSTALL_SH, &args, &core_plist(paths))?;

    let agent = refresh_plist_path()?;
    fs::create_dir_all(agent.parent().unwrap())?;
    fs::write(&agent, refresh_plist(settings, paths)?)?;
    let domain = format!("gui/{}", uid()?);
    if refresh_loaded() {
        launchctl(&["bootout", &format!("{domain}/{REFRESH_NAME}")])?;
    }
    if !launchctl(&["bootstrap", &domain, &agent.to_string_lossy()])? {
        bail!("launchctl bootstrap {domain} {} failed", agent.display());
    }
    Ok(())
}

pub fn uninstall() -> Result<()> {
    let agent = refresh_plist_path()?;
    if refresh_loaded() {
        launchctl(&["bootout", &format!("gui/{}/{REFRESH_NAME}", uid()?)])?;
    }
    fs::remove_file(&agent).ok();
    // ROOT_DIR holds only what install put there or the core regenerates (binary, cache, ui, geodata).
    sudo_sh(
        r#"launchctl bootout "system/$1" 2>/dev/null || true
rm -f "$2"
rm -rf "$3""#,
        &[CORE_NAME, &core_plist_path().to_string_lossy(), ROOT_DIR],
        "",
    )
}

/// Restart the core process (needed after changing `tun:`; plain reloads keep the TUN).
pub fn restart_core() -> Result<()> {
    sudo_sh(r#"launchctl kickstart -k "system/$1""#, &[CORE_NAME], "")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_runs_only_root_owned_paths() {
        let p = core_plist(&Paths::new("/Users/u/m.toml".into(), "/Users/u".into(), "/Users/u/core".into()));
        let strings: Vec<&str> = p.lines().filter_map(|l| l.trim().strip_prefix("<string>")?.strip_suffix("</string>")).collect();
        assert_eq!(
            strings[1..6],
            ["/Library/Application Support/mihoctl/bin/mihomo", "-d", "/Library/Application Support/mihoctl/core", "-f", "/Users/u/core/config.yaml"]
        );
        assert!(strings.contains(&"/Library/Logs/mihoctl/core.log"));
        assert!(p.contains("<key>Umask</key>\n  <integer>63</integer>"));
        // The config, read via `-f`, is the only user path the root unit references.
        assert_eq!(strings.iter().filter(|s| s.starts_with("/Users/")).count(), 1);
    }
}
