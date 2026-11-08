//! template + providers + YAML overrides + JS scripts → validated config → hot reload.

use crate::core::{self, Controller};
use crate::merge;
use crate::script;
use crate::service;
use crate::settings::{self, Paths, Settings};
use crate::state::{self, ApplyRecord, State};
use crate::subscription;
use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};
use std::fs;
use std::path::Path;

/// Keys owned by mihoctl; templates and overrides cannot move the controller off loopback or
/// drop its secret, and the provider list always matches `mihoctl.toml`.
fn enforce(cfg: &mut Map<String, Value>, settings: &Settings) -> Result<()> {
    let host = settings.controller.rsplit_once(':').map(|(h, _)| h).unwrap_or("");
    if !matches!(host, "127.0.0.1" | "[::1]" | "localhost") {
        bail!("controller {:?} must bind a loopback address", settings.controller);
    }
    cfg.insert("external-controller".into(), settings.controller.clone().into());
    cfg.insert("secret".into(), settings.secret.clone().into());
    cfg.insert("external-ui".into(), "ui".into());
    cfg.insert("external-ui-url".into(), settings.ui_url.clone().into());
    for key in ["external-controller-tls", "external-controller-unix", "external-controller-pipe"] {
        cfg.remove(key);
    }
    Ok(())
}

/// Put each subscription's nodes into its provider after the overrides ran. An override may drop a
/// subscription's provider, but not turn it into something other than `inline`.
fn fill_nodes(cfg: &mut Map<String, Value>, nodes: Vec<(String, Value)>) -> Result<()> {
    let Some(Value::Object(providers)) = cfg.get_mut("proxy-providers") else {
        bail!("overrides removed the `proxy-providers` mapping");
    };
    for (name, payload) in nodes {
        match providers.get_mut(&name) {
            None => {}
            Some(Value::Object(p)) if p.get("type").and_then(Value::as_str) == Some("inline") => {
                p.insert("payload".into(), payload);
            }
            Some(_) => bail!("overrides changed proxy-provider {name:?}; it must stay `type: inline` (mihoctl fills in its nodes)"),
        }
    }
    Ok(())
}

/// The core runs from a root-owned home, so a `type: file` provider (resolved there, and only
/// readable inside it) would pass `mihomo -t` against `core_dir` and then be empty at runtime.
pub fn reject_local_files(cfg: &Map<String, Value>) -> Result<()> {
    for section in ["proxy-providers", "rule-providers"] {
        for (name, p) in cfg.get(section).and_then(Value::as_object).into_iter().flatten() {
            if p.get("type").and_then(Value::as_str) == Some("file") {
                bail!("{section}.{name} is `type: file`, which the core can't read from its root-owned home; use `type: inline` or `type: http`");
            }
        }
    }
    Ok(())
}

pub fn build(settings: &Settings, paths: &Paths) -> Result<String> {
    let template_path = settings::expand(&paths.config_dir, &settings.template)?;
    let text = fs::read_to_string(&template_path).with_context(|| format!("reading {}", template_path.display()))?;
    let mut cfg: Map<String, Value> = match serde_yaml_ng::from_str::<Value>(&text)
        .with_context(|| format!("parsing {}", template_path.display()))?
    {
        Value::Object(m) => m,
        _ => bail!("{} must be a YAML mapping", template_path.display()),
    };

    let providers = subscription::providers(settings, paths)?;
    if providers.entries.is_empty() {
        bail!("no subscription has cached nodes; run `mihoctl update` first");
    }
    let mut merged_providers = cfg.remove("proxy-providers").and_then(|v| v.as_object().cloned()).unwrap_or_default();
    merged_providers.extend(providers.entries);
    cfg.insert("proxy-providers".into(), Value::Object(merged_providers));

    let timeout = settings::parse_duration(&settings.script_timeout)?;
    for path in resolve_overrides(&paths.config_dir, &settings.overrides)? {
        let source = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("override");
        eprintln!("  override {}", path.display());
        match path.extension().and_then(|e| e.to_str()) {
            Some("js") => {
                cfg = match script::run(name, &source, &Value::Object(cfg), timeout)? {
                    Value::Object(m) => m,
                    _ => unreachable!("script::run returns objects"),
                };
            }
            Some("yaml" | "yml") => {
                let over: Value = serde_yaml_ng::from_str(&source).with_context(|| format!("parsing {}", path.display()))?;
                let Value::Object(over) = over else { bail!("{} must be a YAML mapping", path.display()) };
                merge::deep_merge(&mut cfg, over).with_context(|| format!("applying {}", path.display()))?;
            }
            _ => bail!("override {} must end in .yaml, .yml or .js", path.display()),
        }
    }

    fill_nodes(&mut cfg, providers.nodes)?;
    reject_local_files(&cfg)?;
    enforce(&mut cfg, settings)?;
    let header = "# Generated by mihoctl from mihoctl.toml; edit the template/overrides, not this file.\n";
    Ok(format!("{header}{}", serde_yaml_ng::to_string(&Value::Object(cfg))?))
}

const OVERRIDE_EXTS: [&str; 3] = ["js", "yaml", "yml"];

/// Overrides in execution order. Entries run in list order; a directory entry expands to its
/// `*.js` / `*.yaml` / `*.yml` files (non-recursive, dotfiles skipped) sorted by file name, so
/// `10-a.js` runs before `99-b.js`. Sorting is byte-wise: zero-pad prefixes (`05-`, not `5-`).
pub fn resolve_overrides(base: &Path, entries: &[String]) -> Result<Vec<std::path::PathBuf>> {
    let mut out = Vec::new();
    for raw in entries {
        let path = settings::expand(base, raw)?;
        if !path.is_dir() {
            out.push(path);
            continue;
        }
        let mut files = Vec::new();
        for entry in fs::read_dir(&path).with_context(|| format!("reading {}", path.display()))? {
            let p = entry?.path();
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
            if p.is_file() && !name.starts_with('.') && OVERRIDE_EXTS.contains(&ext) {
                files.push(p);
            }
        }
        files.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
        out.extend(files);
    }
    Ok(out)
}

/// Build + `mihomo -t` into the candidate file. Returns its path.
pub fn build_checked(settings: &Settings, paths: &Paths) -> Result<std::path::PathBuf> {
    let yaml = build(settings, paths)?;
    let candidate = paths.candidate();
    state::atomic_write(&candidate, yaml.as_bytes())?;
    core::test_config(&service::runtime_mihomo(settings), &paths.core_dir, &candidate)?;
    Ok(candidate)
}

pub enum Applied {
    /// Core running: hot-reloaded and health-checked.
    Reloaded { alive: usize },
    /// No core answering: config installed for the next start.
    Installed,
    Unchanged,
}

/// Swap a validated candidate in, hot reload, health check; restore the previous config on failure.
pub fn apply(settings: &Settings, paths: &Paths, st: &mut State, candidate: &Path) -> Result<Applied> {
    let target = paths.config();
    let new = fs::read(candidate)?;
    let old = fs::read(&target).ok();
    let ctl = Controller::new(&settings.controller, &settings.secret);
    let running = ctl.version()?.is_some();

    // Nodes are inlined into the config, so the same text means the same nodes too.
    if old.as_deref() == Some(new.as_slice()) {
        fs::remove_file(candidate).ok();
        record(st, true, "config unchanged");
        return Ok(Applied::Unchanged);
    }
    if let Some(old) = &old {
        state::atomic_write(&paths.last_good(), old)?;
    }
    fs::rename(candidate, &target)?;

    if !running {
        record(st, true, "installed (core not running)");
        return Ok(Applied::Installed);
    }

    let outcome = ctl.reload(&new).and_then(|()| {
        let hc = &settings.health_check;
        ctl.group_delay(&hc.group, &hc.url, hc.timeout_ms)
            .with_context(|| format!("health check: no member of group {:?} reached {}", hc.group, hc.url))
    });
    match outcome {
        Ok(alive) => {
            record(st, true, &format!("reloaded; {alive} nodes alive in {}", settings.health_check.group));
            Ok(Applied::Reloaded { alive })
        }
        Err(err) => {
            let rollback = match old {
                Some(old) => state::atomic_write(&target, &old).and_then(|()| ctl.reload(&old)),
                None => Ok(()),
            };
            match rollback {
                Ok(()) => bail!("{err:#}; rolled back to the previous config"),
                Err(rb) => bail!("{err:#}; ROLLBACK FAILED: {rb:#}"),
            }
        }
    }
}

fn record(st: &mut State, ok: bool, message: &str) {
    st.last_apply = Some(ApplyRecord { at: state::now_unix(), ok, message: message.to_string() });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(controller: &str) -> Settings {
        toml::from_str(&format!("template = \"t.yaml\"\nsecret = \"s3cret\"\ncontroller = \"{controller}\"\n")).unwrap()
    }

    #[test]
    fn enforce_overrides_template_controller_settings() {
        let mut cfg: Map<String, Value> = serde_json::from_str(
            r#"{"external-controller": "0.0.0.0:9090", "secret": "", "external-controller-tls": "0.0.0.0:9443"}"#,
        )
        .unwrap();
        enforce(&mut cfg, &settings("127.0.0.1:9090")).unwrap();
        assert_eq!(cfg["external-controller"], "127.0.0.1:9090");
        assert_eq!(cfg["secret"], "s3cret");
        assert!(!cfg.contains_key("external-controller-tls"));
    }

    #[test]
    fn enforce_rejects_non_loopback_controller() {
        let err = enforce(&mut Map::new(), &settings("0.0.0.0:9090")).unwrap_err();
        assert!(err.to_string().contains("loopback"));
    }

    #[test]
    fn nodes_fill_inline_providers_after_overrides() {
        let nodes = || vec![("a".to_string(), serde_json::json!([{"name": "hk"}])), ("gone".to_string(), serde_json::json!([]))];
        let mut cfg: Map<String, Value> =
            serde_json::from_str(r#"{"proxy-providers": {"a": {"type": "inline", "lazy": true}, "mine": {"type": "http"}}}"#).unwrap();
        fill_nodes(&mut cfg, nodes()).unwrap();
        assert_eq!(cfg["proxy-providers"]["a"]["payload"], serde_json::json!([{"name": "hk"}]));
        assert!(cfg["proxy-providers"].get("gone").is_none(), "a provider an override dropped stays dropped");

        let mut cfg: Map<String, Value> = serde_json::from_str(r#"{"proxy-providers": {"a": {"type": "file"}}}"#).unwrap();
        assert!(fill_nodes(&mut cfg, nodes()).unwrap_err().to_string().contains("must stay `type: inline`"));
        assert!(fill_nodes(&mut Map::new(), nodes()).is_err());
    }

    #[test]
    fn local_file_providers_are_rejected() {
        let ok: Map<String, Value> =
            serde_json::from_str(r#"{"proxy-providers": {"a": {"type": "inline"}}, "rule-providers": {"r": {"type": "http", "path": "./r.yaml"}}}"#).unwrap();
        reject_local_files(&ok).unwrap();
        let bad: Map<String, Value> = serde_json::from_str(r#"{"rule-providers": {"lan": {"type": "file", "path": "./lan.yaml"}}}"#).unwrap();
        assert!(reject_local_files(&bad).unwrap_err().to_string().contains("rule-providers.lan is `type: file`"));
    }

    #[test]
    fn directory_overrides_run_sorted_between_list_entries() {
        let dir = std::env::temp_dir().join(format!("mihoctl-order-{}", std::process::id()));
        let d = dir.join("overrides.d");
        fs::create_dir_all(&d).unwrap();
        for f in ["99-b.js", "10-a.js", "50-m.yaml", ".hidden.js", "README.md"] {
            fs::write(d.join(f), "").unwrap();
        }
        fs::create_dir(d.join("20-subdir.js")).unwrap();
        fs::write(dir.join("first.js"), "").unwrap();
        let entries = ["first.js".to_string(), "overrides.d".to_string(), "/abs/last.yaml".to_string()];
        let got: Vec<String> = resolve_overrides(&dir, &entries)
            .unwrap()
            .iter()
            .map(|p| p.strip_prefix(&dir).unwrap_or(p).to_string_lossy().into_owned())
            .collect();
        fs::remove_dir_all(&dir).ok();
        assert_eq!(got, ["first.js", "overrides.d/10-a.js", "overrides.d/50-m.yaml", "overrides.d/99-b.js", "/abs/last.yaml"]);
    }
}
