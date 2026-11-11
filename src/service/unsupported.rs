//! No service manager integration on this platform: run `mihomo -d <home> -f <core_dir>/config.yaml`
//! and schedule `mihoctl refresh` with the system's own tools (rc.d, cron, …).

use crate::settings::{Paths, Settings};
use anyhow::{Result, bail};
use std::path::PathBuf;

pub const MANAGER: &str = "none";
pub const CORE_NAME: &str = "core";
pub const REFRESH_NAME: &str = "refresh";

fn unsupported<T>() -> Result<T> {
    bail!(
        "`mihoctl service` supports launchd (macOS), systemd (Linux) and rc.d (FreeBSD) only; run the core and schedule `mihoctl refresh` yourself"
    )
}

pub fn installed_mihomo() -> Option<PathBuf> {
    None
}

pub fn units(_: &Settings, _: &Paths) -> Result<Vec<(PathBuf, String)>> {
    unsupported()
}

pub fn core_loaded() -> bool {
    false
}

pub fn refresh_loaded() -> bool {
    false
}

pub fn install(_: &Settings, _: &Paths) -> Result<()> {
    unsupported()
}

pub fn uninstall() -> Result<()> {
    unsupported()
}

pub fn restart_core() -> Result<()> {
    unsupported()
}
