use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Write via temp file + fsync + rename so readers (the core, a concurrent `status`) never see a
/// partial file. Mode 0600: generated configs carry the controller secret and node credentials.
pub fn atomic_write(path: &Path, contents: &[u8]) -> Result<()> {
    let dir = path.parent().context("path has no parent")?;
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = dir.join(format!(".{}.tmp", path.file_name().and_then(|n| n.to_str()).unwrap_or("file")));
    {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
        f.set_permissions(fs::Permissions::from_mode(0o600))?;
        f.write_all(contents)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))
}

pub fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub subscriptions: BTreeMap<String, SubState>,
    pub last_apply: Option<ApplyRecord>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SubState {
    /// Unix time of the last successful download that produced the cached provider file.
    pub updated_at: Option<u64>,
    pub nodes: usize,
    /// Raw `subscription-userinfo` header (traffic and expiry), when the server sends one.
    pub userinfo: Option<String>,
    pub last_attempt: Option<u64>,
    pub last_error: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ApplyRecord {
    pub at: u64,
    pub ok: bool,
    pub message: String,
}

impl State {
    pub fn load(path: &Path) -> Result<Self> {
        match fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        atomic_write(path, serde_json::to_string_pretty(self)?.as_bytes())
    }
}

/// Exclusive lock so the scheduled refresh and a manual run never interleave writes.
pub struct Lock(#[allow(dead_code)] fs::File);

pub fn lock(path: &Path) -> Result<Lock> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let f = fs::OpenOptions::new().create(true).truncate(false).write(true).open(path)?;
    f.try_lock().map_err(|_| anyhow::anyhow!("another mihoctl run holds {}", path.display()))?;
    Ok(Lock(f))
}
