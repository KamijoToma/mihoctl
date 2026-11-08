//! launchd units: the core as a root LaunchDaemon (TUN needs root) and the scheduled refresh
//! as a user LaunchAgent (it only talks to the controller, so it needs no privileges).

use crate::settings::{self, Paths, Settings};
use anyhow::{Context, Result, bail};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const CORE_LABEL: &str = "io.mihoctl.core";
pub const REFRESH_LABEL: &str = "io.mihoctl.refresh";

fn core_plist_path() -> PathBuf {
    PathBuf::from(format!("/Library/LaunchDaemons/{CORE_LABEL}.plist"))
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

pub fn core_plist(settings: &Settings, paths: &Paths) -> Result<String> {
    let log = log_dir()?.join("core.log");
    let log = esc(&log.to_string_lossy());
    let extra = format!(
        "  <key>RunAtLoad</key>\n  <true/>\n  <key>KeepAlive</key>\n  <true/>\n  <key>ProcessType</key>\n  <string>Interactive</string>\n  <key>StandardOutPath</key>\n  <string>{log}</string>\n  <key>StandardErrorPath</key>\n  <string>{log}</string>\n"
    );
    Ok(plist(CORE_LABEL, &[settings.mihomo.clone(), "-d".into(), paths.core_dir.to_string_lossy().into_owned()], &extra))
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

/// Run a shell script as root in one `sudo` call (one password prompt). `$1…` are `args`.
/// Uses `sudo -A` when `SUDO_ASKPASS` is set (GUI password prompt).
fn sudo_sh(script: &str, args: &[&str]) -> Result<()> {
    let mut cmd = Command::new("sudo");
    if std::env::var_os("SUDO_ASKPASS").is_some() {
        cmd.arg("-A");
    }
    let status = cmd.args(["/bin/sh", "-ec", script, "mihoctl"]).args(args).status().context("running sudo")?;
    if !status.success() {
        bail!("privileged step failed ({status})");
    }
    Ok(())
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

fn stage(contents: &str, name: &str) -> Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!("mihoctl-{}", std::process::id()));
    fs::create_dir_all(&dir)?;
    let tmp = dir.join(name);
    fs::write(&tmp, contents)?;
    Ok(tmp)
}

pub fn install(settings: &Settings, paths: &Paths) -> Result<()> {
    if !paths.config().exists() {
        bail!("{} does not exist; run `mihoctl apply` first", paths.config().display());
    }
    if !Path::new(&settings.mihomo).exists() {
        bail!("mihomo binary {} not found", settings.mihomo);
    }
    fs::create_dir_all(log_dir()?)?;

    let core = stage(&core_plist(settings, paths)?, "io.mihoctl.core.plist")?;
    let result = sudo_sh(
        r#"launchctl bootout "system/$1" 2>/dev/null || true
install -m 644 -o root -g wheel "$2" "$3"
launchctl bootstrap system "$3""#,
        &[CORE_LABEL, &core.to_string_lossy(), &core_plist_path().to_string_lossy()],
    );
    fs::remove_dir_all(core.parent().unwrap()).ok();
    result?;

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
    sudo_sh(
        r#"launchctl bootout "system/$1" 2>/dev/null || true
rm -f "$2""#,
        &[CORE_LABEL, &core_plist_path().to_string_lossy()],
    )
}

/// Restart the core process (needed after changing `tun:`; plain reloads keep the TUN).
pub fn restart_core() -> Result<()> {
    sudo_sh(r#"launchctl kickstart -k "system/$1""#, &[CORE_LABEL])
}
