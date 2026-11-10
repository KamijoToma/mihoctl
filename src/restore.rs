//! Restore exactly one provider's nodes with the current config-building inputs.

use crate::{core::{self, Controller}, history, pipeline::{self, Applied}, service, settings::{Paths, Settings}, state::{self, State}};
use anyhow::{Result, bail};
#[cfg(test)]
use serde_json::json;
use std::fs;

pub struct Restored {
    pub hash: String,
    pub applied: Applied,
}

pub fn run(settings: &Settings, paths: &Paths, name: &str, selector: Option<&str>) -> Result<Restored> {
    run_using(settings, paths, name, selector, &service::runtime_mihomo(settings))
}

fn run_using(settings: &Settings, paths: &Paths, name: &str, selector: Option<&str>, mihomo: &str) -> Result<Restored> {
    history::subscription(settings, name)?;
    let cache_file = paths.provider_file(name);
    let old_cache = history::read_optional(&cache_file)?;
    let current = old_cache.as_deref().map(history::nodes).transpose()?.as_deref().map(history::hash);
    // Authenticate schema and content before any cache/config mutation, even for an explicit current hash.
    let version = history::select(paths, name, current.as_deref(), selector)?;
    let old_config = history::read_optional(&paths.config())?;
    let old_state = history::read_optional(&paths.state_file)?;
    let mut st = State::load(&paths.state_file).map_err(|_| anyhow::anyhow!("cannot load local state for restore"))?;
    let ctl = Controller::new(&settings.controller, &settings.secret);
    let running = ctl.version().map_err(|_| anyhow::anyhow!("cannot establish core status for safe restore"))?.is_some();
    if running && old_config.is_none() {
        bail!("cannot restore a running core without a saved rollback config; no nodes changed");
    }
    // Protect the exact bytes, not a rebuilt version of the current template/overrides.
    if let Some(old) = &old_config {
        state::atomic_write(&paths.last_good(), old)
            .map_err(|_| anyhow::anyhow!("cannot protect rollback config; no nodes changed"))?;
        if running {
            core::test_config(mihomo, &paths.core_dir, &paths.last_good())
                .map_err(|_| anyhow::anyhow!("saved rollback config failed validation; no nodes changed"))?;
        }
    }
    let recovery = old_cache.as_deref().map(|body| history::preserve(paths, name, body)).transpose()?;
    let body = history::provider_body(&version.proxies)
        .map_err(|_| anyhow::anyhow!("cannot encode restored nodes"))?;
    let mut live_changed = false;
    let mut state_changed = false;
    let outcome = (|| -> Result<Applied> {
        state::atomic_write(&cache_file, body.as_bytes())
            .map_err(|_| anyhow::anyhow!("restore could not install selected node cache"))?;
        let candidate = pipeline::build_checked_with(settings, paths, mihomo, false)
            .map_err(|_| anyhow::anyhow!("restore build/config validation failed (source details withheld)"))?;
        live_changed = running;
        let applied = pipeline::apply_restore(settings, paths, &mut st, &candidate, running)
            .map_err(|_| anyhow::anyhow!("restore config installation, payload reload or probe-group health check failed"))?;
        let entry = st.subscriptions.entry(name.to_string()).or_default();
        entry.nodes = version.proxies.len();
        entry.restored_at = Some(state::now_unix());
        // Keep last_attempt, updated_at, userinfo and last_error: this was not a subscription download.
        let detail = match &applied {
            Applied::Reloaded { alive } => format!("reloaded; {alive} alive in probe group {:?} (group scope)", settings.health_check.group),
            Applied::Installed => "installed for next start; core not running, not probed".into(),
            Applied::Unchanged => unreachable!("restore forces installation"),
        };
        st.last_apply = Some(state::ApplyRecord {
            at: state::now_unix(), ok: true, message: format!("restored nodes for {name}; {detail}"),
        });
        state_changed = true;
        st.save(&paths.state_file).map_err(|_| anyhow::anyhow!("restore state save failed"))?;
        history::prune(paths, name, &version.hash, recovery.as_deref())
            .map_err(|_| anyhow::anyhow!("restore history commit failed"))?;
        Ok(applied)
    })();
    match outcome {
        Ok(applied) => Ok(Restored { hash: version.hash, applied }),
        Err(err) => {
            let mut failed = Vec::new();
            // Never short-circuit: a failed cache write must not prevent config/runtime recovery.
            if history::restore_file(&cache_file, old_cache.as_deref()).is_err() { failed.push("node cache"); }
            if history::restore_file(&paths.config(), old_config.as_deref()).is_err() { failed.push("active config file"); }
            if live_changed && let Some(old) = &old_config && ctl.reload(old).is_err() {
                failed.push("running core reload");
            }
            if state_changed && history::restore_file(&paths.state_file, old_state.as_deref()).is_err() {
                failed.push("state file");
            }
            if clean_candidate(paths).is_err() { failed.push("candidate cleanup"); }
            if failed.is_empty() { bail!("{err}; previous node cache and config restored"); }
            bail!("{err}; ROLLBACK FAILED: {} (other rollback components were attempted)", failed.join(", "));
        }
    }
}

fn clean_candidate(paths: &Paths) -> Result<()> {
    let mut failed = false;
    for path in [paths.candidate(), paths.core_dir.join(".config.candidate.yaml.tmp")] {
        match fs::remove_file(path) {
            Ok(()) => (),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => (),
            Err(_) => failed = true,
        }
    }
    if failed { bail!("cannot remove restore candidate"); }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Arc, Mutex, atomic::{AtomicBool, AtomicU64, Ordering}};
    use std::thread;

    // Inject only the validator executable, so tests never use an installed privileged core binary.
    fn run(settings: &Settings, paths: &Paths, name: &str, selector: Option<&str>) -> Result<Restored> {
        super::run_using(settings, paths, name, selector, &settings.mihomo)
    }

    struct Fixture { settings: Settings, paths: Paths, original_cache: Vec<u8>, target: String }

    impl Fixture {
        fn new(controller: &str, validator: &str) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!("mihoctl-restore-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
            fs::create_dir_all(&dir).unwrap();
            let paths = Paths::new(dir.join("mihoctl.toml"), dir.clone(), dir.join("core"));
            let binary = dir.join("validator");
            fs::write(&binary, format!("#!/bin/sh\n{validator}\n")).unwrap();
            fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
            let mut settings: Settings = toml::from_str(&format!(
                "template = 'template.yaml'\nsecret = 'fixture-secret'\ncontroller = '{controller}'\n[[subscription]]\nname = 'airport'\nurl = 'https://never-contact.example/subscription'\n[[subscription]]\nname = 'other'\nurl = 'https://never-contact.example/other'\n"
            )).unwrap();
            settings.mihomo = binary.to_str().unwrap().to_string();
            fs::write(dir.join("template.yaml"), "mixed-port: 7890\nproxy-groups: []\nrules: [MATCH,DIRECT]\n").unwrap();
            let target_body = b"proxies: [{name: restored, type: ss, server: restored.example, port: 443, password: fixture-node-secret}]\n";
            history::replace(&paths, "airport", target_body).unwrap();
            let target = history::hash(&history::nodes(target_body).unwrap());
            let original_cache = b"# retain exact original bytes\nproxies: [{name: original, type: ss, server: original.example, port: 443, password: fixture-original-secret}]\n".to_vec();
            history::replace(&paths, "airport", &original_cache).unwrap();
            state::atomic_write(&paths.provider_file("other"), b"proxies: [{name: other, type: ss, server: other.example, port: 443}]\n").unwrap();
            state::atomic_write(&paths.config(), b"# original active config\nmixed-port: 7890\n").unwrap();
            let mut st = State::default();
            st.subscriptions.insert("airport".into(), state::SubState {
                nodes: 1, updated_at: Some(11), last_attempt: Some(12), last_error: Some("previous download failed".into()),
                ..Default::default()
            });
            st.save(&paths.state_file).unwrap();
            Self { settings, paths, original_cache, target }
        }
        fn clean(&self) { fs::remove_dir_all(&self.paths.config_dir).unwrap(); }
    }

    impl Drop for Fixture {
        fn drop(&mut self) { fs::remove_dir_all(&self.paths.config_dir).ok(); }
    }

    struct MockCore {
        addr: String,
        payloads: Arc<Mutex<Vec<String>>>,
        stop: Arc<AtomicBool>,
        thread: Option<thread::JoinHandle<()>>,
    }

    impl MockCore {
        // Fail the first reload, return zero alive, or run a successful probe. No subscription I/O.
        fn new(mode: &'static str, state_path: Option<std::path::PathBuf>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let addr = listener.local_addr().unwrap().to_string();
            let payloads = Arc::new(Mutex::new(Vec::new()));
            let captured = Arc::clone(&payloads);
            let stop = Arc::new(AtomicBool::new(false));
            let stopped = Arc::clone(&stop);
            let thread = thread::spawn(move || {
                let mut reloads = 0;
                while !stopped.load(Ordering::Relaxed) {
                    let (mut stream, _) = match listener.accept() {
                        Ok(pair) => pair,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(std::time::Duration::from_millis(2)); continue;
                        }
                        Err(_) => break,
                    };
                    stream.set_nonblocking(false).unwrap();
                    stream.set_read_timeout(Some(std::time::Duration::from_secs(2))).unwrap();
                    let mut bytes = Vec::new();
                    let mut buf = [0u8; 4096];
                    let header_end = loop {
                        let n = stream.read(&mut buf).unwrap();
                        if n == 0 { return; }
                        bytes.extend_from_slice(&buf[..n]);
                        if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") { break end + 4; }
                    };
                    let header = String::from_utf8_lossy(&bytes[..header_end]).into_owned();
                    let length = header.lines().find_map(|line| {
                        let (key, value) = line.split_once(':')?;
                        key.eq_ignore_ascii_case("content-length").then(|| value.trim().parse::<usize>().unwrap())
                    }).unwrap_or(0);
                    while bytes.len() < header_end + length {
                        let n = stream.read(&mut buf).unwrap();
                        if n == 0 { break; }
                        bytes.extend_from_slice(&buf[..n]);
                    }
                    let (status, body) = if header.starts_with("GET /version ") {
                        (200, r#"{"version":"fixture"}"#)
                    } else if header.starts_with("PUT /configs?") {
                        reloads += 1;
                        let body: serde_json::Value = serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap();
                        captured.lock().unwrap().push(body["payload"].as_str().unwrap().to_string());
                        if mode == "reload-failure" && reloads == 1 { (500, "sensitive controller response") } else { (204, "") }
                    } else if header.starts_with("GET /group/") {
                        if let Some(path) = &state_path {
                            if mode == "history-commit-failure" {
                                fs::write(path, b"{}").unwrap();
                            } else {
                                fs::remove_file(path).unwrap();
                                fs::create_dir(path).unwrap();
                            }
                        }
                        if mode == "zero" { (200, r#"{"node":0}"#) }
                        else if mode == "empty" { (200, "{}") }
                        else if matches!(mode, "probe-failure" | "cache-rollback-failure") { (504, "sensitive probe response") }
                        else { (200, r#"{"node":42}"#) }
                    } else { (500, "unexpected request") };
                    write!(stream, "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                }
            });
            Self { addr, payloads, stop, thread: Some(thread) }
        }
    }

    impl Drop for MockCore {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(thread) = self.thread.take() { thread.join().unwrap(); }
        }
    }

    #[test]
    fn failed_build_restores_both_files_and_removes_candidate() {
        let fixture = Fixture::new("127.0.0.1:1", "exit 1");
        let old_config = fs::read(fixture.paths.config()).unwrap();
        let old_state = fs::read(&fixture.paths.state_file).unwrap();
        let err = run(&fixture.settings, &fixture.paths, "airport", Some(&fixture.target)).err().unwrap();
        let cache = fs::read(fixture.paths.provider_file("airport")).unwrap();
        let cfg = fs::read(fixture.paths.config()).unwrap();
        let state = fs::read(&fixture.paths.state_file).unwrap();
        let candidate_exists = fixture.paths.candidate().exists();
        let recovery = history::select(&fixture.paths, "airport", None, Some(&history::hash(&history::nodes(&fixture.original_cache).unwrap())));
        fixture.clean();
        assert!(format!("{err:#}").contains("build/config validation failed"));
        assert_eq!(cache, fixture.original_cache);
        assert_eq!(cfg, old_config);
        assert_eq!(state, old_state);
        assert!(!candidate_exists);
        assert!(recovery.is_ok());
    }

    #[test]
    fn reload_and_group_failures_roll_back_cache_config_runtime_and_state() {
        for mode in ["reload-failure", "probe-failure", "zero", "empty"] {
            let core = MockCore::new(mode, None);
            let fixture = Fixture::new(&core.addr, "exit 0");
            let old_config = fs::read(fixture.paths.config()).unwrap();
            let old_state = fs::read(&fixture.paths.state_file).unwrap();
            let err = run(&fixture.settings, &fixture.paths, "airport", Some(&fixture.target)).err().unwrap();
            let cache = fs::read(fixture.paths.provider_file("airport")).unwrap();
            let cfg = fs::read(fixture.paths.config()).unwrap();
            let state = fs::read(&fixture.paths.state_file).unwrap();
            let payloads = core.payloads.lock().unwrap().clone();
            let candidate_exists = fixture.paths.candidate().exists();
            fixture.clean();
            assert_eq!(cache, fixture.original_cache, "{mode}");
            assert_eq!(cfg, old_config, "{mode}");
            assert_eq!(state, old_state, "{mode}");
            assert_eq!(payloads.len(), 2, "{mode}");
            assert!(payloads[0].contains("restored.example"), "{mode}");
            assert_eq!(payloads[1].as_bytes(), old_config, "{mode}");
            assert!(!candidate_exists);
            let message = format!("{err:#}");
            assert!(!message.contains("fixture-node-secret"));
            assert!(!message.contains("sensitive"));
            assert!(!message.contains("http"));
        }
    }

    #[test]
    fn success_uses_current_template_other_providers_and_preserves_download_diagnostics() {
        let core = MockCore::new("ok", None);
        let mut fixture = Fixture::new(&core.addr, "exit 0");
        fs::write(fixture.paths.config_dir.join("template.yaml"), "mixed-port: 8877\nproxy-groups: []\nrules: [MATCH,DIRECT]\n").unwrap();
        fixture.settings.overrides.push("current.js".into());
        fs::write(fixture.paths.config_dir.join("current.js"),
            "function main(c) { console.log('fixture-node-secret', 'https://never-print.example'); c['allow-lan'] = true; return c; }"
        ).unwrap();
        let result = run(&fixture.settings, &fixture.paths, "airport", None).unwrap();
        let cfg = fs::read_to_string(fixture.paths.config()).unwrap();
        let st = State::load(&fixture.paths.state_file).unwrap();
        let entry = &st.subscriptions["airport"];
        fixture.clean();
        assert!(matches!(result.applied, Applied::Reloaded { alive: 1 }));
        assert_eq!(result.hash, fixture.target);
        assert!(cfg.contains("8877"));
        assert!(cfg.contains("allow-lan: true"));
        assert!(cfg.contains("other.example"));
        assert!(cfg.contains("restored.example"));
        assert_eq!(entry.updated_at, Some(11));
        assert_eq!(entry.last_attempt, Some(12));
        assert_eq!(entry.last_error.as_deref(), Some("previous download failed"));
        assert_eq!(entry.nodes, 1);
        assert!(entry.restored_at.is_some());
        assert!(st.last_apply.unwrap().message.contains("probe group"));
    }

    #[test]
    fn stopped_core_installs_without_claiming_reload_or_probe() {
        let fixture = Fixture::new("127.0.0.1:1", "exit 0");
        let result = run(&fixture.settings, &fixture.paths, "airport", None).unwrap();
        let st = State::load(&fixture.paths.state_file).unwrap();
        fixture.clean();
        assert!(matches!(result.applied, Applied::Installed));
        assert!(st.last_apply.unwrap().message.contains("not probed"));
    }

    #[test]
    fn running_core_without_valid_rollback_fails_before_changing_cache() {
        let core = MockCore::new("ok", None);
        let fixture = Fixture::new(&core.addr, "exit 1");
        let invalid = run(&fixture.settings, &fixture.paths, "airport", None);
        fs::remove_file(fixture.paths.config()).unwrap();
        let missing = run(&fixture.settings, &fixture.paths, "airport", None);
        let cache = fs::read(fixture.paths.provider_file("airport")).unwrap();
        fixture.clean();
        assert!(invalid.is_err());
        assert!(missing.is_err());
        assert_eq!(cache, fixture.original_cache);
        assert!(core.payloads.lock().unwrap().is_empty());
    }

    #[test]
    fn state_save_failure_still_rolls_back_cache_and_running_config() {
        let mut fixture = Fixture::new("127.0.0.1:1", "exit 0");
        let core = MockCore::new("ok", Some(fixture.paths.state_file.clone()));
        fixture.settings.controller = core.addr.clone();
        let old_config = fs::read(fixture.paths.config()).unwrap();
        let err = run(&fixture.settings, &fixture.paths, "airport", None).err().unwrap();
        let cache = fs::read(fixture.paths.provider_file("airport")).unwrap();
        let cfg = fs::read(fixture.paths.config()).unwrap();
        let payloads = core.payloads.lock().unwrap().clone();
        fixture.clean();
        assert_eq!(cache, fixture.original_cache);
        assert_eq!(cfg, old_config);
        assert_eq!(payloads.last().unwrap().as_bytes(), old_config);
        assert!(format!("{err:#}").contains("ROLLBACK FAILED: state file"));
    }

    #[test]
    fn corrupt_or_empty_selected_snapshot_cannot_overwrite_current_cache() {
        let fixture = Fixture::new("127.0.0.1:1", "exit 0");
        let snapshot = fixture.paths.history_dir("airport").join(format!("{}.json", fixture.target));
        let mut value: serde_json::Value = serde_json::from_slice(&fs::read(&snapshot).unwrap()).unwrap();
        value["proxies"] = json!([]);
        fs::write(snapshot, serde_json::to_vec(&value).unwrap()).unwrap();
        let result = run(&fixture.settings, &fixture.paths, "airport", Some(&fixture.target));
        let unknown = run(&fixture.settings, &fixture.paths, "unknown", Some(&fixture.target));
        let cache = fs::read(fixture.paths.provider_file("airport")).unwrap();
        fixture.clean();
        assert!(result.is_err());
        assert!(unknown.is_err());
        assert_eq!(cache, fixture.original_cache);
    }

    #[test]
    fn history_commit_failure_rolls_back_state_cache_and_runtime_without_pruning_recovery() {
        let mut fixture = Fixture::new("127.0.0.1:1", "exit 0");
        let old_config = fs::read(fixture.paths.config()).unwrap();
        let old_state = fs::read(&fixture.paths.state_file).unwrap();
        let old_hash = history::hash(&history::nodes(&fixture.original_cache).unwrap());
        let snapshot = fixture.paths.history_dir("airport").join(format!("{}.json", fixture.target));
        let recovery = fixture.paths.history_dir("airport").join(format!("{old_hash}.json"));
        let old_snapshot = fs::read(&recovery).unwrap();
        let core = MockCore::new("history-commit-failure", Some(snapshot));
        fixture.settings.controller = core.addr.clone();
        let err = run(&fixture.settings, &fixture.paths, "airport", None).err().unwrap();
        let cache = fs::read(fixture.paths.provider_file("airport")).unwrap();
        let cfg = fs::read(fixture.paths.config()).unwrap();
        let state = fs::read(&fixture.paths.state_file).unwrap();
        let recovered_snapshot = fs::read(recovery).unwrap();
        let payloads = core.payloads.lock().unwrap().clone();
        fixture.clean();
        assert_eq!(cache, fixture.original_cache);
        assert_eq!(cfg, old_config);
        assert_eq!(state, old_state);
        assert_eq!(recovered_snapshot, old_snapshot);
        assert_eq!(payloads.last().unwrap().as_bytes(), old_config);
        assert!(format!("{err:#}").contains("history commit failed"));
    }

    #[test]
    fn cache_rollback_failure_does_not_skip_config_or_runtime_recovery() {
        let mut fixture = Fixture::new("127.0.0.1:1", "exit 0");
        let old_config = fs::read(fixture.paths.config()).unwrap();
        let core = MockCore::new("cache-rollback-failure", Some(fixture.paths.provider_file("airport")));
        fixture.settings.controller = core.addr.clone();
        let err = run(&fixture.settings, &fixture.paths, "airport", None).err().unwrap();
        let cfg = fs::read(fixture.paths.config()).unwrap();
        let payloads = core.payloads.lock().unwrap().clone();
        fixture.clean();
        assert_eq!(cfg, old_config);
        assert_eq!(payloads.last().unwrap().as_bytes(), old_config);
        let message = format!("{err:#}");
        assert!(message.contains("ROLLBACK FAILED: node cache"));
        assert!(!message.contains("active config file"));
        assert!(!message.contains("running core reload"));
    }
}
