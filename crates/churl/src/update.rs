//! `churl update`: self-replace the running binary from the latest GitHub
//! release.
//!
//! Network + self-replace + release-artifact concerns are binary-only (never
//! `churl-core`). The pure, unit-testable pieces — target-triple → asset-name
//! mapping, the version-compare decision, checksum parsing/verification — are
//! free functions below; the async orchestration lives in [`run_update`].
//!
//! Safety contract: nothing irreversible happens until the download is verified
//! against its published SHA-256. The current binary is backed up to `churl.bak`
//! beside itself before an atomic replace, so any failure leaves a usable
//! binary. A checksum mismatch aborts and touches nothing (fail-closed).

use std::io::Read;
use std::path::Path;

use color_eyre::Result;
use color_eyre::eyre::{Context, bail, eyre};
use sha2::{Digest, Sha256};

/// GitHub API endpoint for the latest (non-prerelease) release of this repo.
const LATEST_RELEASE_API: &str = "https://api.github.com/repos/AlsubaieAli/churl/releases/latest";

/// The running binary's version, from its `Cargo.toml`.
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The decision reached by comparing the running version to the latest release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateDecision {
    /// The running version is already the latest — no download.
    UpToDate,
    /// A different version is available (newer or, unusually, older than the
    /// running build). Carries the target version to move to.
    Available {
        /// The version offered by the latest release.
        latest: String,
    },
}

/// The Rust target triple this binary was compiled for. Resolved at compile
/// time from `cfg`, matching the triples the release workflow builds for.
/// Returns `None` for a triple churl does not publish assets for.
pub fn target_triple() -> Option<&'static str> {
    // The five published targets (see release.yml / install.sh). Any other
    // host has no release asset, so self-update cannot proceed.
    if cfg!(all(target_arch = "aarch64", target_os = "macos")) {
        Some("aarch64-apple-darwin")
    } else if cfg!(all(target_arch = "x86_64", target_os = "macos")) {
        Some("x86_64-apple-darwin")
    } else if cfg!(all(target_arch = "x86_64", target_os = "linux")) {
        Some("x86_64-unknown-linux-musl")
    } else if cfg!(all(target_arch = "aarch64", target_os = "linux")) {
        Some("aarch64-unknown-linux-musl")
    } else if cfg!(all(target_arch = "x86_64", target_os = "windows")) {
        Some("x86_64-pc-windows-msvc")
    } else {
        None
    }
}

/// The release asset filenames for `target`: the archive and its checksum
/// sidecar. MUST match the release workflow + install scripts exactly:
/// `churl-<target>.tar.gz` (unix) / `churl-<target>.zip` (windows), with the
/// checksum named after the archive *stem* — `churl-<target>.sha256`, NOT
/// `…​.tar.gz.sha256` (a mismatch here 404s every update).
pub fn asset_names(target: &str) -> (String, String) {
    let archive_ext = if target.contains("windows") {
        "zip"
    } else {
        "tar.gz"
    };
    let archive = format!("churl-{target}.{archive_ext}");
    let checksum = format!("churl-{target}.sha256");
    (archive, checksum)
}

/// Decides whether an update is warranted by comparing the running version to
/// the `latest` release tag. Semver-aware when both parse (so `0.10.0` > `0.9.0`
/// compares numerically); otherwise falls back to exact string inequality.
/// Anything that is not exactly the running version is [`UpdateDecision::Available`]
/// — the guard is "already current ⇒ do nothing", not "only upgrade".
pub fn compare_versions(current: &str, latest: &str) -> UpdateDecision {
    let current = current.trim();
    let latest = normalize_tag(latest);
    let same = match (
        semver::Version::parse(current),
        semver::Version::parse(latest),
    ) {
        (Ok(a), Ok(b)) => a == b,
        _ => current == latest,
    };
    if same {
        UpdateDecision::UpToDate
    } else {
        UpdateDecision::Available {
            latest: latest.to_owned(),
        }
    }
}

/// Strips a leading `v` from a release tag (`v0.2.0` → `0.2.0`) so it compares
/// against the crate version, which carries no prefix.
fn normalize_tag(tag: &str) -> &str {
    tag.trim().strip_prefix('v').unwrap_or(tag.trim())
}

/// Extracts the expected hex digest for `archive_name` from a `sha256sum`-format
/// checksum file (`<hex>  <filename>` lines). Returns the digest of the line
/// naming the archive, or — for a single-line file with no filename column — its
/// lone digest. `None` if no digest is found. Case-normalised to lowercase.
pub fn parse_checksum_file(text: &str, archive_name: &str) -> Option<String> {
    let mut only: Option<&str> = None;
    let mut single_candidate = true;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let digest = parts.next()?;
        match parts.next() {
            // `<hex>  <name>` (or `<hex> *<name>` binary marker) — match the name.
            Some(name) => {
                let name = name.trim_start_matches('*');
                if name == archive_name {
                    return Some(digest.to_ascii_lowercase());
                }
                single_candidate = false;
            }
            // Bare `<hex>` with no filename column — remember as the sole digest.
            None => only = Some(digest),
        }
    }
    // Only honour a bare digest when the file was purely bare lines (no named
    // lines that simply didn't match — that would be the wrong archive).
    if single_candidate {
        only.map(str::to_ascii_lowercase)
    } else {
        None
    }
}

/// Whether `bytes` hash to `expected_hex` under SHA-256 (case-insensitive hex).
/// The fail-closed gate before any binary swap.
pub fn verify_checksum(bytes: &[u8], expected_hex: &str) -> bool {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let actual = hasher.finalize();
    let actual_hex = hex_lower(&actual);
    actual_hex == expected_hex.trim().to_ascii_lowercase()
}

/// Lowercase hex encoding of a byte slice (no external hex dep).
fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Copies `exe` to `<name>.bak` beside it so a failed/undesired update is
/// reversible, returning the backup path. Pure I/O — split out so the backup
/// step is testable against a fake binary without invoking `self_replace`
/// (which always targets the *real* running executable).
pub fn backup_binary(exe: &Path) -> Result<std::path::PathBuf> {
    let backup = backup_path(exe);
    std::fs::copy(exe, &backup)
        .wrap_err_with(|| format!("failed to back up current binary to {}", backup.display()))?;
    Ok(backup)
}

/// Writes `new_bytes` to a fresh temp file (marked executable on unix, since a
/// fresh temp file is `0600`) and returns the handle. The staged file is what
/// gets atomically renamed over the running binary. Split out so staging is
/// testable independently of the `self_replace` swap.
pub fn stage_binary(new_bytes: &[u8]) -> Result<tempfile::NamedTempFile> {
    let staged = tempfile::Builder::new()
        .prefix("churl-update-")
        .tempfile()
        .wrap_err("failed to create staging file for the update")?;
    std::fs::write(staged.path(), new_bytes).wrap_err("failed to write staged binary")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(staged.path(), std::fs::Permissions::from_mode(0o755))
            .wrap_err("failed to mark staged binary executable")?;
    }
    Ok(staged)
}

/// Backs up the running binary to `<name>.bak`, then atomically replaces it with
/// `new_bytes` via `self_replace` (cross-platform atomic rename over the running
/// binary — never hand-rolled). On any failure the original binary remains usable
/// (the swap is atomic, and the backup is written first). Returns the backup path.
///
/// `self_replace` always targets [`std::env::current_exe`], so this function is
/// exercised end-to-end only by the reviewer + a real release, never by
/// `cargo test`; the reversible/atomic *mechanics* are unit-tested via
/// [`backup_binary`] + [`stage_binary`] against a fake binary in a tempdir.
pub fn backup_and_replace(current_exe: &Path, new_bytes: &[u8]) -> Result<std::path::PathBuf> {
    let backup = backup_binary(current_exe)?;
    let staged = stage_binary(new_bytes)?;
    self_replace::self_replace(staged.path())
        .wrap_err("failed to atomically replace the running binary")?;
    Ok(backup)
}

/// The backup path for a binary: its own path with a `.bak` extension appended
/// (`.../churl` → `.../churl.bak`, `churl.exe` → `churl.exe.bak`).
fn backup_path(exe: &Path) -> std::path::PathBuf {
    let mut name = exe.file_name().unwrap_or_default().to_os_string();
    name.push(".bak");
    exe.with_file_name(name)
}

/// A GitHub release, minimally deserialized: only the tag we compare against.
#[derive(serde::Deserialize)]
struct Release {
    tag_name: String,
}

/// Builds the shared HTTP client. GitHub's API + release CDN both require a
/// `User-Agent`; reqwest sends none by default.
fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!("churl/", env!("CARGO_PKG_VERSION")))
        .build()
        .wrap_err("failed to build HTTP client")
}

/// Queries the latest release's tag from the GitHub API.
async fn fetch_latest_tag(client: &reqwest::Client) -> Result<String> {
    let release: Release = client
        .get(LATEST_RELEASE_API)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .wrap_err("failed to query the latest release")?
        .error_for_status()
        .wrap_err("GitHub returned an error for the latest release")?
        .json()
        .await
        .wrap_err("failed to parse the latest-release response")?;
    Ok(release.tag_name)
}

/// Downloads `url` into memory as raw bytes.
async fn download_bytes(client: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    let bytes = client
        .get(url)
        .send()
        .await
        .wrap_err_with(|| format!("failed to download {url}"))?
        .error_for_status()
        .wrap_err_with(|| format!("download failed for {url}"))?
        .bytes()
        .await
        .wrap_err_with(|| format!("failed to read body of {url}"))?;
    Ok(bytes.to_vec())
}

/// Extracts the single `churl`/`churl.exe` binary from a downloaded release
/// archive (tar.gz on unix, zip on windows) in memory.
fn extract_binary(archive: &[u8], target: &str) -> Result<Vec<u8>> {
    if target.contains("windows") {
        extract_from_zip(archive)
    } else {
        extract_from_targz(archive)
    }
}

/// Reads the `churl` entry out of a gzip-compressed tarball.
fn extract_from_targz(archive: &[u8]) -> Result<Vec<u8>> {
    let decoder = flate2::read::GzDecoder::new(archive);
    let mut tar = tar::Archive::new(decoder);
    for entry in tar.entries().wrap_err("failed to read tar entries")? {
        let mut entry = entry.wrap_err("failed to read a tar entry")?;
        let path = entry.path().wrap_err("bad path in tar")?;
        if path.file_name().and_then(|n| n.to_str()) == Some("churl") {
            let mut buf = Vec::new();
            entry
                .read_to_end(&mut buf)
                .wrap_err("failed to read churl binary from tar")?;
            return Ok(buf);
        }
    }
    bail!("release archive did not contain a `churl` binary")
}

/// Reads the `churl.exe` entry out of a zip archive.
fn extract_from_zip(archive: &[u8]) -> Result<Vec<u8>> {
    let reader = std::io::Cursor::new(archive);
    let mut zip = zip::ZipArchive::new(reader).wrap_err("failed to open release zip")?;
    for i in 0..zip.len() {
        let mut file = zip.by_index(i).wrap_err("failed to read a zip entry")?;
        let name = file.name().rsplit('/').next().unwrap_or(file.name());
        if name == "churl.exe" {
            let mut buf = Vec::new();
            file.read_to_end(&mut buf)
                .wrap_err("failed to read churl.exe from zip")?;
            return Ok(buf);
        }
    }
    bail!("release archive did not contain a `churl.exe` binary")
}

/// `churl update`: check the latest release, and (unless `--check`) verify and
/// atomically self-replace after a confirmation.
///
/// - `check_only`: report the available version and exit, downloading nothing.
/// - `yes`: skip the interactive confirm.
pub async fn run_update(check_only: bool, yes: bool) -> Result<()> {
    let Some(target) = target_triple() else {
        bail!(
            "self-update is unavailable on this platform (no published release asset for this target)"
        );
    };

    let client = client()?;
    let tag = fetch_latest_tag(&client).await?;
    let latest = match compare_versions(CURRENT_VERSION, &tag) {
        UpdateDecision::UpToDate => {
            println!("churl {CURRENT_VERSION} is already the latest release.");
            return Ok(());
        }
        UpdateDecision::Available { latest } => latest,
    };

    println!("churl {CURRENT_VERSION} → {latest}");
    if check_only {
        println!("run `churl update` to install it (`-y` to skip the prompt).");
        return Ok(());
    }

    if !yes && !confirm("Update now?")? {
        println!("update cancelled.");
        return Ok(());
    }

    let (archive_name, checksum_name) = asset_names(target);
    let base = format!("https://github.com/AlsubaieAli/churl/releases/download/{tag}");
    let archive_url = format!("{base}/{archive_name}");
    let checksum_url = format!("{base}/{checksum_name}");

    println!("Downloading {archive_name} …");
    let archive = download_bytes(&client, &archive_url).await?;
    let checksum_text = String::from_utf8(download_bytes(&client, &checksum_url).await?)
        .wrap_err("checksum file was not valid UTF-8")?;

    let expected = parse_checksum_file(&checksum_text, &archive_name)
        .ok_or_else(|| eyre!("no checksum found for {archive_name} in the published sums"))?;
    if !verify_checksum(&archive, &expected) {
        bail!("checksum mismatch for {archive_name} — aborting, nothing was changed");
    }
    println!("Checksum verified.");

    let new_binary = extract_binary(&archive, target)?;
    let current_exe = std::env::current_exe().wrap_err("cannot locate the running executable")?;
    let backup = backup_and_replace(&current_exe, &new_binary)?;

    println!(
        "Updated to churl {latest}. Previous binary saved at {}.",
        backup.display()
    );
    Ok(())
}

/// Prompts `question [y/N]` on stderr and reads a line from stdin; anything but
/// an affirmative answer is a decline.
fn confirm(question: &str) -> Result<bool> {
    use std::io::Write;
    eprint!("{question} [y/N] ");
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .wrap_err("failed to read confirmation")?;
    let answer = line.trim().to_ascii_lowercase();
    Ok(answer == "y" || answer == "yes")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_names_unix_and_windows() {
        assert_eq!(
            asset_names("aarch64-apple-darwin"),
            (
                "churl-aarch64-apple-darwin.tar.gz".to_owned(),
                "churl-aarch64-apple-darwin.sha256".to_owned()
            )
        );
        assert_eq!(
            asset_names("x86_64-pc-windows-msvc"),
            (
                "churl-x86_64-pc-windows-msvc.zip".to_owned(),
                "churl-x86_64-pc-windows-msvc.sha256".to_owned()
            )
        );
    }

    #[test]
    fn target_triple_resolves_on_supported_hosts() {
        // Whatever host runs the tests must be one of the five published
        // targets (CI covers all three OSes), so this is always Some here.
        assert!(target_triple().is_some());
    }

    #[test]
    fn compare_versions_same_is_up_to_date() {
        assert_eq!(compare_versions("0.2.0", "0.2.0"), UpdateDecision::UpToDate);
        // Leading `v` on the tag is normalised away.
        assert_eq!(
            compare_versions("0.2.0", "v0.2.0"),
            UpdateDecision::UpToDate
        );
    }

    #[test]
    fn compare_versions_newer_is_available() {
        assert_eq!(
            compare_versions("0.2.0", "v0.3.0"),
            UpdateDecision::Available {
                latest: "0.3.0".to_owned()
            }
        );
        // Numeric, not lexical: 0.10.0 > 0.9.0.
        assert_eq!(
            compare_versions("0.9.0", "0.10.0"),
            UpdateDecision::Available {
                latest: "0.10.0".to_owned()
            }
        );
    }

    #[test]
    fn compare_versions_older_tag_is_still_available() {
        // "already current ⇒ do nothing"; any difference (even a lower tag) is
        // offered — the guard is equality, not strict-greater.
        assert_eq!(
            compare_versions("0.3.0", "0.2.0"),
            UpdateDecision::Available {
                latest: "0.2.0".to_owned()
            }
        );
    }

    #[test]
    fn parse_checksum_matches_named_line() {
        let text = "abc123  churl-x86_64-apple-darwin.tar.gz\n\
                    def456  churl-other.tar.gz\n";
        assert_eq!(
            parse_checksum_file(text, "churl-x86_64-apple-darwin.tar.gz"),
            Some("abc123".to_owned())
        );
    }

    #[test]
    fn parse_checksum_bare_single_digest() {
        assert_eq!(
            parse_checksum_file("ABC123\n", "churl-any.tar.gz"),
            Some("abc123".to_owned())
        );
    }

    #[test]
    fn parse_checksum_no_match_is_none() {
        let text = "abc123  churl-other.tar.gz\n";
        assert_eq!(parse_checksum_file(text, "churl-wanted.tar.gz"), None);
    }

    #[test]
    fn verify_checksum_match_and_mismatch() {
        // SHA-256 of "hello" (known vector).
        let expected = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        assert!(verify_checksum(b"hello", expected));
        assert!(verify_checksum(b"hello", &expected.to_uppercase()));
        assert!(!verify_checksum(b"world", expected));
    }

    // The atomic-replace + `.bak` backup mechanics, exercised against a FAKE
    // binary in a tempdir. `self_replace` itself always targets the real running
    // executable (the test harness), so it can't run here — the swap is modelled
    // by the same stage-then-rename it performs, over the fake file. `self_replace`'s
    // own atomicity is proven by the reviewer + a real release, not by cargo test.
    #[test]
    fn backup_and_stage_then_swap_a_fake_binary() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("churl");
        std::fs::write(&exe, b"OLD-BINARY").unwrap();

        // 1) Back up the current binary.
        let backup = backup_binary(&exe).unwrap();
        assert_eq!(backup, dir.path().join("churl.bak"));
        assert_eq!(std::fs::read(&backup).unwrap(), b"OLD-BINARY");

        // 2) Stage the new bytes, then atomically rename over the fake binary —
        // the exact stage→rename `backup_and_replace` performs via self_replace.
        let staged = stage_binary(b"NEW-BINARY").unwrap();
        std::fs::rename(staged.path(), &exe).unwrap();

        // Target now holds the new bytes; the backup still holds the old ones.
        assert_eq!(std::fs::read(&exe).unwrap(), b"NEW-BINARY");
        assert_eq!(std::fs::read(&backup).unwrap(), b"OLD-BINARY");
    }

    #[test]
    fn backup_path_appends_bak() {
        assert_eq!(
            backup_path(Path::new("/usr/local/bin/churl")),
            Path::new("/usr/local/bin/churl.bak")
        );
        assert_eq!(
            backup_path(Path::new("C:/bin/churl.exe")),
            Path::new("C:/bin/churl.exe.bak")
        );
    }
}
