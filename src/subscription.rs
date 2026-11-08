//! Subscriptions are node sources only: we keep their `proxies` and drop everything else
//! (groups, rules, DNS, inbounds). Each one becomes a `file` proxy-provider whose content we
//! download, validate and cache ourselves, so a broken or unreachable subscription never
//! replaces the last good node list.

use crate::settings::{Paths, Settings, Subscription, Via};
use crate::state;
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Map, Value, json};
use std::time::Duration;

const MAX_BODY: u64 = 32 << 20;

pub struct Fetched {
    pub proxies: Vec<Value>,
    pub userinfo: Option<String>,
    pub route: &'static str,
}

/// Download every selected subscription concurrently and replace each cache file that validates.
/// Failures keep the previous cache and are recorded in state; the result lists them.
pub fn update_all(
    settings: &Settings,
    paths: &Paths,
    state: &mut state::State,
    only: &[String],
    core_proxy: Option<String>,
) -> Vec<(String, Result<usize>)> {
    let selected: Vec<&Subscription> = settings
        .subscriptions
        .iter()
        .filter(|s| only.is_empty() || only.iter().any(|n| n == &s.name))
        .collect();

    let results: Vec<(String, Result<Fetched>)> = std::thread::scope(|scope| {
        let handles: Vec<_> = selected
            .iter()
            .map(|sub| {
                let proxy = core_proxy.clone();
                let ua = sub.user_agent.clone().unwrap_or_else(|| settings.user_agent.clone());
                let keep_ipv6 = settings.ipv6_nodes;
                scope.spawn(move || (sub.name.clone(), fetch(sub, &ua, proxy.as_deref(), keep_ipv6)))
            })
            .collect();
        handles.into_iter().map(|h| h.join().expect("fetch thread panicked")).collect()
    });

    let now = state::now_unix();
    results
        .into_iter()
        .map(|(name, res)| {
            let entry = state.subscriptions.entry(name.clone()).or_default();
            entry.last_attempt = Some(now);
            let outcome = res.and_then(|f| {
                let body = serde_yaml_ng::to_string(&json!({ "proxies": f.proxies }))?;
                state::atomic_write(&paths.provider_file(&name), body.as_bytes())?;
                entry.updated_at = Some(now);
                entry.nodes = f.proxies.len();
                entry.userinfo = f.userinfo;
                eprintln!("  {name}: {} nodes ({})", f.proxies.len(), f.route);
                Ok(f.proxies.len())
            });
            entry.last_error = outcome.as_ref().err().map(|e| format!("{e:#}"));
            (name, outcome)
        })
        .collect()
}

fn fetch(sub: &Subscription, ua: &str, core_proxy: Option<&str>, keep_ipv6: bool) -> Result<Fetched> {
    let attempts: Vec<(&'static str, Option<&str>)> = match (sub.via, core_proxy) {
        (Via::Direct, _) => vec![("direct", None)],
        (Via::Proxy, Some(p)) => vec![("via core", Some(p))],
        (Via::Proxy, None) => bail!("via = \"proxy\" but no running core / --proxy to go through"),
        (Via::Auto, Some(p)) => vec![("via core", Some(p)), ("direct", None)],
        (Via::Auto, None) => vec![("direct", None)],
    };
    let mut errors = Vec::new();
    for (route, proxy) in attempts {
        match fetch_once(&sub.url, ua, proxy, keep_ipv6) {
            Ok((proxies, userinfo)) => return Ok(Fetched { proxies, userinfo, route }),
            Err(e) => errors.push(format!("{route}: {e:#}")),
        }
    }
    bail!("{}", errors.join("; "))
}

fn fetch_once(url: &str, ua: &str, proxy: Option<&str>, keep_ipv6: bool) -> Result<(Vec<Value>, Option<String>)> {
    let proxy = proxy.map(ureq::Proxy::new).transpose()?;
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .proxy(proxy)
        .user_agent(ua)
        .timeout_global(Some(Duration::from_secs(30)))
        .build()
        .into();
    // ureq errors embed the URL, which carries the subscription token; strip it.
    let mut resp = agent.get(url).call().map_err(|e| anyhow!("{}", redact(&e.to_string(), url)))?;
    let userinfo = resp
        .headers()
        .get("subscription-userinfo")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let body = resp
        .body_mut()
        .with_config()
        .limit(MAX_BODY)
        .read_to_string()
        .map_err(|e| anyhow!("{}", redact(&e.to_string(), url)))?;
    Ok((extract_proxies(&body, keep_ipv6)?, userinfo))
}

fn redact(msg: &str, url: &str) -> String {
    msg.replace(url, "<subscription url>")
}

/// Accept only mihomo YAML with a non-empty `proxies` list of named, typed nodes. Dropped here:
/// placeholder "nodes" airports use as banners (port 0/1, e.g. `1.1.1.1:1`), and — when the
/// template disables IPv6 — nodes whose server is an IPv6 literal, which could never connect but
/// would still be picked by url-test before their first failed probe.
pub fn extract_proxies(body: &str, keep_ipv6: bool) -> Result<Vec<Value>> {
    let doc: Value = serde_yaml_ng::from_str(body).map_err(|_| {
        anyhow!("response is not mihomo YAML (got {:?}…); try another user_agent", body.chars().take(40).collect::<String>())
    })?;
    let proxies = doc
        .get("proxies")
        .and_then(Value::as_array)
        .context("response has no `proxies` list; try another user_agent")?;
    for (i, p) in proxies.iter().enumerate() {
        let ok = p.get("name").and_then(Value::as_str).is_some_and(|s| !s.is_empty())
            && p.get("type").and_then(Value::as_str).is_some_and(|s| !s.is_empty());
        if !ok {
            bail!("node #{i} lacks a string `name`/`type`");
        }
    }
    let usable = |p: &&Value| {
        let placeholder = p.get("port").and_then(Value::as_u64).is_some_and(|port| port <= 1);
        let v6_only = !keep_ipv6
            && p.get("server")
                .and_then(Value::as_str)
                .is_some_and(|s| s.trim_matches(['[', ']']).parse::<std::net::Ipv6Addr>().is_ok());
        !placeholder && !v6_only
    };
    let real: Vec<Value> = proxies.iter().filter(usable).cloned().collect();
    if real.is_empty() {
        bail!("subscription returned zero usable nodes");
    }
    Ok(real)
}

/// `proxy-providers` entries for every subscription that has a cached node file.
pub fn providers(settings: &Settings, paths: &Paths) -> Map<String, Value> {
    let mut out = Map::new();
    for sub in &settings.subscriptions {
        if !paths.provider_file(&sub.name).exists() {
            eprintln!("warning: {} has no cached nodes yet; run `mihoctl update`", sub.name);
            continue;
        }
        let prefix = sub.prefix.clone().unwrap_or_else(|| format!("[{}] ", sub.name));
        out.insert(
            sub.name.clone(),
            json!({
                "type": "file",
                "path": format!("./providers/{}.yaml", sub.name),
                "health-check": {
                    "enable": true,
                    "url": settings.health_check.url,
                    "interval": 600,
                    "timeout": settings.health_check.timeout_ms,
                    "lazy": true,
                },
                "exclude-filter": settings.exclude_filter,
                "override": { "additional-prefix": prefix },
            }),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_only_proxies_from_full_config() {
        let body = "mixed-port: 7890\nproxies:\n  - {name: a, type: ss, server: 1.1.1.1, port: 443}\nproxy-groups: []\nrules: [MATCH,DIRECT]\n";
        let p = extract_proxies(body, false).unwrap();
        assert_eq!(p.len(), 1);
        assert_eq!(p[0]["name"], "a");
    }

    #[test]
    fn drops_placeholder_banner_nodes() {
        let body = "proxies:\n  - {name: '剩余流量：1 GB', type: ss, server: 1.1.1.1, port: 1}\n  - {name: '——Asia——', type: ss, server: 1.1.1.2, port: 0}\n  - {name: hk, type: ss, server: h.example, port: 8388}\n";
        let names: Vec<_> = extract_proxies(body, false).unwrap().iter().map(|p| p["name"].clone()).collect();
        assert_eq!(names, vec![json!("hk")]);
    }

    #[test]
    fn ipv6_literal_servers_dropped_unless_enabled() {
        let body = "proxies:\n  - {name: v6, type: ss, server: '2001:db8::1', port: 443}\n  - {name: v6b, type: ss, server: '[2001:db8:1::1]', port: 443}\n  - {name: v4, type: ss, server: 1.2.3.4, port: 443}\n  - {name: dns, type: ss, server: v6.example.com, port: 443}\n";
        let names = |keep| extract_proxies(body, keep).unwrap().iter().map(|p| p["name"].as_str().unwrap().to_string()).collect::<Vec<_>>();
        assert_eq!(names(false), ["v4", "dns"]);
        assert_eq!(names(true), ["v6", "v6b", "v4", "dns"]);
    }

    #[test]
    fn rejects_unusable_responses() {
        for (body, needle) in [
            ("{\"outbounds\": []}", "no `proxies`"),
            ("proxies: []", "zero usable nodes"),
            ("proxies:\n  - {name: x, type: ss, server: 1.1.1.1, port: 1}", "zero usable nodes"),
            ("proxies:\n  - {type: ss}", "lacks"),
            ("c3M6Ly9ZV1Z6TFRJMU5pMW5ZMjA2", "no `proxies`"),
        ] {
            let err = extract_proxies(body, false).unwrap_err().to_string();
            assert!(err.contains(needle), "{body}: {err}");
        }
    }

    #[test]
    fn redact_strips_token_url() {
        let url = "https://x.example/sub?token=SECRET";
        assert_eq!(redact(&format!("io error for {url}: timeout"), url), "io error for <subscription url>: timeout");
    }
}
