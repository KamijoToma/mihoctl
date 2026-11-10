//! Content-addressed subscription nodes, not whole-config or known-healthy snapshots.

use crate::{settings::{Paths, Settings}, state};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize, Serializer, ser::{SerializeMap, SerializeSeq}};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::Path;

const LIMIT: usize = 5;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Version {
    schema: u32,
    pub hash: String,
    pub at: u64,
    // Immutable first-seen ordering: timestamps can collide or the wall clock can move backwards.
    sequence: u64,
    pub proxies: Vec<Value>,
}

pub fn subscription(settings: &Settings, name: &str) -> Result<()> {
    valid_name(name)?;
    if !settings.subscriptions.iter().any(|s| s.name == name) { bail!("unknown subscription"); }
    Ok(())
}

fn valid_name(name: &str) -> Result<()> {
    if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
        bail!("invalid subscription name");
    }
    Ok(())
}

struct Canonical<'a>(&'a Value);

impl Serialize for Canonical<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        match self.0 {
            Value::Object(map) => {
                let mut keys: Vec<_> = map.keys().collect();
                keys.sort_unstable();
                let mut out = serializer.serialize_map(Some(map.len()))?;
                for key in keys { out.serialize_entry(key, &Canonical(&map[key]))?; }
                out.end()
            }
            Value::Array(values) => {
                let mut out = serializer.serialize_seq(Some(values.len()))?;
                for value in values { out.serialize_element(&Canonical(value))?; }
                out.end()
            }
            scalar => scalar.serialize(serializer),
        }
    }
}

struct HashWriter(Sha256);

impl std::io::Write for HashWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
}

pub fn hash(proxies: &[Value]) -> String {
    // Stream canonical JSON directly to SHA-256: no cloned node trees or serialized credential buffer.
    let mut writer = HashWriter(Sha256::new());
    {
        let mut serializer = serde_json::Serializer::new(&mut writer);
        let mut array = Serializer::serialize_seq(&mut serializer, Some(proxies.len())).expect("JSON array");
        for proxy in proxies { array.serialize_element(&Canonical(proxy)).expect("JSON node"); }
        SerializeSeq::end(array).expect("JSON array end");
    }
    format!("{:x}", writer.0.finalize())
}

fn validate_nodes(proxies: &[Value]) -> Result<()> {
    if proxies.is_empty() || proxies.iter().any(|node| !node.is_object()) {
        bail!("node cache must contain a non-empty proxies array of node mappings");
    }
    Ok(())
}

pub fn nodes(body: &[u8]) -> Result<Vec<Value>> {
    // Parser errors may quote credentials or URLs from the source; deliberately do not chain them.
    let mut value: Value = serde_yaml_ng::from_slice(body).map_err(|_| anyhow::anyhow!("invalid node cache YAML"))?;
    let proxies = match value.get_mut("proxies").map(Value::take) {
        Some(Value::Array(proxies)) => proxies,
        _ => bail!("node cache has no proxies array"),
    };
    validate_nodes(&proxies)?;
    Ok(proxies)
}

pub fn provider_body(proxies: &[Value]) -> Result<String> {
    #[derive(Serialize)]
    struct Provider<'a> { proxies: &'a [Value] }
    serde_yaml_ng::to_string(&Provider { proxies }).context("encoding node cache")
}

pub fn read_optional(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => bail!("cannot read local recovery file"),
    }
}

fn private_dir(path: &Path) -> Result<()> {
    fs::DirBuilder::new().recursive(true).mode(0o700).create(path).context("creating private history directory")?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).context("securing history directory")?;
    fs::File::open(path)?.sync_all()?;
    if let Some(parent) = path.parent() { fs::File::open(parent)?.sync_all()?; }
    Ok(())
}

pub fn versions(paths: &Paths, name: &str) -> Result<Vec<Version>> {
    valid_name(name)?;
    let dir = paths.history_dir(name);
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(_) => bail!("cannot read node history"),
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.context("reading history directory entry")?;
        let file = entry.path();
        if file.extension().and_then(|s| s.to_str()) != Some("json") { continue; }
        let stem = file.file_stem().and_then(|s| s.to_str()).context("invalid history filename")?;
        if stem.len() != 64 || !stem.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()) {
            bail!("invalid history filename");
        }
        if !entry.file_type()?.is_file() { bail!("history snapshot is not a regular file"); }
        let bytes = fs::read(&file).context("reading history snapshot")?;
        let version: Version = serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("invalid history schema"))?;
        validate_nodes(&version.proxies)?;
        if version.schema != 1 || version.hash != stem || hash(&version.proxies) != version.hash {
            bail!("history snapshot failed schema/content hash verification");
        }
        out.push(version);
    }
    out.sort_by(|a, b| b.sequence.cmp(&a.sequence).then_with(|| a.hash.cmp(&b.hash)));
    Ok(out)
}

/// Persist and fsync snapshot plus directory before the caller changes its provider cache.
pub fn preserve(paths: &Paths, name: &str, body: &[u8]) -> Result<String> {
    valid_name(name)?;
    let proxies = nodes(body)?;
    let digest = hash(&proxies);
    let retained = versions(paths, name)?;
    let dir = paths.history_dir(name);
    private_dir(dir.parent().context("history directory has no parent")?)?;
    private_dir(&dir)?;
    if retained.iter().any(|v| v.hash == digest) {
        // A previous attempt may have renamed a snapshot but failed its directory fsync.
        // Re-establish durability before allowing a duplicate to replace the current cache.
        let file = fs::File::open(dir.join(format!("{digest}.json")))?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        file.sync_all()?;
        fs::File::open(&dir)?.sync_all()?;
        return Ok(digest);
    }
    let sequence = retained.iter().map(|v| v.sequence).max().unwrap_or(0).checked_add(1)
        .context("history sequence exhausted")?;
    let version = Version { schema: 1, hash: digest.clone(), at: state::now_unix(), sequence, proxies };
    state::atomic_write(&dir.join(format!("{digest}.json")), &serde_json::to_vec(&version)?)
        .map_err(|_| anyhow::anyhow!("cannot durably preserve node history"))?;
    Ok(digest)
}

/// Prune only after installing successfully. Both current and pre-operation recovery stay protected,
/// including if a later deletion or directory fsync fails and the caller needs to roll back.
pub fn prune(paths: &Paths, name: &str, current: &str, recovery: Option<&str>) -> Result<()> {
    let mut retained = versions(paths, name)?;
    let position = retained.iter().position(|version| version.hash == current)
        .context("current nodes missing from history")?;
    if position > 0 && recovery != Some(current) {
        let sequence = retained[0].sequence.checked_add(1).context("history sequence exhausted")?;
        let version = &mut retained[position];
        version.sequence = sequence;
        state::atomic_write(&paths.history_dir(name).join(format!("{current}.json")), &serde_json::to_vec(version)?)
            .context("recording revisited node version")?;
        retained.sort_by(|a, b| b.sequence.cmp(&a.sequence).then_with(|| a.hash.cmp(&b.hash)));
    }
    let mut keep: Vec<&str> = vec![current];
    if let Some(old) = recovery.filter(|old| *old != current) { keep.push(old); }
    for version in &retained {
        if keep.len() < LIMIT && !keep.contains(&version.hash.as_str()) { keep.push(&version.hash); }
    }
    let dir = paths.history_dir(name);
    for version in &retained {
        if !keep.contains(&version.hash.as_str()) {
            fs::remove_file(dir.join(format!("{}.json", version.hash))).context("pruning node history")?;
        }
    }
    fs::File::open(&dir)?.sync_all().context("syncing pruned history")
}

pub fn select(paths: &Paths, name: &str, current: Option<&str>, selector: Option<&str>) -> Result<Version> {
    if let Some(s) = selector
        && (s.is_empty() || s.len() > 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        bail!("version must be a full SHA-256 hash or unique hexadecimal prefix");
    }
    let versions = versions(paths, name)?;
    match selector {
        Some(prefix) => {
            let prefix = prefix.to_ascii_lowercase();
            let mut matches = versions.into_iter().filter(|v| v.hash.starts_with(&prefix));
            let found = matches.next().context("no retained version matches this hash")?;
            if matches.next().is_some() { bail!("ambiguous version prefix; use more hexadecimal characters"); }
            Ok(found)
        }
        None => versions.into_iter().find(|v| Some(v.hash.as_str()) != current)
            .context("no retained version differs from the current node cache"),
    }
}

pub fn restore_file(path: &Path, body: Option<&[u8]>) -> Result<()> {
    match body {
        Some(body) => state::atomic_write(path, body),
        None => {
            match fs::remove_file(path) {
                Ok(()) => (),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(e) => return Err(e.into()),
            }
            fs::File::open(path.parent().context("recovery path has no parent")?)?.sync_all()?;
            Ok(())
        }
    }
}

/// Download persistence: history failure cannot discard the original provider cache.
pub fn replace(paths: &Paths, name: &str, body: &[u8]) -> Result<()> {
    valid_name(name)?;
    let file = paths.provider_file(name);
    let old = read_optional(&file)?;
    let recovery = old.as_deref().map(|bytes| preserve(paths, name, bytes)).transpose()?;
    // Identity comes from the exact provider document being persisted, not the upstream response.
    let current = preserve(paths, name, body)?;
    let result = state::atomic_write(&file, body).and_then(|()| prune(paths, name, &current, recovery.as_deref()));
    if result.is_err() {
        return match restore_file(&file, old.as_deref()) {
            Ok(()) => Err(anyhow::anyhow!("node cache replacement failed; previous cache preserved")),
            Err(_) => Err(anyhow::anyhow!("node cache replacement failed; CACHE ROLLBACK FAILED")),
        };
    }
    Ok(())
}

pub fn print(settings: &Settings, paths: &Paths, name: Option<&str>) -> Result<()> {
    if let Some(name) = name { subscription(settings, name)?; }
    println!("subscription\tversion (SHA-256)\ttimestamp (unix)\tnodes\tcurrent");
    for sub in settings.subscriptions.iter().filter(|s| name.is_none_or(|n| s.name == n)) {
        let cached = read_optional(&paths.provider_file(&sub.name))?;
        let current = cached.as_deref().map(nodes).transpose()?;
        let digest = current.as_deref().map(hash);
        let retained = versions(paths, &sub.name)?;
        for version in &retained {
            println!("{}\t{}\t{}\t{}\t{}", sub.name, version.hash, version.at, version.proxies.len(),
                if digest.as_deref() == Some(&version.hash) { "*" } else { "" });
        }
        if let (Some(proxies), Some(digest)) = (current, digest)
            && !retained.iter().any(|v| v.hash == digest)
        {
            println!("{}\t{digest}\tunrecorded\t{}\t* (legacy cache)", sub.name, proxies.len());
        }
        if cached.is_none() && retained.is_empty() { println!("{}\t(no cached versions)", sub.name); }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicU64, Ordering};

    pub fn temp_paths() -> Paths {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!("mihoctl-history-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::create_dir_all(&dir).unwrap();
        Paths::new(dir.join("settings.toml"), dir.clone(), dir.join("core"))
    }

    fn body(n: usize) -> Vec<u8> {
        serde_yaml_ng::to_string(&json!({"proxies": [{"name": format!("node-{n}"), "server": "node.example", "port": 443}]})).unwrap().into_bytes()
    }

    #[test]
    fn canonical_identity_preserves_all_node_content_and_array_order() {
        let a = nodes(b"metadata: ignored\nproxies: [{name: a, options: {z: 1, a: true}, alpn: [h2, http]}]").unwrap();
        let b = nodes(b"# formatting ignored\nproxies:\n  - alpn: [h2, http]\n    options: {a: true, z: 1}\n    name: a\n").unwrap();
        assert_eq!(hash(&a), hash(&b));
        let mut changed = b.clone();
        changed[0]["options"]["z"] = json!("1");
        assert_ne!(hash(&a), hash(&changed));
        changed = b.clone();
        changed[0]["alpn"] = json!(["http", "h2"]);
        assert_ne!(hash(&a), hash(&changed));
        assert_ne!(hash(&[a[0].clone(), json!({"name": "b"})]), hash(&[json!({"name": "b"}), a[0].clone()]));
        let aliases = nodes(b"anchor: &node {name: a, port: 443}\nproxies: [*node]\n").unwrap();
        assert_eq!(hash(&aliases), hash(&[json!({"port": 443, "name": "a"})]));
    }

    #[test]
    fn retention_deduplicates_and_default_selection_uses_stable_recency() {
        let paths = temp_paths();
        for n in 0..7 { replace(&paths, "airport", &body(n)).unwrap(); }
        let before = versions(&paths, "airport").unwrap();
        replace(&paths, "airport", &body(3)).unwrap();
        let after = versions(&paths, "airport").unwrap();
        let current = hash(&nodes(&body(3)).unwrap());
        let previous = select(&paths, "airport", Some(&current), None).unwrap();
        let metadata = fs::metadata(paths.history_dir("airport")).unwrap();
        let file_mode = fs::metadata(paths.history_dir("airport").join(format!("{current}.json"))).unwrap().permissions().mode() & 0o777;
        for n in [7, 8] { replace(&paths, "airport", &body(n)).unwrap(); }
        replace(&paths, "airport", &body(8)).unwrap();
        let recent: Vec<String> = [8, 7, 3, 6, 5].into_iter().map(|n| hash(&nodes(&body(n)).unwrap())).collect();
        let retained: Vec<String> = versions(&paths, "airport").unwrap().into_iter().map(|version| version.hash).collect();
        fs::remove_dir_all(&paths.config_dir).unwrap();
        assert_eq!(before.len(), 5);
        assert_eq!(after.len(), 5);
        assert_eq!(retained, recent);
        assert_eq!(previous.hash, hash(&nodes(&body(6)).unwrap()));
        assert_eq!(metadata.permissions().mode() & 0o777, 0o700);
        assert_eq!(file_mode, 0o600);
    }

    #[test]
    fn history_persistence_failure_keeps_legacy_cache_byte_for_byte() {
        let paths = temp_paths();
        let original = body(1);
        state::atomic_write(&paths.provider_file("airport"), &original).unwrap();
        fs::write(paths.core_dir.join("history"), b"not a directory").unwrap();
        let result = replace(&paths, "airport", &body(2));
        let cached = fs::read(paths.provider_file("airport")).unwrap();
        fs::remove_dir_all(&paths.config_dir).unwrap();
        assert!(result.is_err());
        assert_eq!(cached, original);
    }

    #[test]
    fn corrupt_snapshot_and_invalid_selectors_fail_without_touching_cache() {
        let paths = temp_paths();
        replace(&paths, "airport", &body(1)).unwrap();
        let digest = hash(&nodes(&body(1)).unwrap());
        let snapshot = paths.history_dir("airport").join(format!("{digest}.json"));
        let mut v: Value = serde_json::from_slice(&fs::read(&snapshot).unwrap()).unwrap();
        v["proxies"][0]["name"] = json!("tampered");
        fs::write(snapshot, serde_json::to_vec(&v).unwrap()).unwrap();
        let corrupt = select(&paths, "airport", None, Some(&digest));
        let update = replace(&paths, "airport", &body(2));
        let traversal = select(&paths, "airport", None, Some("../secret"));
        let cached = fs::read(paths.provider_file("airport")).unwrap();
        fs::remove_dir_all(&paths.config_dir).unwrap();
        assert!(corrupt.is_err());
        assert!(update.is_err());
        assert!(traversal.is_err());
        assert_eq!(cached, body(1));
    }

    #[test]
    fn selectors_reject_ambiguity_and_empty_snapshots() {
        let paths = temp_paths();
        let mut hashes = std::collections::HashMap::new();
        let mut collision = None;
        for n in 0..17 {
            let digest = preserve(&paths, "airport", &body(n)).unwrap();
            let prefix = digest[..1].to_string();
            if hashes.insert(prefix.clone(), digest).is_some() { collision = Some(prefix); break; }
        }
        let ambiguous = select(&paths, "airport", None, collision.as_deref());
        let empty = preserve(&paths, "airport", b"proxies: []\n");
        fs::remove_dir_all(&paths.config_dir).unwrap();
        assert!(ambiguous.is_err());
        assert!(empty.is_err());
    }
}
