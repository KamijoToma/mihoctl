//! Expose the core's dashboard to the tailnet with Tailscale Serve.
//!
//! The core keeps its controller on loopback (see `pipeline::enforce`), so the only way to reach it
//! from another machine is a reverse proxy that terminates TLS on the tailnet. Tailscale Serve does
//! exactly that: `tailscale serve --bg <target>` publishes the local port over the node's MagicDNS
//! name, with a real certificate, reachable only by tailnet members.
//!
//! Why the `tailscale` CLI and not a Go library: Tailscale's own client for this is
//! `tailscale.com/client/local` (`GetServeConfig` / `SetServeConfig`), whose Go API is explicitly
//! marked unstable. The Rust crates around it are no better a foundation: `tailscale-localapi` has
//! no serve-config API at all, and `tslocal` has one but drags in a full async runtime. The CLI is
//! the supported interface, already installed wherever Tailscale runs, and it owns the tricky
//! parts: MagicDNS name discovery, certificate provisioning, ETag-guarded config updates, and the
//! foreground/background distinction.
//!
//! Two transports matter, and the CLI hides the difference:
//! - Linux/FreeBSD: the `tailscaled` unix socket, where the caller's uid decides read/write.
//! - macOS: the App Store / system-extension build has no unix socket; `tailscaled` listens on a
//!   loopback TCP port and authenticates with a per-user token (`/Library/Tailscale/ipnport` +
//!   `sameuserproof-<port>`). The CLI handles this itself, so mihoctl never parses those files.

use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::process::{Command, Stdio};

/// The `tailscale` binary. The macOS App Store build installs a stub to `/usr/local/bin/tailscale`
/// that execs the app's own binary; other platforms install it on `PATH`.
const CLI: &str = "tailscale";

/// Where the dashboard is published on the tailnet.
///
/// A prefix keeps the node's HTTPS root free for other services and lets several dashboards share
/// one node; the root mount is simpler but claims `/` for the whole node. Tailscale strips the
/// prefix before forwarding, so the dashboard and its API keep their normal paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mount {
    /// Publish at `/` (the whole node's HTTPS root).
    Root,
    /// Publish under a path prefix, e.g. `/mihoctl`.
    Prefix(String),
}

impl Mount {
    /// A mount for the given path, normalized to an absolute path without a trailing slash.
    pub fn new(path: &str) -> Mount {
        let trimmed = path.trim().trim_matches('/');
        if trimmed.is_empty() { Mount::Root } else { Mount::Prefix(format!("/{trimmed}")) }
    }

    /// The path as the tailnet sees it, and as `tailscale serve --set-path` spells it.
    pub fn path(&self) -> &str {
        match self {
            Mount::Root => "/",
            Mount::Prefix(p) => p,
        }
    }
}

/// The node's identity, as reported by `tailscale status --json`.
#[derive(Debug, Clone)]
pub struct Node {
    /// MagicDNS name without the trailing dot.
    pub dns_name: String,
}

/// A published dashboard.
#[derive(Debug, Clone)]
pub struct Served {
    /// The mount path, e.g. `/` or `/mihoctl`.
    pub path: String,
    /// The tailnet-visible base URL of the mount.
    pub url: String,
}

/// The local node's identity. Fails when Tailscale is not installed, not running, or logged out.
pub fn node() -> Result<Node> {
    let out = cli(&["status", "--json"])?;
    let v: Value = serde_json::from_str(&out).context("parsing `tailscale status --json`")?;
    let backend = v["BackendState"].as_str().unwrap_or("");
    if backend != "Running" {
        bail!("Tailscale is not running (backend state {backend:?}); start it and log in first");
    }
    let dns_name = v["Self"]["DNSName"].as_str().unwrap_or("").trim_end_matches('.').to_string();
    if dns_name.is_empty() {
        bail!("this Tailscale node has no MagicDNS name; HTTPS serving needs one (enable MagicDNS for the tailnet)");
    }
    Ok(Node { dns_name })
}

/// The base URL the tailnet reaches this node on.
pub fn base_url(dns_name: &str) -> String {
    format!("https://{dns_name}")
}

/// Publish the dashboard: reverse-proxy `target` over the node's MagicDNS name.
///
/// Re-running this is safe: it replaces the handler at the same mount, leaving the node's other
/// Serve configuration alone.
pub fn serve(node: &Node, target: &str, mount: &Mount, port: u16) -> Result<Served> {
    let args = serve_args(target, mount, port);
    run(&args)?;

    // The CLI reports success before the daemon has necessarily published the mount; confirm it,
    // so the URL mihoctl prints is one that actually resolves.
    if !mounts(node, port)?.iter().any(|(path, _)| path == mount.path()) {
        bail!("`{CLI} {}` reported success but the mount {:?} is not published", args.join(" "), mount.path());
    }
    Ok(Served { path: mount.path().to_string(), url: mount_url(&node.dns_name, mount, port) })
}

/// Stop publishing the dashboard. Removes only mihoctl's own mount; other Serve configuration on
/// the same node (other services, TCP forwards) is left alone.
pub fn unserve(node: &Node, mount: &Mount, port: u16) -> Result<()> {
    let args = off_args(mount, port);
    match run(&args) {
        Ok(_) => {}
        Err(e) => {
            // The CLI fails when the mount does not exist. That is the desired end state, so treat
            // it as success rather than an error the user has to read past.
            if mounts(node, port)?.iter().any(|(path, _)| path == mount.path()) {
                return Err(e).context(format!("stopping the tailnet proxy with `{}`", args.join(" ")));
            }
        }
    }
    Ok(())
}

/// The web mounts currently published for this node on `port`, as `(path, proxy target)` pairs.
pub fn mounts(node: &Node, port: u16) -> Result<Vec<(String, String)>> {
    let out = cli(&["serve", "status", "--json"])?;
    parse_mounts(&out, &node.dns_name, port)
}

/// The web mounts in a `tailscale serve status --json` document, as `(path, proxy target)` pairs.
fn parse_mounts(json: &str, dns_name: &str, port: u16) -> Result<Vec<(String, String)>> {
    let v: Value = serde_json::from_str(json).context("parsing `tailscale serve status --json`")?;
    let host = format!("{dns_name}:{port}");
    let Some(handlers) = v["Web"][&host]["Handlers"].as_object() else {
        return Ok(Vec::new());
    };
    let mut out: Vec<(String, String)> = handlers
        .iter()
        .filter_map(|(path, h)| {
            let proxy = h.get("Proxy").and_then(Value::as_str)?;
            Some((path.clone(), proxy.to_string()))
        })
        .collect();
    out.sort();
    Ok(out)
}

/// The tailnet URL of a mount.
fn mount_url(dns_name: &str, mount: &Mount, port: u16) -> String {
    let base = if port == 443 { base_url(dns_name) } else { format!("https://{dns_name}:{port}") };
    match mount {
        Mount::Root => base,
        Mount::Prefix(p) => format!("{base}{p}"),
    }
}

/// `tailscale serve --bg --https=PORT --set-path P <target>`.
///
/// `--set-path` is always passed, even for the root mount: without it an `off` invocation clears
/// *every* web mount on the node, not just mihoctl's.
fn serve_args(target: &str, mount: &Mount, port: u16) -> Vec<String> {
    vec![
        "serve".into(),
        "--bg".into(),
        format!("--https={port}"),
        "--set-path".into(),
        mount.path().into(),
        target.into(),
    ]
}

/// `tailscale serve --https=PORT --set-path P off`.
fn off_args(mount: &Mount, port: u16) -> Vec<String> {
    vec!["serve".into(), format!("--https={port}"), "--set-path".into(), mount.path().into(), "off".into()]
}

/// Run the Tailscale CLI and return its stdout. The CLI writes diagnostics to stderr, so a
/// failure is reported with stderr attached rather than swallowed.
fn cli(args: &[&str]) -> Result<String> {
    let out = Command::new(CLI)
        .args(args)
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                anyhow::anyhow!("the `{CLI}` binary is not on PATH; install Tailscale to publish the dashboard on the tailnet")
            } else {
                anyhow::anyhow!("running `{CLI} {}`: {e}", args.join(" "))
            }
        })?;
    finish(out, args)
}

/// Run the CLI, retrying under `sudo` when the daemon refuses the call.
///
/// On Linux the `tailscaled` unix socket grants write access to root, to the operator user
/// (`tailscale set --operator=$USER`) and to nobody else; on a stock install that means only root
/// can publish a Serve config. Tailscale's own error names both escape hatches, so mihoctl takes
/// the same route rather than leaving the user with a wall of text.
fn run(args: &[String]) -> Result<String> {
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    match cli(&refs) {
        Ok(out) => Ok(out),
        Err(e) if is_access_denied(&e.to_string()) => {
            sudo_cli(&refs).with_context(|| format!("`{CLI} {}` was refused; retrying as root", refs.join(" ")))
        }
        Err(e) => Err(e),
    }
}

/// Tailscale's access-denied marker. The daemon answers `403 serve config denied` and the CLI
/// turns that into its "Use 'sudo tailscale ...'" hint.
fn is_access_denied(msg: &str) -> bool {
    msg.to_lowercase().contains("denied") || msg.contains("Use 'sudo tailscale")
}

/// Run the CLI as root, with one password prompt.
///
/// stderr is inherited so sudo's prompt reaches the terminal; stdout is captured because it
/// carries the "Available within your tailnet" summary rather than anything the user must read.
/// `SUDO_ASKPASS` is honoured, like the service units do, so a headless run can supply the
/// password out of band.
fn sudo_cli(args: &[&str]) -> Result<String> {
    let mut cmd = Command::new("sudo");
    if std::env::var_os("SUDO_ASKPASS").is_some() {
        cmd.arg("-A");
    }
    cmd.arg(CLI).args(args);
    let child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("running `sudo {CLI} {}`", args.join(" ")))?;
    let out = child
        .wait_with_output()
        .with_context(|| format!("running `sudo {CLI} {}`", args.join(" ")))?;
    if !out.status.success() {
        bail!("`sudo {CLI} {}` failed: {}", args.join(" "), out.status);
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn finish(out: std::process::Output, args: &[&str]) -> Result<String> {
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    if out.status.success() {
        return Ok(stdout);
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let detail = if stderr.trim().is_empty() { stdout.trim().to_string() } else { stderr.trim().to_string() };
    bail!("`{CLI} {}` failed: {detail}", args.join(" "));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node() -> Node {
        Node { dns_name: "laptop.tailnet.ts.net".into() }
    }

    #[test]
    fn mount_paths_are_absolute() {
        assert_eq!(Mount::Root.path(), "/");
        assert_eq!(Mount::Prefix("/mihoctl".into()).path(), "/mihoctl");
    }

    #[test]
    fn mounts_are_normalized() {
        assert_eq!(Mount::new("/"), Mount::Root);
        assert_eq!(Mount::new(""), Mount::Root);
        assert_eq!(Mount::new("/"), Mount::Root);
        assert_eq!(Mount::new("/mihoctl/"), Mount::Prefix("/mihoctl".into()));
        assert_eq!(Mount::new("mihoctl"), Mount::Prefix("/mihoctl".into()));
    }

    #[test]
    fn base_url_is_https_on_the_magic_dns_name() {
        assert_eq!(base_url("laptop.tailnet.ts.net"), "https://laptop.tailnet.ts.net");
    }

    #[test]
    fn served_url_carries_the_mount_prefix() {
        let mount = Mount::Prefix("/mihoctl".into());
        let s = Served { path: mount.path().into(), url: mount_url(&node().dns_name, &mount, 443) };
        assert_eq!(s.url, "https://laptop.tailnet.ts.net/mihoctl");
        assert_eq!(s.path, "/mihoctl");
    }

    #[test]
    fn served_url_at_root_has_no_trailing_path() {
        let mount = Mount::Root;
        let s = Served { path: mount.path().into(), url: mount_url(&node().dns_name, &mount, 443) };
        assert_eq!(s.url, "https://laptop.tailnet.ts.net");
        assert_eq!(s.path, "/");
    }

    #[test]
    fn served_url_carries_a_non_default_port() {
        let mount = Mount::Prefix("/mihoctl".into());
        assert_eq!(mount_url(&node().dns_name, &mount, 8443), "https://laptop.tailnet.ts.net:8443/mihoctl");
    }

    #[test]
    fn serve_args_always_scope_the_mount() {
        // Without `--set-path`, `tailscale serve ... off` clears every web mount on the node.
        let args = serve_args("http://127.0.0.1:9090", &Mount::Root, 443);
        assert_eq!(args, ["serve", "--bg", "--https=443", "--set-path", "/", "http://127.0.0.1:9090"]);
    }

    #[test]
    fn off_args_target_the_same_mount() {
        assert_eq!(off_args(&Mount::Prefix("/mihoctl".into()), 443),
                   ["serve", "--https=443", "--set-path", "/mihoctl", "off"]);
        assert_eq!(off_args(&Mount::Root, 443), ["serve", "--https=443", "--set-path", "/", "off"]);
    }

    #[test]
    fn parses_the_mounts_of_this_node_only() {
        let json = r#"{
            "TCP": {"443": {"HTTPS": true}},
            "Web": {
                "laptop.tailnet.ts.net:443": {
                    "Handlers": {
                        "/": {"Proxy": "http://127.0.0.1:9090"},
                        "/mihoctl": {"Proxy": "http://127.0.0.1:9091"}
                    }
                },
                "other.tailnet.ts.net:443": {
                    "Handlers": {"/": {"Proxy": "http://127.0.0.1:1"}}
                }
            }
        }"#;
        let mounts = parse_mounts(json, "laptop.tailnet.ts.net", 443).unwrap();
        assert_eq!(mounts, [("/".to_string(), "http://127.0.0.1:9090".to_string()),
                             ("/mihoctl".to_string(), "http://127.0.0.1:9091".to_string())]);
    }

    #[test]
    fn a_node_without_web_mounts_has_none() {
        assert!(parse_mounts(r#"{"TCP":{"9333":{"TCPForward":"127.0.0.1:9333"}}}"#, "laptop.tailnet.ts.net", 443)
            .unwrap()
            .is_empty());
        assert!(parse_mounts(r#"{"Web":{"other.tailnet.ts.net:443":{"Handlers":{"/":{}}}}}"#, "laptop.tailnet.ts.net", 443)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn mounts_are_scoped_to_the_published_port() {
        let json = r#"{
            "Web": {
                "laptop.tailnet.ts.net:8443": {
                    "Handlers": {"/dash": {"Proxy": "http://127.0.0.1:9090"}}
                }
            }
        }"#;
        assert_eq!(parse_mounts(json, "laptop.tailnet.ts.net", 8443).unwrap().len(), 1);
        assert!(parse_mounts(json, "laptop.tailnet.ts.net", 443).unwrap().is_empty());
    }

    #[test]
    fn access_denied_is_recognised() {
        // What the daemon answers for a read-only connection, and what the CLI prints for it.
        assert!(is_access_denied("`tailscale serve ...` failed: serve config denied"));
        assert!(is_access_denied("serve config denied\n\nUse 'sudo tailscale serve ...'."));
        assert!(!is_access_denied("`tailscale serve ...` failed: handler does not exist"));
        assert!(!is_access_denied("`tailscale serve ...` failed: 404 Not Found"));
    }
}
