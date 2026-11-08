mod core;
mod merge;
mod pipeline;
mod script;
mod service;
mod settings;
mod state;
mod status;
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
    /// Build and validate the config, then show how it differs from the installed one
    Diff,
    /// Show core, controller, subscription and last-apply status
    Status {
        /// Machine-readable output (no subscription URLs or secret)
        #[arg(long)]
        json: bool,
    },
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
        Cmd::Diff => {
            // Shares config.candidate.yaml with apply/refresh.
            let _lock = state::lock(&paths.lock_file)?;
            let candidate = pipeline::build_checked(&settings, &paths)?;
            let changed = pipeline::diff(&paths.config(), &candidate);
            let identical = std::fs::read(paths.config()).ok() == std::fs::read(&candidate).ok();
            std::fs::remove_file(&candidate).ok();
            if !changed? {
                if identical {
                    eprintln!("no changes to {}; apply would only reload providers", paths.config().display());
                } else {
                    eprintln!("only the (masked) secret changed; apply would reload the core");
                }
            }
        }
        Cmd::Status { json } => {
            let s = status::gather(&settings, &paths)?;
            if json { status::print_json(&s)? } else { status::print_text(&s) }
        }
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
                println!("{}", service::core_plist(&settings, &paths)?);
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
