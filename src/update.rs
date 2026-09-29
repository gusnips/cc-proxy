//! Self-update: replace the running binary with a newer GitHub release.
//!
//! `cc-proxy update` keeps the install hands-free: it compares the running
//! version against the latest release tag, downloads the matching platform
//! archive over HTTPS, verifies its SHA-256 checksum, swaps the binary
//! atomically, and restarts the background service when one is running.
//! `cc-proxy update --check` only reports; it changes nothing.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};

use crate::daemon;

const REPO: &str = "gusnips/cc-proxy";
const BIN_NAME: &str = "cc-proxy";
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Archive suffix per platform, matching the release workflow matrix.
pub fn asset_platform() -> Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Ok("darwin-arm64"),
        ("macos", "x86_64") => Ok("darwin-amd64"),
        ("linux", "x86_64") => Ok("linux-amd64"),
        ("linux", "aarch64") => Ok("linux-arm64"),
        (os, arch) => anyhow::bail!(
            "no prebuilt {BIN_NAME} binary for {os}/{arch}; build from source instead:\n\
             git clone https://github.com/{REPO} && ./scripts/install.sh --local"
        ),
    }
}

fn normalize_tag(tag: &str) -> String {
    let trimmed = tag.trim();
    if trimmed.starts_with('v') {
        trimmed.to_string()
    } else {
        format!("v{trimmed}")
    }
}

fn version_parts(version: &str) -> Option<Vec<u64>> {
    version
        .trim()
        .trim_start_matches('v')
        .split('.')
        .map(|part| part.parse::<u64>().ok())
        .collect()
}

/// True when `latest` is strictly newer than `current`. Unparseable versions
/// compare unequal so an explicit request still proceeds to the download.
pub fn is_newer(current: &str, latest: &str) -> bool {
    match (version_parts(current), version_parts(latest)) {
        (Some(current), Some(latest)) => {
            let width = current.len().max(latest.len());
            let mut current = current;
            let mut latest = latest;
            current.resize(width, 0);
            latest.resize(width, 0);
            latest > current
        }
        _ => current.trim() != latest.trim(),
    }
}

fn parse_latest_tag(body: &serde_json::Value) -> Result<String> {
    body.get("tag_name")
        .and_then(|tag| tag.as_str())
        .filter(|tag| !tag.is_empty())
        .map(normalize_tag)
        .context("GitHub releases API did not return a tag_name")
}

fn http_client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .user_agent(format!("{BIN_NAME}-update/{CURRENT_VERSION}"))
        .timeout(Duration::from_secs(120))
        .build()
        .context("failed to build HTTP client")
}

fn download_text(client: &reqwest::blocking::Client, url: &str) -> Result<String> {
    client
        .get(url)
        .send()
        .with_context(|| format!("request failed: {url}"))?
        .error_for_status()
        .with_context(|| format!("download failed: {url}"))?
        .text()
        .context("failed to read response body")
}

fn download_bytes(client: &reqwest::blocking::Client, url: &str) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    client
        .get(url)
        .send()
        .with_context(|| format!("request failed: {url}"))?
        .error_for_status()
        .with_context(|| format!("download failed: {url}"))?
        .copy_to(&mut bytes)
        .context("failed to read response body")?;
    Ok(bytes)
}

pub fn fetch_latest_tag(client: &reqwest::blocking::Client) -> Result<String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let body: serde_json::Value =
        serde_json::from_str(&download_text(client, &url)?).context("invalid releases response")?;
    parse_latest_tag(&body)
}

fn archive_urls(tag: &str, platform: &str) -> (String, String) {
    let base = format!("https://github.com/{REPO}/releases/download/{tag}");
    (
        format!("{base}/{BIN_NAME}-{platform}.tar.gz"),
        format!("{base}/{BIN_NAME}-{platform}.sha256"),
    )
}

fn verify_sha256(bytes: &[u8], checksum_file: &str) -> Result<()> {
    use sha2::Digest;
    let expected = checksum_file
        .split_whitespace()
        .next()
        .context("checksum file is empty")?;
    let mut hasher = sha2::Sha256::new();
    hasher.update(bytes);
    let actual = format!("{:x}", hasher.finalize());
    if actual.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        anyhow::bail!("checksum mismatch: the download may be corrupt; refusing to install")
    }
}

/// Extract the single `cc-proxy` binary from the release tarball.
fn extract_binary(archive: &[u8]) -> Result<Vec<u8>> {
    let gz = flate2::read::GzDecoder::new(archive);
    let mut tar = tar::Archive::new(gz);
    for entry in tar.entries().context("invalid release archive")? {
        let mut entry = entry.context("invalid release archive")?;
        if entry.path().is_ok_and(|path| path.ends_with(BIN_NAME)) {
            let mut binary = Vec::new();
            entry.read_to_end(&mut binary)?;
            if binary.is_empty() {
                anyhow::bail!("release archive contains an empty binary");
            }
            return Ok(binary);
        }
    }
    anyhow::bail!("release archive does not contain the {BIN_NAME} binary")
}

fn install_path() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("cannot locate the running binary")?;
    let dir = exe
        .parent()
        .context("running binary has no parent directory")?;
    Ok(dir.join(BIN_NAME))
}

/// Atomically replace the installed binary: write beside it, then rename.
fn replace_binary(path: &Path, binary: &[u8]) -> Result<()> {
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&tmp, binary).with_context(|| format!("cannot write to {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::rename(&tmp, path).with_context(|| {
        format!(
            "cannot replace {} (not writable? re-run with sudo or set CC_PROXY_INSTALL_DIR)",
            path.display()
        )
    })?;
    Ok(())
}

/// Update the installed binary to `version` (`None` = latest release).
/// With `check`, only report what would happen.
pub fn run_update(check: bool, version: Option<&str>) -> Result<()> {
    let platform = asset_platform()?;
    let client = http_client()?;
    let tag = match version {
        Some(pinned) => normalize_tag(pinned),
        None => fetch_latest_tag(&client).context(
            "cannot reach the GitHub releases API (offline?); \
             pin a version with `cc-proxy update --version vX.Y.Z`",
        )?,
    };

    if !is_newer(CURRENT_VERSION, &tag) {
        println!("{BIN_NAME} {CURRENT_VERSION} is already up to date ({tag}).");
        return Ok(());
    }
    if check {
        println!(
            "{BIN_NAME} {CURRENT_VERSION} -> {tag} available; re-run without --check to install."
        );
        return Ok(());
    }

    let path = install_path()?;
    let (archive_url, checksum_url) = archive_urls(&tag, platform);
    println!("Downloading {BIN_NAME} {tag} for {platform}...");
    let archive = download_bytes(&client, &archive_url)?;
    let checksum = download_text(&client, &checksum_url)?;
    verify_sha256(&archive, &checksum)?;
    let binary = extract_binary(&archive)?;
    replace_binary(&path, &binary)?;
    println!("Installed {BIN_NAME} {tag} to {}", path.display());

    if let daemon::DaemonStatus::Running(info) = daemon::describe() {
        let info = daemon::restart_service(Some(info.port))?;
        println!(
            "Background service restarted (pid {}) on {}.",
            info.pid,
            info.listen_url()
        );
    }
    println!("Verify with `{BIN_NAME} --version`.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_matches_release_matrix() {
        // Must stay in sync with .github/workflows/release.yml.
        assert!(asset_platform().is_ok());
        assert!(
            ["darwin-arm64", "darwin-amd64", "linux-amd64", "linux-arm64"]
                .contains(&asset_platform().unwrap())
        );
    }

    #[test]
    fn newer_detection_ignores_leading_v_and_width() {
        assert!(is_newer("0.1.42", "v0.1.43"));
        assert!(is_newer("v0.1.9", "v0.1.10"));
        assert!(!is_newer("0.1.43", "v0.1.43"));
        assert!(!is_newer("v0.2.0", "v0.1.99"));
        assert!(is_newer("bogus", "v1.0.0"));
    }

    #[test]
    fn latest_tag_parses_github_response() {
        let body = serde_json::json!({"tag_name": "v0.1.43"});
        assert_eq!(parse_latest_tag(&body).unwrap(), "v0.1.43");
        assert!(parse_latest_tag(&serde_json::json!({})).is_err());
    }

    #[test]
    fn checksum_rejects_tampered_bytes() {
        use sha2::Digest;
        let bytes = b"fake-binary";
        let mut hasher = sha2::Sha256::new();
        hasher.update(bytes);
        let good = format!("{:x}  cc-proxy-darwin-arm64.tar.gz", hasher.finalize());
        assert!(verify_sha256(bytes, &good).is_ok());
        assert!(verify_sha256(b"tampered", &good).is_err());
        assert!(verify_sha256(bytes, "").is_err());
    }

    #[test]
    fn extract_finds_binary_in_tarball() {
        let mut archive = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        let bytes = b"binary-bytes";
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        archive
            .append_data(&mut header, BIN_NAME, bytes.as_slice())
            .unwrap();
        let gz = archive.into_inner().unwrap().finish().unwrap();
        assert_eq!(extract_binary(&gz).unwrap(), bytes);
        assert!(extract_binary(b"not-a-tarball").is_err());
    }
}
