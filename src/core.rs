//! The mihomo binary (`-t` validation) and its REST controller.

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

/// `mihomo -t`: full config parse (rules, groups, inline proxies, geodata references) without
/// starting anything. `home` is the `-d` directory used to resolve geodata and relative paths.
pub fn test_config(mihomo: &str, home: &Path, file: &Path) -> Result<()> {
    let out = Command::new(mihomo)
        .arg("-t")
        .arg("-d")
        .arg(home)
        .arg("-f")
        .arg(file)
        .output()
        .with_context(|| format!("running {mihomo}"))?;
    if out.status.success() {
        return Ok(());
    }
    let mut msg = String::from_utf8_lossy(&out.stdout).into_owned();
    msg.push_str(&String::from_utf8_lossy(&out.stderr));
    let detail: Vec<&str> = msg.lines().filter(|l| !l.trim().is_empty()).collect();
    bail!("mihomo -t rejected {}:\n  {}", file.display(), detail.join("\n  "))
}

pub struct Controller {
    base: String,
    secret: String,
    agent: ureq::Agent,
}

impl Controller {
    pub fn new(addr: &str, secret: &str) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .proxy(None)
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(15)))
            .build()
            .into();
        Self { base: format!("http://{addr}"), secret: secret.to_string(), agent }
    }

    fn call(&self, method: &str, path: &str, body: Option<Value>) -> Result<(u16, String)> {
        let url = format!("{}{}", self.base, path);
        let auth = format!("Bearer {}", self.secret);
        let resp = match (method, body) {
            ("GET", _) => self.agent.get(&url).header("Authorization", &auth).call(),
            ("PUT", Some(b)) => self.agent.put(&url).header("Authorization", &auth).send_json(b),
            ("PUT", None) => self.agent.put(&url).header("Authorization", &auth).send_empty(),
            ("POST", _) => self.agent.post(&url).header("Authorization", &auth).send_empty(),
            _ => unreachable!("unsupported method"),
        };
        let mut resp = resp.map_err(|e| anyhow!("{method} {path}: {e}"))?;
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().unwrap_or_default();
        Ok((status, text))
    }

    fn expect_ok(&self, method: &str, path: &str, body: Option<Value>) -> Result<String> {
        let (status, text) = self.call(method, path, body)?;
        if !(200..300).contains(&status) {
            let msg = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v.get("message").and_then(Value::as_str).map(str::to_string))
                .unwrap_or(text);
            bail!("{method} {path}: HTTP {status}: {msg}");
        }
        Ok(text)
    }

    /// `Some(version)` when a core answers with our secret; `None` when nothing listens.
    pub fn version(&self) -> Result<Option<String>> {
        match self.call("GET", "/version", None) {
            Ok((200, text)) => {
                let v: Value = serde_json::from_str(&text)?;
                Ok(Some(v["version"].as_str().unwrap_or("?").to_string()))
            }
            Ok((401, _)) => bail!(
                "controller at {} rejected the secret: another client (FlClash, Clash Verge…) owns this port; quit it or change `controller`",
                self.base
            ),
            Ok((status, text)) => bail!("controller at {}: HTTP {status}: {text}", self.base),
            Err(_) => Ok(None),
        }
    }

    pub fn configs(&self) -> Result<Value> {
        Ok(serde_json::from_str(&self.expect_ok("GET", "/configs", None)?)?)
    }

    /// Reload the core's own config file. `force` re-applies listeners (ports); unchanged
    /// listeners and an unchanged `tun:` block are kept, so connections survive.
    pub fn reload(&self, path: &Path) -> Result<()> {
        let body = serde_json::json!({ "path": path.to_string_lossy() });
        self.expect_ok("PUT", "/configs?force=true", Some(body)).map(drop)
    }

    /// URL-test every member of a group; returns how many answered. mihomo replies 504 when none did.
    pub fn group_delay(&self, group: &str, url: &str, timeout_ms: u64) -> Result<usize> {
        let path = format!("/group/{}/delay?url={}&timeout={timeout_ms}", encode(group), encode(url));
        let v: Value = serde_json::from_str(&self.expect_ok("GET", &path, None)?)?;
        Ok(v.as_object().map_or(0, |m| m.values().filter(|d| d.as_u64().is_some_and(|d| d > 0)).count()))
    }

    pub fn proxy(&self, name: &str) -> Result<Value> {
        Ok(serde_json::from_str(&self.expect_ok("GET", &format!("/proxies/{}", encode(name)), None)?)?)
    }

    pub fn update_provider(&self, name: &str) -> Result<()> {
        self.expect_ok("PUT", &format!("/providers/proxies/{}", encode(name)), None).map(drop)
    }

    pub fn update_ui(&self) -> Result<()> {
        self.expect_ok("POST", "/upgrade/ui", None).map(drop)
    }
}

pub fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
