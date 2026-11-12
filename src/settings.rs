use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// `mihoctl.toml`. Relative paths resolve against the directory holding the file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    #[serde(default = "default_mihomo")]
    pub mihomo: String,
    pub core_dir: Option<String>,
    pub template: String,
    #[serde(default)]
    pub overrides: Vec<String>,
    #[serde(default = "default_controller")]
    pub controller: String,
    pub secret: String,
    #[serde(default = "default_ui_url")]
    pub ui_url: String,
    #[serde(default = "default_user_agent")]
    pub user_agent: String,
    #[serde(default = "default_refresh_interval")]
    pub refresh_interval: String,
    #[serde(default = "default_script_timeout")]
    pub script_timeout: String,
    /// Node-name regex dropped from every subscription (provider `exclude-filter`): traffic and
    /// expiry banners airports publish as fake nodes.
    #[serde(default = "default_exclude_filter")]
    pub exclude_filter: String,
    /// Keep nodes whose server is an IPv6 literal. Leave false while the template sets `ipv6: false`.
    #[serde(default)]
    pub ipv6_nodes: bool,
    #[serde(default)]
    pub health_check: HealthCheck,
    #[serde(default, rename = "subscription")]
    pub subscriptions: Vec<Subscription>,
    /// How the dashboard is published on the tailnet.
    #[serde(default)]
    pub tailscale: Tailscale,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HealthCheck {
    #[serde(default = "default_health_group")]
    pub group: String,
    #[serde(default = "default_health_url")]
    pub url: String,
    #[serde(default = "default_health_timeout")]
    pub timeout_ms: u64,
}

impl Default for HealthCheck {
    fn default() -> Self {
        Self { group: default_health_group(), url: default_health_url(), timeout_ms: default_health_timeout() }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Subscription {
    pub name: String,
    pub url: String,
    /// `auto`: through the running core's mixed port, falling back to a direct request.
    #[serde(default)]
    pub via: Via,
    pub user_agent: Option<String>,
    /// Prefix added to node names (mihomo `override.additional-prefix`). Defaults to `[name] `.
    pub prefix: Option<String>,
    /// `false` for plans that never expire (pay-as-you-go) whose airport still sends a meaningless
    /// `expire` in `subscription-userinfo`: status drops it, so nothing warns about it.
    #[serde(default = "yes")]
    pub expires: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Default, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Via {
    #[default]
    Auto,
    Proxy,
    Direct,
}

#[cfg(target_os = "macos")]
fn default_mihomo() -> String {
    "/opt/homebrew/opt/mihomo/bin/mihomo".into()
}

/// The first `mihomo` on `PATH`, made absolute so `service install` can copy it.
#[cfg(not(target_os = "macos"))]
fn default_mihomo() -> String {
    std::env::var_os("PATH")
        .iter()
        .flat_map(std::env::split_paths)
        .map(|dir| dir.join("mihomo"))
        .find(|p| p.is_absolute() && p.is_file())
        .map_or_else(|| "mihomo".into(), |p| p.to_string_lossy().into_owned())
}

fn default_controller() -> String {
    "127.0.0.1:9090".into()
}
fn default_ui_url() -> String {
    "https://github.com/Zephyruso/zashboard/releases/latest/download/dist.zip".into()
}
fn default_user_agent() -> String {
    "clash.meta".into()
}
fn default_refresh_interval() -> String {
    "6h".into()
}
fn default_script_timeout() -> String {
    "5s".into()
}
fn default_exclude_filter() -> String {
    "剩余|流量|到期|官网|套餐|重置|余额|状态|频道|t\\.me|Expire|Traffic".into()
}
fn default_health_group() -> String {
    "自动选择".into()
}
fn default_health_url() -> String {
    "https://www.gstatic.com/generate_204".into()
}
fn default_health_timeout() -> u64 {
    5000
}

/// How `mihoctl ui --tailscale` publishes the dashboard on the tailnet.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tailscale {
    /// Mount path on the tailnet. A prefix leaves the node's HTTPS root free for other services.
    #[serde(default = "default_tailscale_path")]
    pub path: String,
    /// The tailnet HTTPS port to publish on.
    #[serde(default = "default_tailscale_port")]
    pub port: u16,
}

impl Default for Tailscale {
    fn default() -> Self {
        Self { path: default_tailscale_path(), port: default_tailscale_port() }
    }
}

fn default_tailscale_path() -> String {
    "/mihoctl".into()
}
fn default_tailscale_port() -> u16 {
    443
}

/// Resolved locations used by every command.
#[derive(Debug)]
pub struct Paths {
    pub settings_file: PathBuf,
    pub config_dir: PathBuf,
    /// User-owned workspace: config.yaml (the root core reads it via `-f`), candidates, node cache,
    /// state, and the `-d` home for `mihomo -t` (geodata). The root core's home is separate.
    pub core_dir: PathBuf,
    pub providers_dir: PathBuf,
    pub state_file: PathBuf,
    pub lock_file: PathBuf,
}

impl Paths {
    pub fn new(settings_file: PathBuf, config_dir: PathBuf, core_dir: PathBuf) -> Self {
        Self {
            settings_file,
            config_dir,
            providers_dir: core_dir.join("providers"),
            // Next to the core dir, so two setups (e.g. a test core) never share state or locks.
            state_file: core_dir.join("mihoctl-state.json"),
            lock_file: core_dir.join("mihoctl.lock"),
            core_dir,
        }
    }
    pub fn config(&self) -> PathBuf {
        self.core_dir.join("config.yaml")
    }
    pub fn candidate(&self) -> PathBuf {
        self.core_dir.join("config.candidate.yaml")
    }
    pub fn last_good(&self) -> PathBuf {
        self.core_dir.join("config.last-good.yaml")
    }
    pub fn provider_file(&self, name: &str) -> PathBuf {
        self.providers_dir.join(format!("{name}.yaml"))
    }
    pub fn history_dir(&self, name: &str) -> PathBuf {
        self.core_dir.join("history").join(name)
    }
}

pub fn home() -> Result<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from).context("HOME is not set")
}

/// `$XDG_*_HOME` when set to an absolute path, else `~/<fallback>`.
fn xdg(var: &str, fallback: &str) -> Result<PathBuf> {
    match std::env::var_os(var).map(PathBuf::from) {
        Some(p) if p.is_absolute() => Ok(p),
        _ => Ok(home()?.join(fallback)),
    }
}

/// macOS ignores `$XDG_CONFIG_HOME`: mihoctl has always looked in `~/.config` there.
pub fn config_home() -> Result<PathBuf> {
    if cfg!(target_os = "macos") { Ok(home()?.join(".config")) } else { xdg("XDG_CONFIG_HOME", ".config") }
}

fn default_core_dir() -> Result<PathBuf> {
    if cfg!(target_os = "macos") {
        Ok(home()?.join("Library/Application Support/mihoctl/core"))
    } else {
        Ok(xdg("XDG_DATA_HOME", ".local/share")?.join("mihoctl/core"))
    }
}

pub fn expand(base: &Path, raw: &str) -> Result<PathBuf> {
    if let Some(rest) = raw.strip_prefix("~/") {
        return Ok(home()?.join(rest));
    }
    let p = PathBuf::from(raw);
    Ok(if p.is_absolute() { p } else { base.join(p) })
}

pub fn default_settings_file() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("MIHOCTL_CONFIG") {
        return Ok(PathBuf::from(p));
    }
    Ok(config_home()?.join("mihoctl/mihoctl.toml"))
}

pub fn load(path: &Path) -> Result<(Settings, Paths)> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let settings: Settings = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    validate(&settings)?;

    let config_dir = path.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
    let core_dir = match &settings.core_dir {
        Some(raw) => expand(&config_dir, raw)?,
        None => default_core_dir()?,
    };
    Ok((settings, Paths::new(path.to_path_buf(), config_dir, core_dir)))
}

fn validate(s: &Settings) -> Result<()> {
    if s.secret.trim().is_empty() {
        bail!("`secret` must be non-empty: the controller is the only thing guarding the core");
    }
    let mut seen = std::collections::HashSet::new();
    for sub in &s.subscriptions {
        let ok = !sub.name.is_empty()
            && sub.name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !ok {
            bail!("subscription name {:?} must match [A-Za-z0-9_-]+ (it becomes a file and provider name)", sub.name);
        }
        if !seen.insert(sub.name.as_str()) {
            bail!("duplicate subscription name {:?}", sub.name);
        }
    }
    parse_duration(&s.refresh_interval).context("refresh_interval")?;
    parse_duration(&s.script_timeout).context("script_timeout")?;
    Ok(())
}

/// `90s`, `15m`, `6h`, `1d`.
pub fn parse_duration(raw: &str) -> Result<Duration> {
    let raw = raw.trim();
    let split = raw.find(|c: char| !c.is_ascii_digit()).unwrap_or(raw.len());
    let (num, unit) = raw.split_at(split);
    let n: u64 = num.parse().with_context(|| format!("invalid duration {raw:?}"))?;
    let secs = match unit {
        "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        "d" => n * 86400,
        _ => bail!("invalid duration {raw:?}: use s, m, h or d"),
    };
    if secs == 0 {
        bail!("duration must be positive");
    }
    Ok(Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal valid config, so each test only states what it is about.
    fn toml_with(extra: &str) -> Result<Settings> {
        let text = format!("template = \"t.yaml\"\nsecret = \"s3cret\"\n{extra}");
        Ok(toml::from_str(&text)?)
    }

    #[test]
    fn tailscale_defaults_to_a_prefixed_mount_on_443() {
        let s = toml_with("").unwrap();
        assert_eq!(s.tailscale.path, "/mihoctl");
        assert_eq!(s.tailscale.port, 443);
    }

    #[test]
    fn tailscale_section_overrides_the_defaults() {
        let s = toml_with("[tailscale]\npath = \"/dash\"\nport = 8443\n").unwrap();
        assert_eq!(s.tailscale.path, "/dash");
        assert_eq!(s.tailscale.port, 8443);
    }

    #[test]
    fn an_unknown_tailscale_key_is_rejected() {
        let err = toml_with("[tailscale]\nprot = 443\n").unwrap_err().to_string();
        assert!(err.contains("unknown field"), "unexpected error: {err}");
    }
}
