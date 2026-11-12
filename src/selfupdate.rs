//! `mihoctl self-update`: replace the running binary with a GitHub release build.
//!
//! Releases are found and downloaded through github.com's web endpoints, never the REST API: its
//! anonymous quota is 60 requests an hour per address, and people who need a proxy share an exit
//! address with strangers, so that quota is routinely spent before they ask.
//!
//! The asset names, the archive layout and `SHA256SUMS.txt` are a contract with
//! `.github/workflows/release.yml`: `mihoctl-<tag>-<target>.tar.gz` holds `mihoctl-<tag>-<target>/mihoctl`,
//! and the checksum file lists every asset, which is also how a missing platform build is noticed.
//! Only the user-owned binary is replaced. The root core runs its own root-owned copy of mihomo and never
//! executes mihoctl, so nothing here widens the privilege split.

use anyhow::{Context, Result, anyhow, bail};
use semver::Version;
use sha2::{Digest, Sha256};
use std::cell::Cell;
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

const WEB: &str = "https://github.com";
const REPO: &str = "KamijoToma/mihoctl";
const BIN: &str = "mihoctl";
const CHECKSUMS: &str = "SHA256SUMS.txt";
const MAX_SUMS: u64 = 1 << 20;
const MAX_ARCHIVE: u64 = 64 << 20;
const MAX_BINARY: u64 = 128 << 20;
const RATE_LIMITED: &str = "GitHub is limiting requests from this address; try again later or use --proxy";

type Route = (&'static str, Option<ureq::Proxy>);

struct Release {
    tag: String,
    version: Version,
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
fn release_of(raw: &str) -> Result<Release> {
    let bare = raw.strip_prefix('v').unwrap_or(raw);
    let version = Version::parse(bare).with_context(|| format!("{raw:?} is not a release version like 1.2.3"))?;
    Ok(Release { tag: format!("v{bare}"), version })
}

/// The digest `SHA256SUMS.txt` lists for `name` (`<hex>  <name>` per line, `*` marks binary mode);
/// `None` when the release does not contain that asset.
fn checksum_for(sums: &str, name: &str) -> Result<Option<String>> {
    for line in sums.lines() {
        let Some((digest, file)) = line.split_once(char::is_whitespace) else { continue };
        if file.trim_start().trim_start_matches('*') != name {
            continue;
        }
        if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("{CHECKSUMS} has a malformed digest for {name}");
        }
        return Ok(Some(digest.to_ascii_lowercase()));
    }
    Ok(None)
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

struct Reply {
    status: u16,
    location: Option<String>,
    body: Vec<u8>,
}

/// GitHub over one or more routes. A request that cannot reach GitHub moves on to the next route;
/// an answer, whatever its status, is final.
struct Client {
    web: String,
    routes: Vec<(&'static str, ureq::Agent)>,
    /// The first route not known to be dead, so later requests do not retry the ones that failed.
    route: Cell<usize>,
}

impl Client {
    fn new(web: &str, routes: Vec<Route>) -> Self {
        let routes = routes
            .into_iter()
            .map(|(label, proxy)| {
                let agent = ureq::Agent::config_builder()
                    .proxy(proxy)
                    .user_agent(concat!("mihoctl/", env!("CARGO_PKG_VERSION")))
                    .https_only(web.starts_with("https://"))
                    .http_status_as_error(false)
                    .timeout_connect(Some(Duration::from_secs(10)))
                    // A proxy whose node is dead accepts the connection and then says nothing.
                    .timeout_recv_response(Some(Duration::from_secs(20)))
                    .timeout_global(Some(Duration::from_secs(300)))
                    .build()
                    .into();
                (label, agent)
            })
            .collect();
        Self { web: web.to_string(), routes, route: Cell::new(0) }
    }

    fn get(&self, url: &str, limit: u64, follow: bool) -> Result<Reply> {
        let mut errors = Vec::new();
        for (i, (label, agent)) in self.routes.iter().enumerate().skip(self.route.get()) {
            match send(agent, url, limit, follow) {
                Ok(reply) => {
                    self.route.set(i);
                    return Ok(reply);
                }
                Err(e) => errors.push(format!("{label}: {e:#}")),
            }
        }
        bail!("could not reach GitHub: {}", errors.join("; "))
    }

    /// The latest published release (GitHub leaves out drafts and prereleases). `/releases/latest`
    /// redirects to `/releases/tag/<tag>`, or to the bare `/releases` list while nothing is published.
    fn latest(&self) -> Result<Release> {
        let reply = self.get(&format!("{}/{REPO}/releases/latest", self.web), MAX_SUMS, false)?;
        match reply.status {
            301 | 302 | 303 | 307 | 308 => {}
            404 => bail!("{REPO} was not found on GitHub"),
            403 | 429 => bail!(RATE_LIMITED),
            other => bail!("GitHub answered HTTP {other} to the latest-release lookup"),
        }
        let location = reply.location.unwrap_or_default();
        match location.split_once("/releases/tag/") {
            Some((_, tag)) => release_of(tag.split(['?', '#']).next().unwrap_or(tag)),
            None => bail!("no release has been published yet"),
        }
    }

    /// `None` when the release has no such asset.
    fn download(&self, release: &Release, name: &str, limit: u64) -> Result<Option<Vec<u8>>> {
        let reply = self.get(&format!("{}/{REPO}/releases/download/{}/{name}", self.web, release.tag), limit, true)?;
        match reply.status {
            200 => Ok(Some(reply.body)),
            404 => Ok(None),
            403 | 429 => bail!(RATE_LIMITED),
            other => bail!("downloading {name}: HTTP {other}"),
        }
    }
}

fn send(agent: &ureq::Agent, url: &str, limit: u64, follow: bool) -> Result<Reply> {
    let request = agent.get(url);
    let request = if follow { request } else { request.config().max_redirects(0).max_redirects_will_error(false).build() };
    let mut resp = request.call().with_context(|| format!("GET {url}"))?;
    let status = resp.status().as_u16();
    let location = resp.headers().get("location").and_then(|v| v.to_str().ok()).map(str::to_string);
    let body = resp.body_mut().with_config().limit(limit).read_to_vec().with_context(|| format!("reading {url}"))?;
    Ok(Reply { status, location, body })
}

/// An explicit `--proxy`, else the running core's mixed port and then the environment's proxy settings.
fn routes(proxy: Option<&str>, core_proxy: Option<String>) -> Result<Vec<Route>> {
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
    let wanted = version.map(release_of).transpose()?;
    let exe = std::env::current_exe()?.canonicalize().context("locating the running executable")?;
    update(WEB, routes(proxy, core_proxy)?, target, wanted.map(|r| r.tag).as_deref(), check, &exe)
}

fn update(web: &str, routes: Vec<Route>, target: &str, tag: Option<&str>, check: bool, exe: &Path) -> Result<()> {
    let current = Version::parse(env!("CARGO_PKG_VERSION")).context("own version")?;
    let client = Client::new(web, routes);
    let release = match tag {
        Some(tag) => release_of(tag)?,
        None => client.latest()?,
    };

    // Plain `self-update` only ever moves forward; naming a version is an explicit up- or downgrade.
    if release.version == current || (tag.is_none() && release.version < current) {
        let note = if release.version == current { "" } else { " (newer than the latest release)" };
        eprintln!("mihoctl {current} is up to date{note}; latest release is {}", release.tag);
        return Ok(());
    }

    let stem = format!("{BIN}-{}-{target}", release.tag);
    let name = format!("{stem}.tar.gz");
    let sums = client
        .download(&release, CHECKSUMS, MAX_SUMS)?
        .with_context(|| format!("release {} does not exist or has no {CHECKSUMS}", release.tag))?;
    let Some(expected) = checksum_for(&String::from_utf8_lossy(&sums), &name)? else {
        bail!("release {} has no build for {target}; build from source: cargo install --git https://github.com/{REPO}", release.tag);
    };

    let verb = if release.version > current { "upgrade" } else { "downgrade" };
    eprintln!("{verb}: mihoctl {current} -> {} ({target})", release.version);
    eprintln!("release notes: {web}/{REPO}/releases/tag/{}", release.tag);
    if check {
        eprintln!("run `mihoctl self-update{}` to install it", tag.map(|t| format!(" --version {t}")).unwrap_or_default());
        return Ok(());
    }

    eprintln!("downloading {name}");
    let archive = client
        .download(&release, &name, MAX_ARCHIVE)?
        .with_context(|| format!("release {} has no {name}", release.tag))?;
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
    use std::io::{BufRead, BufReader};
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

    /// What the loopback server answers for one path.
    struct Canned {
        status: &'static str,
        headers: String,
        body: Vec<u8>,
    }

    fn ok(body: impl Into<Vec<u8>>) -> Canned {
        Canned { status: "200 OK", headers: String::new(), body: body.into() }
    }

    fn redirect(to: &str) -> Canned {
        Canned { status: "302 Found", headers: format!("Location: {to}\r\n"), body: b"redirecting".to_vec() }
    }

    fn refuse(status: &'static str) -> Canned {
        Canned { status, headers: String::new(), body: Vec::new() }
    }

    /// Serves `routes` on a loopback port until the test process exits; anything else is a 404.
    fn serve(routes: Vec<(String, Canned)>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).ok();
                let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
                // Drain the request head so closing the socket cannot reset the connection mid-response.
                while reader.read_line(&mut line).is_ok_and(|n| n > 2) {}
                let (status, headers, body) = match routes.iter().find(|(p, _)| *p == path) {
                    Some((_, c)) => (c.status, c.headers.as_str(), c.body.as_slice()),
                    None => ("404 Not Found", "", &b""[..]),
                };
                let head = format!("HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                stream.write_all(head.as_bytes()).and_then(|_| stream.write_all(body)).ok();
            }
        });
        base
    }

    fn download_path(name: &str) -> String {
        format!("/{REPO}/releases/download/{TAG}/{name}")
    }

    /// The checksum file and archive of release `TAG`; `sums` replaces the real checksum file.
    fn release_routes(archive: &[u8], sums: Option<String>) -> Vec<(String, Canned)> {
        let name = format!("mihoctl-{TAG}-{TARGET}.tar.gz");
        let sums = sums.unwrap_or_else(|| format!("{:x}  {name}\n{:x}  other.zip\n", Sha256::digest(archive), Sha256::digest(b"x")));
        vec![(download_path(CHECKSUMS), ok(sums)), (download_path(&name), ok(archive))]
    }

    /// `/releases/latest` as GitHub answers it: a redirect to the release's page, which is never followed.
    fn latest(tag: &str) -> (String, Canned) {
        (format!("/{REPO}/releases/latest"), redirect(&format!("https://github.com/{REPO}/releases/tag/{tag}")))
    }

    fn direct() -> Vec<Route> {
        vec![("direct", None)]
    }

    /// A route through a proxy that is not listening.
    fn dead(label: &'static str) -> Route {
        let addr = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        (label, Some(ureq::Proxy::new(&format!("http://{addr}")).unwrap()))
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

    fn error(result: Result<()>) -> String {
        format!("{:#}", result.unwrap_err())
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
        assert_eq!(release_of("0.2.0").unwrap().tag, "v0.2.0");
        assert_eq!(release_of("v0.2.0-rc.1").unwrap().version, Version::parse("0.2.0-rc.1").unwrap());
        for bad in ["latest", "v1", "v1.2", "v1.2.3/../../x", "../v1.2.3", "v1.2.3?x=1", "v1.2.3%2Fx", ""] {
            assert!(release_of(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn checksum_lookup_is_exact() {
        let digest = "ab".repeat(32);
        let sums = format!("{digest}  mihoctl-v1.0.0-a.tar.gz\n{} *mihoctl-v1.0.0-b.tar.gz\n", "CD".repeat(32));
        assert_eq!(checksum_for(&sums, "mihoctl-v1.0.0-a.tar.gz").unwrap(), Some(digest));
        assert_eq!(checksum_for(&sums, "mihoctl-v1.0.0-b.tar.gz").unwrap(), Some("cd".repeat(32)));
        assert_eq!(checksum_for(&sums, "mihoctl-v1.0.0.tar.gz").unwrap(), None);
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
    fn update_finds_downloads_verifies_and_installs_the_latest() {
        let dir = scratch("e2e");
        let exe = installed(&dir);
        let mut routes = release_routes(&release_archive("9.9.9"), None);
        routes.push(latest(TAG));
        let web = serve(routes);
        let checked = update(&web, direct(), TARGET, None, true, &exe);
        let untouched = says(&exe);
        let installed_run = update(&web, direct(), TARGET, None, false, &exe);
        let said = says(&exe);
        std::fs::remove_dir_all(&dir).ok();
        checked.unwrap();
        assert_eq!(untouched, "mihoctl 0.0.1", "--check must not install");
        installed_run.unwrap();
        assert_eq!(said, "mihoctl 9.9.9");
    }

    #[test]
    fn naming_a_version_needs_no_latest_lookup() {
        let dir = scratch("by-tag");
        let exe = installed(&dir);
        // No `/releases/latest` route: asking for it would 404.
        let web = serve(release_routes(&release_archive("9.9.9"), None));
        let result = update(&web, direct(), TARGET, Some(TAG), false, &exe);
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
        let web = serve(release_routes(&release_archive("9.9.9"), Some(format!("{}  {name}\n", "00".repeat(32)))));
        let result = update(&web, direct(), TARGET, Some(TAG), false, &exe);
        let said = says(&exe);
        std::fs::remove_dir_all(&dir).ok();
        assert!(error(result).contains("does not match"));
        assert_eq!(said, "mihoctl 0.0.1");
    }

    #[test]
    fn a_platform_missing_from_the_checksums_has_no_build() {
        let dir = scratch("no-build");
        let exe = installed(&dir);
        let web = serve(release_routes(&release_archive("9.9.9"), None));
        let result = update(&web, direct(), "aarch64-apple-darwin", Some(TAG), false, &exe);
        std::fs::remove_dir_all(&dir).ok();
        assert!(error(result).contains("has no build for aarch64-apple-darwin"));
    }

    #[test]
    fn a_release_that_does_not_exist_is_reported() {
        let web = serve(Vec::new());
        let err = error(update(&web, direct(), TARGET, Some("v8.8.8"), true, Path::new("/nonexistent/mihoctl")));
        assert!(err.contains("release v8.8.8 does not exist"), "{err}");
    }

    #[test]
    fn up_to_date_installs_nothing_and_downloads_nothing() {
        let dir = scratch("current");
        let exe = installed(&dir);
        let web = serve(vec![latest(&format!("v{}", env!("CARGO_PKG_VERSION")))]);
        let result = update(&web, direct(), TARGET, None, false, &exe);
        let said = says(&exe);
        std::fs::remove_dir_all(&dir).ok();
        result.unwrap();
        assert_eq!(said, "mihoctl 0.0.1");
    }

    #[test]
    fn no_published_release_is_reported() {
        // GitHub redirects to the bare release list while a repository has none.
        let web = serve(vec![(format!("/{REPO}/releases/latest"), redirect(&format!("https://github.com/{REPO}/releases")))]);
        let err = error(update(&web, direct(), TARGET, None, true, Path::new("/nonexistent/mihoctl")));
        assert!(err.contains("no release has been published yet"), "{err}");
    }

    #[test]
    fn a_redirect_to_something_that_is_not_a_version_is_refused() {
        for tag in ["v1.0.0/../../x", "latest", "v1.0.0%2F..%2Fx"] {
            let web = serve(vec![latest(tag)]);
            let err = error(update(&web, direct(), TARGET, None, true, Path::new("/nonexistent/mihoctl")));
            assert!(err.contains("is not a release version"), "{tag}: {err}");
        }
    }

    #[test]
    fn rate_limiting_is_named_as_such() {
        let web = serve(vec![(format!("/{REPO}/releases/latest"), refuse("429 Too Many Requests"))]);
        let err = error(update(&web, direct(), TARGET, None, true, Path::new("/nonexistent/mihoctl")));
        assert!(err.contains("limiting requests"), "{err}");
    }

    #[test]
    fn an_unreachable_route_falls_through_to_the_next() {
        let dir = scratch("fallback");
        let exe = installed(&dir);
        let mut routes = release_routes(&release_archive("9.9.9"), None);
        routes.push(latest(TAG));
        let web = serve(routes);
        let result = update(&web, vec![dead("via core"), ("direct", None)], TARGET, None, false, &exe);
        let said = says(&exe);
        std::fs::remove_dir_all(&dir).ok();
        result.unwrap();
        assert_eq!(said, "mihoctl 9.9.9");
    }

    #[test]
    fn an_answer_from_github_is_final_even_with_routes_left() {
        let web = serve(Vec::new());
        let err = error(update(&web, vec![("direct", None), dead("second")], TARGET, Some(TAG), true, Path::new("/nonexistent/mihoctl")));
        assert!(err.contains("does not exist") && !err.contains("second"), "{err}");
    }

    #[test]
    fn every_unreachable_route_is_reported() {
        let web = serve(Vec::new());
        let err = error(update(&web, vec![dead("first"), dead("second")], TARGET, None, true, Path::new("/nonexistent/mihoctl")));
        assert!(err.contains("could not reach GitHub") && err.contains("first:") && err.contains("second:"), "{err}");
    }
}
