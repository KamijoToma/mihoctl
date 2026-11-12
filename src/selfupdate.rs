//! `mihoctl self-update`: replace the running binary with a GitHub release build.
//!
//! The asset names, the archive layout and `SHA256SUMS.txt` are a contract with
//! `.github/workflows/release.yml`: `mihoctl-<tag>-<target>.tar.gz` holds `mihoctl-<tag>-<target>/mihoctl`.
//! Only the user-owned binary is replaced. The root core runs its own root-owned copy of mihomo and never
//! executes mihoctl, so nothing here widens the privilege split.

use anyhow::{Context, Result, anyhow, bail};
use semver::Version;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

const REPO: &str = "KamijoToma/mihoctl";
const BIN: &str = "mihoctl";
const CHECKSUMS: &str = "SHA256SUMS.txt";
const MAX_SUMS: u64 = 1 << 20;
const MAX_ARCHIVE: u64 = 64 << 20;
const MAX_BINARY: u64 = 128 << 20;

/// Where releases come from. Tests point it at a local server.
#[derive(Clone)]
struct Github {
    api: String,
    web: String,
}

impl Github {
    fn official() -> Self {
        Self { api: "https://api.github.com".into(), web: "https://github.com".into() }
    }
}

struct Release {
    tag: String,
    version: Version,
    assets: Vec<String>,
}

/// The release build this platform runs. Linux builds are static musl, so they also serve glibc systems.
fn target_for(os: &str, arch: &str) -> Option<&'static str> {
    Some(match (os, arch) {
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("linux", "aarch64") => "aarch64-unknown-linux-musl",
        ("linux", "x86_64") => "x86_64-unknown-linux-musl",
        ("freebsd", "x86_64") => "x86_64-unknown-freebsd",
        _ => return None,
    })
}

/// `v1.2.3` or `1.2.3` to the release tag. Anything that is not a semantic version is rejected, which
/// also guarantees the tag is safe to put into a URL path.
fn tag_version(raw: &str) -> Result<(String, Version)> {
    let bare = raw.strip_prefix('v').unwrap_or(raw);
    let version = Version::parse(bare).with_context(|| format!("{raw:?} is not a release version like 1.2.3"))?;
    Ok((format!("v{bare}"), version))
}

fn parse_release(doc: &Value) -> Result<Release> {
    let raw = doc["tag_name"].as_str().context("release has no tag_name")?;
    let (tag, version) = tag_version(raw)?;
    let assets = doc["assets"]
        .as_array()
        .map(|a| a.iter().filter_map(|a| a["name"].as_str()).map(str::to_string).collect())
        .unwrap_or_default();
    Ok(Release { tag, version, assets })
}

/// The digest `SHA256SUMS.txt` lists for `name` (`<hex>  <name>` per line, `*` marks binary mode).
fn checksum_for(sums: &str, name: &str) -> Result<String> {
    for line in sums.lines() {
        let Some((digest, file)) = line.split_once(char::is_whitespace) else { continue };
        if file.trim_start().trim_start_matches('*') != name {
            continue;
        }
        if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("{CHECKSUMS} has a malformed digest for {name}");
        }
        return Ok(digest.to_ascii_lowercase());
    }
    bail!("{CHECKSUMS} does not list {name}")
}

/// The regular file at exactly `path` in a `.tar.gz`. Everything else in the archive is ignored, and
/// nothing is unpacked to disk, so entry paths cannot escape anywhere.
fn extract(archive: &[u8], path: &str) -> Result<Vec<u8>> {
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(archive));
    for entry in tar.entries().context("reading the archive")? {
        let entry = entry.context("reading the archive")?;
        if !entry.header().entry_type().is_file() || entry.path()?.to_str() != Some(path) {
            continue;
        }
        let mut binary = Vec::new();
        entry.take(MAX_BINARY + 1).read_to_end(&mut binary).context("unpacking the binary")?;
        if binary.len() as u64 > MAX_BINARY {
            bail!("{path} in the archive is larger than {} MiB", MAX_BINARY >> 20);
        }
        return Ok(binary);
    }
    bail!("{path} not found in the archive")
}

/// Stage `binary` beside `exe`, check that it runs and reports `expected`, then rename it over `exe`.
/// The rename gives the new binary a fresh inode, so a running copy keeps working and macOS never
/// sees a signed binary rewritten in place.
fn replace(exe: &Path, binary: &[u8], expected: &Version) -> Result<()> {
    let dir = exe.parent().context("executable has no parent directory")?;
    let staged = dir.join(format!(".{}.update", exe.file_name().and_then(|n| n.to_str()).unwrap_or(BIN)));
    let result = (|| -> Result<()> {
        {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o755)
                .open(&staged)
                .with_context(|| {
                    format!("writing {}; is {} writable by you? (reinstall mihoctl there, or fix its owner)", staged.display(), dir.display())
                })?;
            f.set_permissions(std::fs::Permissions::from_mode(0o755))?;
            f.write_all(binary)?;
            f.sync_all()?;
        }
        let out = Command::new(&staged).arg("--version").output().context("running the downloaded binary")?;
        let said = String::from_utf8_lossy(&out.stdout);
        if !out.status.success() || said.trim() != format!("{BIN} {expected}") {
            bail!("the downloaded binary does not report {BIN} {expected} (got {:?}); not installing it", said.trim());
        }
        std::fs::rename(&staged, exe).with_context(|| format!("replacing {}", exe.display()))?;
        std::fs::File::open(dir)?.sync_all().context("syncing containing directory")
    })();
    if result.is_err() {
        std::fs::remove_file(&staged).ok();
    }
    result
}

struct Client {
    agent: ureq::Agent,
    github: Github,
}

impl Client {
    fn new(github: Github, proxy: Option<ureq::Proxy>) -> Self {
        let agent = ureq::Agent::config_builder()
            .proxy(proxy)
            .user_agent(concat!("mihoctl/", env!("CARGO_PKG_VERSION")))
            .https_only(github.api.starts_with("https://"))
            .http_status_as_error(false)
            .timeout_connect(Some(Duration::from_secs(10)))
            // A proxy whose node is dead accepts the connection and then says nothing.
            .timeout_recv_response(Some(Duration::from_secs(20)))
            .timeout_global(Some(Duration::from_secs(300)))
            .build()
            .into();
        Self { agent, github }
    }

    fn fetch(&self, url: &str, api: bool, limit: u64) -> Result<(u16, Vec<u8>)> {
        let mut req = self.agent.get(url);
        if api {
            req = req.header("Accept", "application/vnd.github+json").header("X-GitHub-Api-Version", "2022-11-28");
        }
        let mut resp = req.call().with_context(|| format!("GET {url}"))?;
        let status = resp.status().as_u16();
        let body = resp.body_mut().with_config().limit(limit).read_to_vec().with_context(|| format!("reading {url}"))?;
        Ok((status, body))
    }

    /// The latest published release (GitHub leaves out drafts and prereleases), or the one tagged `tag`.
    fn release(&self, tag: Option<&str>) -> Result<Release> {
        let path = match tag {
            Some(tag) => format!("tags/{tag}"),
            None => "latest".into(),
        };
        let (status, body) = self.fetch(&format!("{}/repos/{REPO}/releases/{path}", self.github.api), true, MAX_SUMS)?;
        match status {
            200 => {}
            404 => bail!("{}", tag.map_or_else(|| "no release has been published yet".into(), |t| format!("release {t} does not exist"))),
            403 | 429 => bail!("GitHub API rate limit reached; try again later or use --proxy"),
            other => bail!("GitHub API answered HTTP {other}"),
        }
        parse_release(&serde_json::from_slice(&body).context("parsing the release")?)
    }

    fn asset(&self, release: &Release, name: &str, limit: u64) -> Result<Vec<u8>> {
        if !release.assets.iter().any(|a| a == name) {
            bail!("release {} has no asset {name}", release.tag);
        }
        let url = format!("{}/{REPO}/releases/download/{}/{name}", self.github.web, release.tag);
        match self.fetch(&url, false, limit)? {
            (200, body) => Ok(body),
            (status, _) => bail!("downloading {name}: HTTP {status}"),
        }
    }
}

/// Look the release up, trying each route in order. Only transport failures move on to the next
/// route; an answer from GitHub (even an error) is final.
fn lookup(github: &Github, routes: Vec<(&'static str, Option<ureq::Proxy>)>, tag: Option<&str>) -> Result<(Client, Release)> {
    let mut errors = Vec::new();
    for (label, proxy) in routes {
        let client = Client::new(github.clone(), proxy);
        match client.release(tag) {
            Ok(release) => return Ok((client, release)),
            Err(e) if e.downcast_ref::<ureq::Error>().is_some() => errors.push(format!("{label}: {e:#}")),
            Err(e) => return Err(e),
        }
    }
    bail!("could not reach GitHub: {}", errors.join("; "))
}

/// An explicit `--proxy`, else the running core's mixed port and then the environment's proxy settings.
fn routes(proxy: Option<&str>, core_proxy: Option<String>) -> Result<Vec<(&'static str, Option<ureq::Proxy>)>> {
    if let Some(p) = proxy {
        return Ok(vec![("via --proxy", Some(ureq::Proxy::new(p)?))]);
    }
    let mut routes = Vec::new();
    if let Some(p) = core_proxy {
        routes.push(("via core", Some(ureq::Proxy::new(&p)?)));
    }
    routes.push(("direct", ureq::Proxy::try_from_env()));
    Ok(routes)
}

pub fn run(check: bool, version: Option<&str>, proxy: Option<&str>, core_proxy: Option<String>) -> Result<()> {
    use std::env::consts::{ARCH, OS};
    let target = target_for(OS, ARCH).ok_or_else(|| {
        anyhow!("no prebuilt mihoctl for {OS}/{ARCH}; build from source: cargo install --git https://github.com/{REPO}")
    })?;
    let wanted = version.map(tag_version).transpose()?;
    let exe = std::env::current_exe()?.canonicalize().context("locating the running executable")?;
    update(&Github::official(), routes(proxy, core_proxy)?, target, wanted.map(|(tag, _)| tag).as_deref(), check, &exe)
}

fn update(
    github: &Github,
    routes: Vec<(&'static str, Option<ureq::Proxy>)>,
    target: &str,
    tag: Option<&str>,
    check: bool,
    exe: &Path,
) -> Result<()> {
    let current = Version::parse(env!("CARGO_PKG_VERSION")).context("own version")?;
    let (client, release) = lookup(github, routes, tag)?;

    // Plain `self-update` only ever moves forward; naming a version is an explicit up- or downgrade.
    if release.version == current || (tag.is_none() && release.version < current) {
        let note = if release.version == current { "" } else { " (newer than the latest release)" };
        eprintln!("mihoctl {current} is up to date{note}; latest release is {}", release.tag);
        return Ok(());
    }
    let verb = if release.version > current { "upgrade" } else { "downgrade" };
    eprintln!("{verb}: mihoctl {current} -> {} ({target})", release.version);
    eprintln!("release notes: {}/{REPO}/releases/tag/{}", github.web, release.tag);
    if check {
        eprintln!("run `mihoctl self-update{}` to install it", tag.map(|t| format!(" --version {t}")).unwrap_or_default());
        return Ok(());
    }

    let stem = format!("{BIN}-{}-{target}", release.tag);
    let name = format!("{stem}.tar.gz");
    if !release.assets.contains(&name) {
        bail!("release {} has no build for {target}; build from source: cargo install --git https://github.com/{REPO}", release.tag);
    }
    let sums = client.asset(&release, CHECKSUMS, MAX_SUMS)?;
    let expected = checksum_for(&String::from_utf8_lossy(&sums), &name)?;
    eprintln!("downloading {name}");
    let archive = client.asset(&release, &name, MAX_ARCHIVE)?;
    let actual = format!("{:x}", Sha256::digest(&archive));
    if actual != expected {
        bail!("{name} does not match {CHECKSUMS} (expected {expected}, got {actual}); not installing it");
    }
    let binary = extract(&archive, &format!("{stem}/{BIN}"))?;
    replace(exe, &binary, &release.version)?;
    eprintln!("installed mihoctl {} at {}", release.version, exe.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;
    use std::net::TcpListener;
    use std::path::PathBuf;

    const TAG: &str = "v9.9.9";
    const TARGET: &str = "x86_64-unknown-linux-musl";

    fn scratch(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mihoctl-selfupdate-{label}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn script(version: &str) -> Vec<u8> {
        format!("#!/bin/sh\necho 'mihoctl {version}'\n").into_bytes()
    }

    /// `.tar.gz` with `files` (path, contents), like the one the release workflow packs.
    fn archive(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast()));
        for (path, contents) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_mode(0o755);
            tar.append_data(&mut header, path, *contents).unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap()
    }

    fn release_archive(version: &str) -> Vec<u8> {
        let stem = format!("mihoctl-{TAG}-{TARGET}");
        archive(&[(&format!("{stem}/README.md"), b"readme"), (&format!("{stem}/{BIN}"), &script(version))])
    }

    /// Serves `routes` (path, body) on a loopback port until the test process exits.
    fn serve(routes: Vec<(String, Vec<u8>)>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut line = String::new();
                std::io::BufReader::new(&stream).read_line(&mut line).ok();
                let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
                let mut stream = stream;
                let (status, body) = match routes.iter().find(|(p, _)| *p == path) {
                    Some((_, body)) => ("200 OK", body.as_slice()),
                    None => ("404 Not Found", &b"{}"[..]),
                };
                let head = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                stream.write_all(head.as_bytes()).and_then(|_| stream.write_all(body)).ok();
            }
        });
        base
    }

    fn release_server(archive: &[u8], sums: Option<String>) -> Github {
        let name = format!("mihoctl-{TAG}-{TARGET}.tar.gz");
        let sums = sums.unwrap_or_else(|| format!("{:x}  {name}\n{:x}  other.zip\n", Sha256::digest(archive), Sha256::digest(b"x")));
        let api = serde_json::json!({
            "tag_name": TAG,
            "assets": [{ "name": name }, { "name": CHECKSUMS }],
        });
        let base = serve(vec![
            (format!("/repos/{REPO}/releases/latest"), api.to_string().into_bytes()),
            (format!("/repos/{REPO}/releases/tags/{TAG}"), api.to_string().into_bytes()),
            (format!("/{REPO}/releases/download/{TAG}/{name}"), archive.to_vec()),
            (format!("/{REPO}/releases/download/{TAG}/{CHECKSUMS}"), sums.into_bytes()),
        ]);
        Github { api: base.clone(), web: base }
    }

    fn direct() -> Vec<(&'static str, Option<ureq::Proxy>)> {
        vec![("direct", None)]
    }

    fn installed(dir: &Path) -> PathBuf {
        let exe = dir.join(BIN);
        std::fs::write(&exe, script("0.0.1")).unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        exe
    }

    fn says(exe: &Path) -> String {
        String::from_utf8(Command::new(exe).output().unwrap().stdout).unwrap().trim().to_string()
    }

    #[test]
    fn targets_match_the_release_matrix() {
        assert_eq!(target_for("macos", "aarch64"), Some("aarch64-apple-darwin"));
        assert_eq!(target_for("macos", "x86_64"), Some("x86_64-apple-darwin"));
        assert_eq!(target_for("linux", "x86_64"), Some("x86_64-unknown-linux-musl"));
        assert_eq!(target_for("linux", "aarch64"), Some("aarch64-unknown-linux-musl"));
        assert_eq!(target_for("freebsd", "x86_64"), Some("x86_64-unknown-freebsd"));
        assert_eq!(target_for("linux", "riscv64"), None);
        assert_eq!(target_for("windows", "x86_64"), None);
    }

    #[test]
    fn tags_must_be_semantic_versions() {
        assert_eq!(tag_version("0.2.0").unwrap().0, "v0.2.0");
        assert_eq!(tag_version("v0.2.0-rc.1").unwrap().1, Version::parse("0.2.0-rc.1").unwrap());
        for bad in ["latest", "v1", "v1.2", "v1.2.3/../../x", "../v1.2.3", "v1.2.3?x=1", ""] {
            assert!(tag_version(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn release_lists_asset_names() {
        let doc = serde_json::json!({ "tag_name": "v1.2.3", "assets": [{ "name": "a.tar.gz" }, { "name": CHECKSUMS }] });
        let release = parse_release(&doc).unwrap();
        assert_eq!((release.tag.as_str(), release.version.to_string()), ("v1.2.3", "1.2.3".to_string()));
        assert_eq!(release.assets, ["a.tar.gz", CHECKSUMS]);
        assert!(parse_release(&serde_json::json!({ "tag_name": "nightly" })).is_err());
        assert!(parse_release(&serde_json::json!({})).is_err());
    }

    #[test]
    fn checksum_lookup_is_exact() {
        let digest = "ab".repeat(32);
        let sums = format!("{digest}  mihoctl-v1.0.0-a.tar.gz\n{} *mihoctl-v1.0.0-b.tar.gz\n", "CD".repeat(32));
        assert_eq!(checksum_for(&sums, "mihoctl-v1.0.0-a.tar.gz").unwrap(), digest);
        assert_eq!(checksum_for(&sums, "mihoctl-v1.0.0-b.tar.gz").unwrap(), "cd".repeat(32));
        assert!(checksum_for(&sums, "mihoctl-v1.0.0.tar.gz").is_err());
        assert!(checksum_for("abcd  x.tar.gz\n", "x.tar.gz").is_err(), "short digest");
        assert!(checksum_for(&format!("{}  x.tar.gz\n", "zz".repeat(32)), "x.tar.gz").is_err(), "non-hex digest");
    }

    #[test]
    fn extract_takes_only_the_exact_binary_path() {
        let tgz = archive(&[
            ("pkg/._mihoctl", b"appledouble"),
            ("pkg/mihoctl.bak", b"backup"),
            ("other/mihoctl", b"wrong dir"),
            ("pkg/mihoctl", b"the binary"),
        ]);
        assert_eq!(extract(&tgz, "pkg/mihoctl").unwrap(), b"the binary");
        assert!(extract(&tgz, "pkg/missing").is_err());
        assert!(extract(b"not gzip", "pkg/mihoctl").is_err());
    }

    #[test]
    fn replace_swaps_in_a_binary_that_reports_the_version() {
        let dir = scratch("replace-ok");
        let exe = installed(&dir);
        replace(&exe, &script("9.9.9"), &Version::new(9, 9, 9)).unwrap();
        let (said, mode, left) = (
            says(&exe),
            std::fs::metadata(&exe).unwrap().permissions().mode() & 0o777,
            std::fs::read_dir(&dir).unwrap().count(),
        );
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!((said.as_str(), mode, left), ("mihoctl 9.9.9", 0o755, 1));
    }

    #[test]
    fn replace_refuses_a_binary_that_does_not_report_the_version() {
        let dir = scratch("replace-bad");
        let exe = installed(&dir);
        let wrong = replace(&exe, &script("1.0.0"), &Version::new(9, 9, 9));
        let garbage = replace(&exe, b"not an executable", &Version::new(9, 9, 9));
        let (said, left) = (says(&exe), std::fs::read_dir(&dir).unwrap().count());
        std::fs::remove_dir_all(&dir).ok();
        assert!(wrong.is_err() && garbage.is_err());
        assert_eq!((said.as_str(), left), ("mihoctl 0.0.1", 1), "old binary kept, staging file removed");
    }

    #[test]
    fn update_downloads_verifies_and_installs() {
        let dir = scratch("e2e");
        let exe = installed(&dir);
        let github = release_server(&release_archive("9.9.9"), None);
        let checked = update(&github, direct(), TARGET, None, true, &exe);
        let untouched = says(&exe);
        let installed_run = update(&github, direct(), TARGET, None, false, &exe);
        let said = says(&exe);
        std::fs::remove_dir_all(&dir).ok();
        checked.unwrap();
        assert_eq!(untouched, "mihoctl 0.0.1", "--check must not install");
        installed_run.unwrap();
        assert_eq!(said, "mihoctl 9.9.9");
    }

    #[test]
    fn update_by_tag_installs_that_release() {
        let dir = scratch("by-tag");
        let exe = installed(&dir);
        let github = release_server(&release_archive("9.9.9"), None);
        let result = update(&github, direct(), TARGET, Some(TAG), false, &exe);
        let said = says(&exe);
        std::fs::remove_dir_all(&dir).ok();
        result.unwrap();
        assert_eq!(said, "mihoctl 9.9.9");
    }

    #[test]
    fn update_rejects_an_archive_that_fails_its_checksum() {
        let dir = scratch("tampered");
        let exe = installed(&dir);
        let name = format!("mihoctl-{TAG}-{TARGET}.tar.gz");
        let github = release_server(&release_archive("9.9.9"), Some(format!("{}  {name}\n", "00".repeat(32))));
        let result = update(&github, direct(), TARGET, None, false, &exe);
        let said = says(&exe);
        std::fs::remove_dir_all(&dir).ok();
        assert!(format!("{:#}", result.unwrap_err()).contains("does not match"));
        assert_eq!(said, "mihoctl 0.0.1");
    }

    #[test]
    fn update_rejects_a_missing_platform_build() {
        let dir = scratch("no-asset");
        let exe = installed(&dir);
        let github = release_server(&release_archive("9.9.9"), None);
        let result = update(&github, direct(), "aarch64-apple-darwin", None, false, &exe);
        std::fs::remove_dir_all(&dir).ok();
        assert!(format!("{:#}", result.unwrap_err()).contains("has no build for"));
    }

    #[test]
    fn up_to_date_installs_nothing() {
        let dir = scratch("current");
        let exe = installed(&dir);
        let own = env!("CARGO_PKG_VERSION");
        let doc = serde_json::json!({ "tag_name": format!("v{own}"), "assets": [] });
        let base = serve(vec![(format!("/repos/{REPO}/releases/latest"), doc.to_string().into_bytes())]);
        let github = Github { api: base.clone(), web: base };
        let result = update(&github, direct(), TARGET, None, false, &exe);
        let said = says(&exe);
        std::fs::remove_dir_all(&dir).ok();
        result.unwrap();
        assert_eq!(said, "mihoctl 0.0.1");
    }

    #[test]
    fn unknown_release_is_a_github_answer_not_a_transport_failure() {
        let github = Github { api: serve(Vec::new()), web: String::new() };
        // Two routes: a 404 must end the lookup instead of retrying on the second one.
        let err = lookup(&github, vec![("first", None), ("second", None)], Some(TAG)).err().unwrap();
        let msg = format!("{err:#}");
        assert!(msg.contains("does not exist") && !msg.contains("second"), "{msg}");
    }

    #[test]
    fn unreachable_routes_are_all_reported() {
        let dead = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        let github = Github { api: format!("http://{dead}"), web: String::new() };
        let err = lookup(&github, vec![("first", None), ("second", None)], None).err().unwrap();
        let msg = format!("{err:#}");
        assert!(msg.contains("first:") && msg.contains("second:"), "{msg}");
    }
}
