mod core;
mod history;
mod merge;
mod pipeline;
mod restore;
mod script;
mod selfupdate;
mod service;
mod settings;
mod state;
mod status;
mod subscription;
mod tailscale;
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
    /// List retained node versions (not historical runtime configurations)
    History { subscription: Option<String> },
    /// Restore one subscription's nodes using the current template and overrides
    Restore { subscription: String, version: Option<String> },
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
        /// Publish the dashboard on the tailnet with Tailscale Serve and print that URL
        #[arg(long)]
        tailscale: bool,
        /// Mount path on the tailnet; `/` claims the node's HTTPS root
        /// (defaults to `tailscale.path` in mihoctl.toml)
        #[arg(long)]
        path: Option<String>,
        /// The tailnet HTTPS port to publish on (defaults to `tailscale.port`)
        #[arg(long)]
        port: Option<u16>,
        /// Remove the tailnet publication instead of creating it
        #[arg(long)]
        unpublish: bool,
    },
    /// Replace this binary with the latest GitHub release (checksum-verified)
    SelfUpdate {
        /// Only report whether a newer release exists
        #[arg(long)]
        check: bool,
        /// Install this release (e.g. 0.2.0) instead of the latest; may be older than the current one
        #[arg(long)]
        version: Option<String>,
        /// Proxy for GitHub instead of the running core's mixed port, e.g. http://127.0.0.1:7897
        #[arg(long)]
        proxy: Option<String>,
    },
    /// Install/remove the service units (launchd on macOS, systemd on Linux, rc.d on FreeBSD)
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
    /// Print the generated units without installing
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
    // Needs no valid settings (a broken config is a reason to update), only a proxy hint from them.
    if let Cmd::SelfUpdate { check, version, proxy } = &cli.cmd {
        let core_proxy = settings::load(&file).ok().and_then(|(settings, _)| core_mixed_proxy(&settings));
        return selfupdate::run(*check, version.as_deref(), proxy.as_deref(), core_proxy);
    }
    let (settings, paths) = settings::load(&file)?;
    std::fs::create_dir_all(&paths.providers_dir)
        .with_context(|| format!("creating {}", paths.providers_dir.display()))?;

    match cli.cmd {
        Cmd::History { subscription } => {
            let _lock = state::lock(&paths.lock_file)?;
            history::print(&settings, &paths, subscription.as_deref())?;
        }
        Cmd::Restore { subscription, version } => {
            let _lock = state::lock(&paths.lock_file)?;
            let restored = restore::run(&settings, &paths, &subscription, version.as_deref())?;
            match restored.applied {
                Applied::Reloaded { alive } => eprintln!(
                    "restored nodes for {subscription} ({}); reloaded; {alive} alive in probe group {:?} (group scope, not provider health)",
                    restored.hash, settings.health_check.group
                ),
                Applied::Installed => eprintln!(
                    "restored nodes for {subscription} ({}); installed for next start (core not running; not probed)",
                    restored.hash
                ),
                Applied::Unchanged => unreachable!("restore always installs and probes when running"),
            }
        }
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
            // Build also shares the candidate with restore/apply/diff.
            let _lock = state::lock(&paths.lock_file)?;
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
                    eprintln!("no changes to {}; apply would do nothing", paths.config().display());
                } else {
                    eprintln!("only masked values changed (secret or node details); apply would reload the core");
                }
            }
        }
        Cmd::Status { json } => {
            let s = status::gather(&settings, &paths)?;
            if json { status::print_json(&s)? } else { status::print_text(&s) }
        }
        Cmd::Ui { update, tailscale, path, port, unpublish } => {
            let ctl = core::Controller::new(&settings.controller, &settings.secret);
            if update {
                ctl.update_ui()?;
            }
            if tailscale {
                let path = path.unwrap_or_else(|| settings.tailscale.path.clone());
                let port = port.unwrap_or(settings.tailscale.port);
                ui_tailscale(&settings, &path, port, unpublish)?;
            } else {
                let (host, port) =
                    settings.controller.rsplit_once(':').context("controller must be host:port")?;
                println!(
                    "http://{}/ui/#/setup?hostname={}&port={}&secret={}",
                    settings.controller,
                    core::encode(host),
                    port,
                    core::encode(&settings.secret)
                );
            }
        }
        Cmd::SelfUpdate { .. } => unreachable!("handled before the settings are loaded"),
        Cmd::Service { action } => match action {
            ServiceAction::Install => {
                service::install(&settings, &paths)?;
                eprintln!("installed {} and {}", service::CORE_NAME, service::REFRESH_NAME);
            }
            ServiceAction::Uninstall => {
                service::uninstall()?;
                eprintln!("removed {} units", service::MANAGER);
            }
            ServiceAction::Restart => service::restart_core()?,
            ServiceAction::Print => {
                for (path, contents) in service::units(&settings, &paths)? {
                    eprintln!("--- {}", path.display());
                    println!("{contents}");
                }
            }
        },
    }
    Ok(())
}

/// Publish (or unpublish) the dashboard on the tailnet and print the URL to open.
///
/// The core's controller stays on loopback; Tailscale Serve is the reverse proxy in front of it,
/// terminating TLS on the node's MagicDNS name. The dashboard is opened through that proxy, so the
/// URL points at the tailnet host and carries the mount prefix as the API's secondary path; the
/// secret travels in the fragment, which the browser never sends to the server.
fn ui_tailscale(settings: &Settings, path: &str, port: u16, unpublish: bool) -> Result<()> {
    let mount = tailscale::Mount::new(path);
    let node = tailscale::node()?;

    if unpublish {
        tailscale::unserve(&node, &mount, port)?;
        eprintln!("removed the tailnet publication at {}", node.dns_name);
        return Ok(());
    }

    let target = controller_target(settings)?;
    let served = tailscale::serve(&node, &target, &mount, port)?;
    let prefix = if served.path == "/" { "" } else { &served.path };
    println!(
        "{}/ui/#/setup?protocol=https&hostname={}&port={}&secondaryPath={}&secret={}",
        served.url,
        core::encode(&node.dns_name),
        port,
        core::encode(prefix),
        core::encode(&settings.secret)
    );
    Ok(())
}

/// The loopback address the tailnet proxy forwards to.
fn controller_target(settings: &Settings) -> Result<String> {
    let (host, port) = settings.controller.rsplit_once(':').context("controller must be host:port")?;
    // Tailscale Serve proxies to loopback only. `localhost` is used for an IPv6 controller because
    // the proxy resolves it to whichever family the core bound.
    let host = match host {
        "localhost" | "127.0.0.1" => "127.0.0.1",
        "[::1]" => "localhost",
        other => bail!(
            "controller host {other:?} is not a loopback address; Tailscale Serve can only proxy to loopback"
        ),
    };
    Ok(format!("http://{host}:{port}"))
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
        Applied::Unchanged => eprintln!("config unchanged"),
    }
    Ok(())
}
