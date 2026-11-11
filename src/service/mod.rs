//! Service units: the core as a privileged system service (TUN needs root) and the scheduled
//! refresh as a user job (it only talks to the controller, so it needs no privileges). launchd on
//! macOS, systemd on Linux, rc.d on FreeBSD; elsewhere only the config pipeline is available.
//!
//! Nothing the root core executes or writes may be user-writable, or any process running as the
//! user gets root without a password: it runs a root-owned copy of mihomo, with a root-owned `-d`
//! home (cache.db, ui/, geodata) and root-only logs. The only user file it touches is the config,
//! read via `-f`; that grants nothing the controller secret doesn't already, because mihomo
//! confines file paths in a config to its home.

#[cfg(target_os = "macos")]
mod launchd;
#[cfg(target_os = "macos")]
pub use launchd::*;

#[cfg(target_os = "linux")]
mod systemd;
#[cfg(target_os = "linux")]
pub use systemd::*;

#[cfg(target_os = "freebsd")]
mod rc;
#[cfg(target_os = "freebsd")]
pub use rc::*;

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "freebsd")))]
mod unsupported;
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "freebsd")))]
pub use unsupported::*;

use crate::settings::Settings;

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "freebsd"))]
use {
    crate::settings::Paths,
    crate::{core, pipeline},
    anyhow::{Context, Result, bail},
    std::{
        fs,
        io::Write,
        path::Path,
        process::{Command, Stdio},
    },
};

/// The binary the core actually runs: the installed copy, which lags `settings.mihomo` until the
/// next `service install`. Validate with it so `mihomo -t` and the core never disagree.
pub fn runtime_mihomo(settings: &Settings) -> String {
    match installed_mihomo() {
        Some(bin) if bin.exists() => bin.to_string_lossy().into_owned(),
        _ => settings.mihomo.clone(),
    }
}

/// Seeded from `core_dir` on install so the core can start where GitHub is blocked.
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "freebsd"))]
const GEODATA: [&str; 6] = ["GEOSITE.dat", "geoip.metadb", "ASN.mmdb", "geosite.dat", "geoip.dat", "Country.mmdb"];

/// The core starts straight from config.yaml: refuse a missing binary, or a config an older
/// mihoctl built with `file` providers, or one the binary about to be installed rejects.
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "freebsd"))]
fn check_installable(settings: &Settings, paths: &Paths) -> Result<()> {
    let config = paths.config();
    if !config.exists() {
        bail!("{} does not exist; run `mihoctl apply` first", config.display());
    }
    if !Path::new(&settings.mihomo).is_absolute() || !Path::new(&settings.mihomo).exists() {
        bail!("mihomo binary {} not found (set `mihomo` to an absolute path)", settings.mihomo);
    }
    let rebuild = || format!("{} predates this mihoctl or this mihomo; run `mihoctl apply` first", config.display());
    let text = fs::read_to_string(&config).with_context(|| format!("reading {}", config.display()))?;
    let cfg: serde_json::Map<String, serde_json::Value> =
        serde_yaml_ng::from_str(&text).with_context(|| format!("parsing {}", config.display()))?;
    pipeline::reject_local_files(&cfg).with_context(rebuild)?;
    core::test_config(&settings.mihomo, &paths.core_dir, &config).with_context(rebuild)
}

/// Run a shell script as root in one `sudo` call (one password prompt). `$1…` are `args`; `stdin`
/// is piped to the script (sudo reads the password from the tty, not stdin), so root-bound files
/// never pass through a user-writable temp file. Uses `sudo -A` when `SUDO_ASKPASS` is set.
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "freebsd"))]
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

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn output(cmd: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(cmd).args(args).output().with_context(|| format!("running {cmd}"))?;
    Ok(String::from_utf8(out.stdout)?.trim().to_string())
}
