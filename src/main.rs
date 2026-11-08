mod core;
mod merge;
mod pipeline;
mod script;
mod service;
mod settings;
mod state;
mod subscription;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use pipeline::Applied;
use settings::{Paths, Settings};
use state::State;
use std::path::PathBuf;

/// Headless mihomo manager: subscriptions as node sources, template + overrides, validated hot reload.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Settings file (default: $MIHOCTL_CONFIG or ~/.config/mihoctl/mihoctl.toml)
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Download subscriptions into the node cache (all, or the named ones)
    Update {
        names: Vec<String>,
        /// Proxy for downloads instead of the running core's mixed port, e.g. http://127.0.0.1:7897
        #[arg(long)]
        proxy: Option<String>,
    },
    /// Generate and validate the config; print it, or write it to a file
    Build {
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Build, validate, install and hot-reload; roll back if the health check fails
    Apply,
    /// update + apply (what the scheduled job runs)
    Refresh,
    /// Show core, controller, subscription and last-apply status
    Status,
    /// Print the zashboard URL (pre-filled with controller and secret)
    Ui {
        /// Ask the core to (re)download the dashboard first
        #[arg(long)]
        update: bool,
    },
    /// Install/remove the launchd units (core LaunchDaemon + refresh LaunchAgent)
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
}

#[derive(Subcommand)]
enum ServiceAction {
    Install,
    Uninstall,
    /// Restart the core (needed after editing `tun:`; ordinary applies keep the TUN up)
    Restart,
    /// Print the generated plists without installing
    Print,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let file = match cli.config {
        Some(p) => p,
        None => settings::default_settings_file()?,
    };
    let (settings, paths) = settings::load(&file)?;
    std::fs::create_dir_all(&paths.providers_dir)
        .with_context(|| format!("creating {}", paths.providers_dir.display()))?;

    match cli.cmd {
        Cmd::Update { names, proxy } => {
            let _lock = state::lock(&paths.lock_file)?;
            for n in &names {
                if !settings.subscriptions.iter().any(|s| &s.name == n) {
                    bail!("unknown subscription {n:?}");
                }
            }
            update(&settings, &paths, &names, proxy)?;
        }
        Cmd::Build { output } => {
            let candidate = pipeline::build_checked(&settings, &paths)?;
            match output {
                Some(out) => {
                    std::fs::rename(&candidate, &out).or_else(|_| std::fs::copy(&candidate, &out).map(drop))?;
                    eprintln!("wrote {} (mihomo -t ok)", out.display());
                }
                None => {
                    print!("{}", std::fs::read_to_string(&candidate)?);
                    std::fs::remove_file(candidate).ok();
                }
            }
        }
        Cmd::Apply => {
            let _lock = state::lock(&paths.lock_file)?;
            apply(&settings, &paths)?;
        }
        Cmd::Refresh => {
            let _lock = state::lock(&paths.lock_file)?;
            eprintln!("--- refresh {}", state::now_unix());
            let failed = update(&settings, &paths, &[], None)?;
            apply(&settings, &paths)?;
            if failed > 0 {
                bail!("{failed} subscription(s) failed to update; kept their cached nodes");
            }
        }
        Cmd::Status => status(&settings, &paths)?,
        Cmd::Ui { update } => {
            let ctl = core::Controller::new(&settings.controller, &settings.secret);
            if update {
                ctl.update_ui()?;
            }
            let (host, port) = settings.controller.rsplit_once(':').context("controller must be host:port")?;
            println!(
                "http://{}/ui/#/setup?hostname={}&port={}&secret={}",
                settings.controller,
                core::encode(host),
                port,
                core::encode(&settings.secret)
            );
        }
        Cmd::Service { action } => match action {
            ServiceAction::Install => {
                service::install(&settings, &paths)?;
                eprintln!("installed {} and {}", service::CORE_LABEL, service::REFRESH_LABEL);
            }
            ServiceAction::Uninstall => {
                service::uninstall()?;
                eprintln!("removed launchd units");
            }
            ServiceAction::Restart => service::restart_core()?,
            ServiceAction::Print => {
                println!("{}", service::core_plist(&paths));
                println!("{}", service::refresh_plist(&settings, &paths)?);
            }
        },
    }
    Ok(())
}

/// Returns how many subscriptions failed.
fn update(settings: &Settings, paths: &Paths, names: &[String], proxy: Option<String>) -> Result<usize> {
    let mut st = State::load(&paths.state_file)?;
    let core_proxy = proxy.or_else(|| core_mixed_proxy(settings));
    let results = subscription::update_all(settings, paths, &mut st, names, core_proxy);
    st.save(&paths.state_file)?;
    let failed: Vec<_> = results.iter().filter_map(|(n, r)| r.as_ref().err().map(|e| (n, e))).collect();
    for (name, err) in &failed {
        eprintln!("  {name}: FAILED, keeping cached nodes: {err:#}");
    }
    if failed.len() == results.len() && !results.is_empty() {
        bail!("every subscription failed to update");
    }
    Ok(failed.len())
}

/// The running core's mixed port, so downloads of blocked subscription hosts can go through it.
fn core_mixed_proxy(settings: &Settings) -> Option<String> {
    let ctl = core::Controller::new(&settings.controller, &settings.secret);
    let cfg = ctl.configs().ok()?;
    let port = cfg["mixed-port"].as_u64().filter(|p| *p > 0)?;
    Some(format!("http://127.0.0.1:{port}"))
}

fn apply(settings: &Settings, paths: &Paths) -> Result<()> {
    let mut st = State::load(&paths.state_file)?;
    let result = pipeline::build_checked(settings, paths).and_then(|c| pipeline::apply(settings, paths, &mut st, &c));
    if let Err(e) = &result {
        st.last_apply = Some(state::ApplyRecord { at: state::now_unix(), ok: false, message: format!("{e:#}") });
    }
    st.save(&paths.state_file)?;
    match result? {
        Applied::Reloaded { alive } => eprintln!("applied and reloaded; {alive} nodes alive in {}", settings.health_check.group),
        Applied::Installed => eprintln!("installed {} (core not running)", paths.config().display()),
        Applied::Unchanged => eprintln!("config unchanged; providers refreshed"),
    }
    Ok(())
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

fn status(settings: &Settings, paths: &Paths) -> Result<()> {
    let ctl = core::Controller::new(&settings.controller, &settings.secret);
    println!("settings   {}", paths.settings_file.display());
    println!("core dir   {}", paths.core_dir.display());
    println!(
        "launchd    core: {}  refresh: {}",
        if service::is_loaded_system(service::CORE_LABEL) { "loaded" } else { "not loaded" },
        if service::is_loaded_user(service::REFRESH_LABEL) { "loaded" } else { "not loaded" },
    );
    match ctl.version() {
        Ok(Some(v)) => {
            let cfg = ctl.configs()?;
            println!(
                "core       mihomo {v} at {}  mode={} tun={} ipv6={} mixed-port={}",
                settings.controller, cfg["mode"], cfg["tun"]["enable"], cfg["ipv6"], cfg["mixed-port"]
            );
            if let Ok(g) = ctl.proxy(&settings.health_check.group) {
                println!("group      {} -> {}", settings.health_check.group, g["now"].as_str().unwrap_or("?"));
            }
        }
        Ok(None) => println!("core       not reachable at {}", settings.controller),
        Err(e) => println!("core       {e:#}"),
    }
    let st = State::load(&paths.state_file)?;
    println!("subscriptions");
    for sub in &settings.subscriptions {
        let s = st.subscriptions.get(&sub.name);
        let mut line = format!(
            "  {:<14} {:>3} nodes  updated {}",
            sub.name,
            s.map_or(0, |s| s.nodes),
            ago(s.and_then(|s| s.updated_at))
        );
        if let Some(info) = s.and_then(|s| s.userinfo.as_deref()) {
            line.push_str(&format!("  [{}]", userinfo_summary(info)));
        }
        if let Some(err) = s.and_then(|s| s.last_error.as_deref()) {
            line.push_str(&format!("\n                 last attempt {} failed: {err}", ago(s.and_then(|s| s.last_attempt))));
        }
        println!("{line}");
    }
    if let Some(a) = &st.last_apply {
        println!("last apply {} {}: {}", ago(Some(a.at)), if a.ok { "ok" } else { "FAILED" }, a.message);
    }
    Ok(())
}

/// `upload=1; download=2; total=3; expire=4` → `used 1.2G/100G, expires 2026-12-01`.
fn userinfo_summary(raw: &str) -> String {
    let get = |k: &str| {
        raw.split(';')
            .filter_map(|kv| kv.split_once('='))
            .find(|(key, _)| key.trim() == k)
            .and_then(|(_, v)| v.trim().parse::<f64>().ok())
    };
    let gib = |b: f64| format!("{:.1}G", b / (1u64 << 30) as f64);
    let mut parts = Vec::new();
    if let Some(total) = get("total").filter(|t| *t > 0.0) {
        let used = get("upload").unwrap_or(0.0) + get("download").unwrap_or(0.0);
        parts.push(format!("used {}/{}", gib(used), gib(total)));
    }
    if let Some(exp) = get("expire").filter(|e| *e > 0.0) {
        let days = (exp as i64 - state::now_unix() as i64) / 86400;
        parts.push(format!("expires in {days}d"));
    }
    if parts.is_empty() { raw.to_string() } else { parts.join(", ") }
}
