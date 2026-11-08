//! `mihoctl status`: one snapshot gathered once, rendered as text or as JSON for scripts and menu-bar plugins.
//! The snapshot never carries subscription URLs or the controller secret.

use crate::core::Controller;
use crate::service;
use crate::settings::{Paths, Settings};
use crate::state::{self, ApplyRecord, State};
use anyhow::Result;
use serde::Serialize;
use std::path::PathBuf;

#[derive(Serialize)]
pub struct Status {
    pub settings: PathBuf,
    pub core_dir: PathBuf,
    pub launchd: Launchd,
    pub core: Core,
    pub subscriptions: Vec<Sub>,
    pub last_apply: Option<ApplyRecord>,
}

#[derive(Serialize)]
pub struct Launchd {
    pub core: bool,
    pub refresh: bool,
}

#[derive(Serialize)]
pub struct Core {
    pub controller: String,
    pub state: CoreState,
    pub version: Option<String>,
    pub mode: Option<String>,
    pub tun: Option<bool>,
    pub ipv6: Option<bool>,
    pub mixed_port: Option<u64>,
    pub group: String,
    /// Member currently selected in `health_check.group`.
    pub group_now: Option<String>,
    pub error: Option<String>,
}

#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum CoreState {
    Running,
    /// Nothing listens on the controller address.
    Down,
    /// Something answered but not as our core (see `error`).
    Error,
}

#[derive(Serialize)]
pub struct Sub {
    pub name: String,
    pub nodes: usize,
    pub updated_at: Option<u64>,
    pub last_attempt: Option<u64>,
    pub last_error: Option<String>,
    pub userinfo: Option<Userinfo>,
}

/// Parsed `subscription-userinfo`: bytes and a unix expiry; `raw` is the header as sent.
#[derive(Serialize, Debug, PartialEq)]
pub struct Userinfo {
    pub upload: Option<u64>,
    pub download: Option<u64>,
    pub total: Option<u64>,
    pub expire: Option<u64>,
    pub raw: String,
}

pub fn gather(settings: &Settings, paths: &Paths) -> Result<Status> {
    let st = State::load(&paths.state_file)?;
    let subscriptions = settings
        .subscriptions
        .iter()
        .map(|sub| {
            let s = st.subscriptions.get(&sub.name);
            Sub {
                name: sub.name.clone(),
                nodes: s.map_or(0, |s| s.nodes),
                updated_at: s.and_then(|s| s.updated_at),
                last_attempt: s.and_then(|s| s.last_attempt),
                last_error: s.and_then(|s| s.last_error.clone()),
                userinfo: s.and_then(|s| s.userinfo.as_deref()).map(parse_userinfo).map(|mut u| {
                    if !sub.expires {
                        u.expire = None;
                    }
                    u
                }),
            }
        })
        .collect();
    Ok(Status {
        settings: paths.settings_file.clone(),
        core_dir: paths.core_dir.clone(),
        launchd: Launchd {
            core: service::is_loaded_system(service::CORE_LABEL),
            refresh: service::is_loaded_user(service::REFRESH_LABEL),
        },
        core: core(settings),
        subscriptions,
        last_apply: st.last_apply,
    })
}

fn core(settings: &Settings) -> Core {
    let ctl = Controller::new(&settings.controller, &settings.secret);
    let mut c = Core {
        controller: settings.controller.clone(),
        state: CoreState::Down,
        version: None,
        mode: None,
        tun: None,
        ipv6: None,
        mixed_port: None,
        group: settings.health_check.group.clone(),
        group_now: None,
        error: None,
    };
    match ctl.version() {
        Ok(Some(v)) => {
            c.state = CoreState::Running;
            c.version = Some(v);
            match ctl.configs() {
                Ok(cfg) => {
                    c.mode = cfg["mode"].as_str().map(str::to_string);
                    c.tun = cfg["tun"]["enable"].as_bool();
                    c.ipv6 = cfg["ipv6"].as_bool();
                    c.mixed_port = cfg["mixed-port"].as_u64().filter(|p| *p > 0);
                }
                Err(e) => c.error = Some(format!("{e:#}")),
            }
            c.group_now = ctl.proxy(&settings.health_check.group).ok().and_then(|g| g["now"].as_str().map(str::to_string));
        }
        Ok(None) => {}
        Err(e) => {
            c.state = CoreState::Error;
            c.error = Some(format!("{e:#}"));
        }
    }
    c
}

/// `upload=1; download=2; total=3; expire=4`. Some airports send floats; they are truncated, and
/// out-of-range values saturate at `u64::MAX`.
pub fn parse_userinfo(raw: &str) -> Userinfo {
    let get = |k: &str| {
        raw.split(';')
            .filter_map(|kv| kv.split_once('='))
            .find(|(key, _)| key.trim() == k)
            .and_then(|(_, v)| v.trim().parse::<f64>().ok())
            .filter(|v| *v >= 0.0)
            .map(|v| v as u64)
    };
    Userinfo { upload: get("upload"), download: get("download"), total: get("total"), expire: get("expire"), raw: raw.into() }
}

pub fn print_json(s: &Status) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(s)?);
    Ok(())
}

pub fn print_text(s: &Status) {
    let c = &s.core;
    println!("settings   {}", s.settings.display());
    println!("core dir   {}", s.core_dir.display());
    let loaded = |b: bool| if b { "loaded" } else { "not loaded" };
    println!("launchd    core: {}  refresh: {}", loaded(s.launchd.core), loaded(s.launchd.refresh));
    match c.state {
        CoreState::Running => {
            let show = |v: Option<String>| v.unwrap_or_else(|| "?".into());
            println!(
                "core       mihomo {} at {}  mode={} tun={} ipv6={} mixed-port={}",
                show(c.version.clone()),
                c.controller,
                show(c.mode.clone()),
                show(c.tun.map(|b| b.to_string())),
                show(c.ipv6.map(|b| b.to_string())),
                show(c.mixed_port.map(|p| p.to_string())),
            );
            if let Some(e) = &c.error {
                println!("           {e}");
            }
            if let Some(now) = &c.group_now {
                println!("group      {} -> {now}", c.group);
            }
        }
        CoreState::Down => println!("core       not reachable at {}", c.controller),
        CoreState::Error => println!("core       {}", c.error.as_deref().unwrap_or("?")),
    }
    println!("subscriptions");
    for sub in &s.subscriptions {
        let mut line = format!("  {:<14} {:>3} nodes  updated {}", sub.name, sub.nodes, ago(sub.updated_at));
        if let Some(info) = &sub.userinfo {
            line.push_str(&format!("  [{}]", userinfo_summary(info)));
        }
        if let Some(err) = &sub.last_error {
            line.push_str(&format!("\n                 last attempt {} failed: {err}", ago(sub.last_attempt)));
        }
        println!("{line}");
    }
    if let Some(a) = &s.last_apply {
        println!("last apply {} {}: {}", ago(Some(a.at)), if a.ok { "ok" } else { "FAILED" }, a.message);
    }
}

fn ago(t: Option<u64>) -> String {
    let Some(t) = t else { return "never".into() };
    let d = state::now_unix().saturating_sub(t);
    match d {
        0..60 => format!("{d}s ago"),
        60..3600 => format!("{}m ago", d / 60),
        3600..86400 => format!("{}h ago", d / 3600),
        _ => format!("{}d ago", d / 86400),
    }
}

/// → `used 1.2G/100.0G, expires in 23d`. A `total` of 0 or 1 is a placeholder (pay-as-you-go
/// plans), shown as plain usage — the same threshold the SwiftBar plugin uses.
fn userinfo_summary(u: &Userinfo) -> String {
    let gib = |b: u64| format!("{:.1}G", b as f64 / (1u64 << 30) as f64);
    let mut parts = Vec::new();
    let used = u.upload.unwrap_or(0).saturating_add(u.download.unwrap_or(0));
    match u.total {
        Some(total) if total > 1 => parts.push(format!("used {}/{}", gib(used), gib(total))),
        _ if u.upload.is_some() || u.download.is_some() => parts.push(format!("used {}", gib(used))),
        _ => {}
    }
    if let Some(exp) = u.expire.filter(|e| *e > 0) {
        let days = i64::try_from(exp).unwrap_or(i64::MAX).saturating_sub(state::now_unix() as i64) / 86400;
        parts.push(format!("expires in {days}d"));
    }
    if parts.is_empty() { u.raw.clone() } else { parts.join(", ") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn userinfo_parses_ints_floats_and_missing_keys() {
        let u = parse_userinfo("upload=1024; download=2.5e3;total=107374182400; expire=");
        assert_eq!((u.upload, u.download, u.total, u.expire), (Some(1024), Some(2500), Some(107374182400), None));
        assert_eq!(parse_userinfo("garbage").total, None);
    }

    #[test]
    fn summary_falls_back_to_raw_header() {
        assert_eq!(userinfo_summary(&parse_userinfo("plan=pro")), "plan=pro");
        let u = parse_userinfo("upload=536870912; download=536870912; total=10737418240");
        assert_eq!(userinfo_summary(&u), "used 1.0G/10.0G");
    }

    #[test]
    fn placeholder_total_shows_plain_usage() {
        // Pay-as-you-go airports send `total=1`; that is not a 0.0G quota.
        let u = parse_userinfo("upload=536870912; download=536870912; total=1");
        assert_eq!(userinfo_summary(&u), "used 1.0G");
    }

    #[test]
    fn non_expiring_subscription_drops_expire() {
        let dir = std::env::temp_dir().join(format!("mihoctl-status-{}", std::process::id()));
        let paths = Paths::new(dir.join("m.toml"), dir.clone(), dir.clone());
        let mut st = State::default();
        for name in ["payg", "monthly"] {
            st.subscriptions.entry(name.into()).or_default().userinfo = Some("upload=1; download=1; total=1; expire=1".into());
        }
        st.save(&paths.state_file).unwrap();
        let settings: Settings = toml::from_str(
            "template = \"t.yaml\"\nsecret = \"s\"\ncontroller = \"127.0.0.1:1\"\n[[subscription]]\nname = \"payg\"\nurl = \"https://a.example\"\nexpires = false\n[[subscription]]\nname = \"monthly\"\nurl = \"https://b.example\"\n",
        )
        .unwrap();
        let subs: Vec<_> = settings.subscriptions.iter().map(|s| (s.name.clone(), s.expires)).collect();
        let got = gather(&settings, &paths).map(|s| s.subscriptions.iter().map(|s| s.userinfo.as_ref().unwrap().expire).collect::<Vec<_>>());
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(subs, [("payg".to_string(), false), ("monthly".to_string(), true)]);
        assert_eq!(got.unwrap(), [None, Some(1)]);
    }

    #[test]
    fn absurd_userinfo_values_saturate_instead_of_overflowing() {
        let u = parse_userinfo("upload=1e30; download=inf; total=1; expire=1e300");
        assert_eq!((u.upload, u.download, u.expire), (Some(u64::MAX), Some(u64::MAX), Some(u64::MAX)));
        assert!(userinfo_summary(&u).contains("expires in"));
    }
}
