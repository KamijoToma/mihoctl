//! launchd units: the core as a root LaunchDaemon (TUN needs root) and the scheduled refresh
//! as a user LaunchAgent (it only talks to the controller, so it needs no privileges).
//!
//! Nothing the root core executes or writes may be user-writable, or any process running as the
//! user gets root without a password: it runs a root-owned copy of mihomo, with a root-owned `-d`
//! home (cache.db, ui/, geodata) and root-only logs. The only user file it touches is the config,
//! read via `-f`; that grants nothing the controller secret doesn't already, because mihomo
//! confines file paths in a config to its home.

use crate::settings::{self, Paths, Settings};
use crate::{core, pipeline};
use anyhow::{Context, Result, bail};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const CORE_LABEL: &str = "io.mihoctl.core";
pub const REFRESH_LABEL: &str = "io.mihoctl.refresh";

/// Root-owned install root: `bin/mihomo` and the core's `-d` home `core/`.
const ROOT_DIR: &str = "/Library/Application Support/mihoctl";
const CORE_LOG_DIR: &str = "/Library/Logs/mihoctl";
/// Seeded from `core_dir` on install so the core can start where GitHub is blocked.
const GEODATA: [&str; 6] = ["GEOSITE.dat", "geoip.metadb", "ASN.mmdb", "geosite.dat", "geoip.dat", "Country.mmdb"];

fn core_plist_path() -> PathBuf {
    PathBuf::from(format!("/Library/LaunchDaemons/{CORE_LABEL}.plist"))
}

pub fn core_bin() -> PathBuf {
    Path::new(ROOT_DIR).join("bin/mihomo")
}

pub fn core_home() -> PathBuf {
    Path::new(ROOT_DIR).join("core")
}

/// The binary the core actually runs: the installed copy, which lags `settings.mihomo` until the
/// next `service install`. Validate with it so `mihomo -t` and the core never disagree.
pub fn runtime_mihomo(settings: &Settings) -> String {
    let bin = core_bin();
    if bin.exists() { bin.to_string_lossy().into_owned() } else { settings.mihomo.clone() }
}

fn refresh_plist_path() -> Result<PathBuf> {
    Ok(settings::home()?.join(format!("Library/LaunchAgents/{REFRESH_LABEL}.plist")))
}

pub fn log_dir() -> Result<PathBuf> {
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
pub fn core_plist(paths: &Paths) -> String {
    let log = esc(&Path::new(CORE_LOG_DIR).join("core.log").to_string_lossy());
    let extra = format!(
        "  <key>RunAtLoad</key>\n  <true/>\n  <key>KeepAlive</key>\n  <true/>\n  <key>ProcessType</key>\n  <string>Interactive</string>\n  <key>Umask</key>\n  <integer>63</integer>\n  <key>StandardOutPath</key>\n  <string>{log}</string>\n  <key>StandardErrorPath</key>\n  <string>{log}</string>\n"
    );
    let lossy = |p: &Path| p.to_string_lossy().into_owned();
    let args = [lossy(&core_bin()), "-d".into(), lossy(&core_home()), "-f".into(), lossy(&paths.config())];
    plist(CORE_LABEL, &args, &extra)
}

pub fn refresh_plist(settings: &Settings, paths: &Paths) -> Result<String> {
    let exe = std::env::current_exe()?.canonicalize()?;
    let secs = settings::parse_duration(&settings.refresh_interval)?.as_secs();
    let log = esc(&log_dir()?.join("refresh.log").to_string_lossy());
    let cfg = esc(&paths.settings_file.canonicalize()?.to_string_lossy());
    let extra = format!(
        "  <key>StartInterval</key>\n  <integer>{secs}</integer>\n  <key>EnvironmentVariables</key>\n  <dict>\n    <key>MIHOCTL_CONFIG</key>\n    <string>{cfg}</string>\n  </dict>\n  <key>StandardOutPath</key>\n  <string>{log}</string>\n  <key>StandardErrorPath</key>\n  <string>{log}</string>\n"
    );
    Ok(plist(REFRESH_LABEL, &[exe.to_string_lossy().into_owned(), "refresh".into()], &extra))
}

/// Run a shell script as root in one `sudo` call (one password prompt). `$1…` are `args`; `stdin`
/// is piped to the script (sudo reads the password from the tty, not stdin), so root-bound files
/// never pass through a user-writable temp file. Uses `sudo -A` when `SUDO_ASKPASS` is set.
fn sudo_sh(script: &str, args: &[&str], stdin: &str) -> Result<()> {
    let mut cmd = Command::new("sudo");
    if std::env::var_os("SUDO_ASKPASS").is_some() {
        cmd.arg("-A");
    }
    let mut child = cmd
        .args(["/bin/sh", "-euc", script, "mihoctl"])
        .args(args)
        .stdin(Stdio::piped())
        .spawn()
        .context("running sudo")?;
    // Wait even if the write fails: sudo exiting early (wrong password, Ctrl-C) breaks the pipe,
    // and its exit status is the error worth reporting.
    let written = child.stdin.take().context("sudo stdin")?.write_all(stdin.as_bytes());
    let status = child.wait().context("running sudo")?;
    if !status.success() {
        bail!("privileged step failed ({status})");
    }
    written.context("writing to sudo")
}

fn launchctl(args: &[&str]) -> Result<bool> {
    Ok(Command::new("launchctl").args(args).output()?.status.success())
}

fn uid() -> Result<String> {
    let out = Command::new("id").arg("-u").output()?;
    Ok(String::from_utf8(out.stdout)?.trim().to_string())
}

pub fn is_loaded_system(label: &str) -> bool {
    launchctl(&["print", &format!("system/{label}")]).unwrap_or(false)
}

pub fn is_loaded_user(label: &str) -> bool {
    uid().and_then(|u| launchctl(&["print", &format!("gui/{u}/{label}")])).unwrap_or(false)
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
    if !paths.config().exists() {
        bail!("{} does not exist; run `mihoctl apply` first", paths.config().display());
    }
    if !Path::new(&settings.mihomo).exists() {
        bail!("mihomo binary {} not found", settings.mihomo);
    }
    // The core starts straight from config.yaml: refuse one an older mihoctl built with `file`
    // providers, or one the binary about to be installed rejects.
    let config = paths.config();
    let rebuild = || format!("{} predates this mihoctl or this mihomo; run `mihoctl apply` first", config.display());
    let text = fs::read_to_string(&config).with_context(|| format!("reading {}", config.display()))?;
    let cfg: serde_json::Map<String, serde_json::Value> =
        serde_yaml_ng::from_str(&text).with_context(|| format!("parsing {}", config.display()))?;
    pipeline::reject_local_files(&cfg).with_context(rebuild)?;
    core::test_config(&settings.mihomo, &paths.core_dir, &config).with_context(rebuild)?;
    fs::create_dir_all(log_dir()?)?;

    let plist_path = core_plist_path().to_string_lossy().into_owned();
    let seed = paths.core_dir.to_string_lossy().into_owned();
    let mut args = vec![CORE_LABEL, ROOT_DIR, CORE_LOG_DIR, settings.mihomo.as_str(), &plist_path, &seed];
    args.extend(GEODATA);
    sudo_sh(INSTALL_SH, &args, &core_plist(paths))?;

    let agent = refresh_plist_path()?;
    fs::create_dir_all(agent.parent().unwrap())?;
    fs::write(&agent, refresh_plist(settings, paths)?)?;
    let domain = format!("gui/{}", uid()?);
    if is_loaded_user(REFRESH_LABEL) {
        launchctl(&["bootout", &format!("{domain}/{REFRESH_LABEL}")])?;
    }
    if !launchctl(&["bootstrap", &domain, &agent.to_string_lossy()])? {
        bail!("launchctl bootstrap {domain} {} failed", agent.display());
    }
    Ok(())
}

pub fn uninstall() -> Result<()> {
    let agent = refresh_plist_path()?;
    if is_loaded_user(REFRESH_LABEL) {
        launchctl(&["bootout", &format!("gui/{}/{REFRESH_LABEL}", uid()?)])?;
    }
    fs::remove_file(&agent).ok();
    // ROOT_DIR holds only what install put there or the core regenerates (binary, cache, ui, geodata).
    sudo_sh(
        r#"launchctl bootout "system/$1" 2>/dev/null || true
rm -f "$2"
rm -rf "$3""#,
        &[CORE_LABEL, &core_plist_path().to_string_lossy(), ROOT_DIR],
        "",
    )
}

/// Restart the core process (needed after changing `tun:`; plain reloads keep the TUN).
pub fn restart_core() -> Result<()> {
    sudo_sh(r#"launchctl kickstart -k "system/$1""#, &[CORE_LABEL], "")
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
