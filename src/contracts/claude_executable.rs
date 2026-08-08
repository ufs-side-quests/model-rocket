use std::{
    collections::HashMap,
    fs::{self, File},
    io::{BufReader, Read},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::Duration,
};

use futures_util::StreamExt;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::domain::BridgeError;

const CLAUDE_RELEASE_BASE_URL: &str = "https://downloads.claude.ai/claude-code-releases";
const MAX_MANIFEST_BYTES: usize = 64 * 1024;
const MAX_MANIFEST_BYTES_U64: u64 = 64 * 1024;
const MAX_CLAUDE_EXECUTABLE_BYTES: u64 = 512 * 1024 * 1024;
const EXECUTABLE_HASH_BUFFER_BYTES: usize = 64 * 1024;
const NATIVE_HEADER_BYTES: usize = 4;
const MACH_O_64_HEADER: [u8; NATIVE_HEADER_BYTES] = [0xcf, 0xfa, 0xed, 0xfe];
const ELF_HEADER: [u8; NATIVE_HEADER_BYTES] = *b"\x7fELF";
const MANIFEST_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const MANIFEST_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const CLAUDE_PLATFORM: &str = "darwin-arm64";
#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
const CLAUDE_PLATFORM: &str = "darwin-x64";
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
const CLAUDE_PLATFORM: &str = "linux-arm64";
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const CLAUDE_PLATFORM: &str = "linux-x64";

#[derive(Debug)]
pub struct ValidatedClaudeExecutable {
    path: PathBuf,
    version: String,
}

impl ValidatedClaudeExecutable {
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }
}

#[derive(Debug, Deserialize)]
struct ReleaseManifest {
    version: String,
    platforms: HashMap<String, ReleaseArtifact>,
}

#[derive(Debug, Deserialize)]
struct ReleaseArtifact {
    binary: String,
    checksum: String,
    size: u64,
}

/// Validates an official native Claude Code executable against its release manifest.
///
/// # Errors
///
/// Returns an explicit error when the path, version, official manifest, native format, size, or
/// checksum cannot be verified.
pub async fn validate(path: &Path) -> Result<ValidatedClaudeExecutable, BridgeError> {
    let canonical = canonical_executable(path)?;
    let version = release_version(&canonical)?;
    let manifest = fetch_manifest(&version).await?;
    verify_release(&canonical, &version, &manifest)?;
    Ok(ValidatedClaudeExecutable {
        path: canonical,
        version,
    })
}

fn canonical_executable(path: &Path) -> Result<PathBuf, BridgeError> {
    if !path.is_absolute() {
        return Err(BridgeError::configuration(
            "Claude Code executable must use an absolute path",
        ));
    }
    let canonical = fs::canonicalize(path).map_err(|error| {
        BridgeError::configuration(format!(
            "cannot resolve Claude Code executable {}: {error}",
            path.display()
        ))
    })?;
    let value = canonical.to_str().ok_or_else(|| {
        BridgeError::configuration("Claude Code executable path must contain valid UTF-8")
    })?;
    if value.contains(['\r', '\n']) {
        return Err(BridgeError::configuration(
            "Claude Code executable path must not contain line breaks",
        ));
    }
    Ok(canonical)
}

fn release_version(path: &Path) -> Result<String, BridgeError> {
    let version = path
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .ok_or_else(|| {
            BridgeError::configuration(
                "Claude Code native release path must end with its semantic version",
            )
        })?;
    let mut components = version.split('.');
    let _major = version_component(components.next(), "major")?;
    let _minor = version_component(components.next(), "minor")?;
    let _patch = version_component(components.next(), "patch")?;
    if components.next().is_some() {
        return Err(invalid_version(version));
    }
    Ok(version.to_owned())
}

fn version_component(component: Option<&str>, name: &str) -> Result<u64, BridgeError> {
    let value = component.ok_or_else(|| {
        BridgeError::configuration(format!(
            "Claude Code version is missing its {name} component"
        ))
    })?;
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid_version(value));
    }
    value.parse::<u64>().map_err(|error| {
        BridgeError::configuration(format!("Claude Code {name} version is invalid: {error}"))
    })
}

fn invalid_version(version: &str) -> BridgeError {
    BridgeError::configuration(format!(
        "Claude Code native release path has invalid version {version}"
    ))
}

async fn fetch_manifest(version: &str) -> Result<Vec<u8>, BridgeError> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(MANIFEST_CONNECT_TIMEOUT)
        .timeout(MANIFEST_REQUEST_TIMEOUT)
        .build()
        .map_err(|error| {
            BridgeError::configuration(format!(
                "cannot configure Claude Code release verification: {error}"
            ))
        })?;
    let url = format!("{CLAUDE_RELEASE_BASE_URL}/{version}/manifest.json");
    let response = client.get(&url).send().await.map_err(|error| {
        BridgeError::configuration(format!(
            "cannot fetch the official Claude Code {version} manifest: {error}"
        ))
    })?;
    if !response.status().is_success() {
        return Err(BridgeError::configuration(format!(
            "official Claude Code {version} manifest returned HTTP {}",
            response.status()
        )));
    }
    bounded_manifest(response, version).await
}

async fn bounded_manifest(
    response: reqwest::Response,
    version: &str,
) -> Result<Vec<u8>, BridgeError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_MANIFEST_BYTES_U64)
    {
        return Err(manifest_too_large(version));
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| {
            BridgeError::configuration(format!(
                "cannot read the official Claude Code {version} manifest: {error}"
            ))
        })?;
        let next_length = body
            .len()
            .checked_add(chunk.len())
            .ok_or_else(|| manifest_too_large(version))?;
        if next_length > MAX_MANIFEST_BYTES {
            return Err(manifest_too_large(version));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn manifest_too_large(version: &str) -> BridgeError {
    BridgeError::configuration(format!(
        "official Claude Code {version} manifest exceeds {MAX_MANIFEST_BYTES} bytes"
    ))
}

fn verify_release(path: &Path, version: &str, body: &[u8]) -> Result<(), BridgeError> {
    let manifest: ReleaseManifest = serde_json::from_slice(body).map_err(|error| {
        BridgeError::configuration(format!(
            "official Claude Code {version} manifest is invalid: {error}"
        ))
    })?;
    if manifest.version != version {
        return Err(BridgeError::configuration(format!(
            "official Claude Code manifest version {} does not match executable version {version}",
            manifest.version
        )));
    }
    let artifact = manifest.platforms.get(CLAUDE_PLATFORM).ok_or_else(|| {
        BridgeError::configuration(format!(
            "official Claude Code {version} manifest has no {CLAUDE_PLATFORM} artifact"
        ))
    })?;
    if artifact.binary != "claude" {
        return Err(BridgeError::configuration(format!(
            "official Claude Code {version} manifest names an unexpected {CLAUDE_PLATFORM} artifact"
        )));
    }
    validate_checksum(&artifact.checksum, version)?;
    let actual_digest = native_digest(path, artifact.size)?;
    if actual_digest != artifact.checksum {
        return Err(BridgeError::configuration(format!(
            "Claude Code executable digest does not match the official {version} release at {}",
            path.display()
        )));
    }
    Ok(())
}

fn validate_checksum(checksum: &str, version: &str) -> Result<(), BridgeError> {
    let valid = checksum.len() == 64
        && checksum
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'));
    if !valid {
        return Err(BridgeError::configuration(format!(
            "official Claude Code {version} manifest has an invalid SHA-256 checksum"
        )));
    }
    Ok(())
}

fn native_digest(path: &Path, expected_size: u64) -> Result<String, BridgeError> {
    if expected_size > MAX_CLAUDE_EXECUTABLE_BYTES {
        return Err(executable_too_large(path));
    }
    let file = File::open(path).map_err(|error| {
        BridgeError::configuration(format!(
            "cannot inspect Claude Code executable {}: {error}",
            path.display()
        ))
    })?;
    let metadata = file.metadata().map_err(|error| {
        BridgeError::configuration(format!(
            "cannot inspect Claude Code executable {}: {error}",
            path.display()
        ))
    })?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        return Err(BridgeError::configuration(format!(
            "Claude Code executable must be an executable file: {}",
            path.display()
        )));
    }
    if metadata.len() != expected_size {
        return Err(BridgeError::configuration(format!(
            "Claude Code executable size does not match the official release at {}",
            path.display()
        )));
    }
    hash_native_file(file, path)
}

fn hash_native_file(file: File, path: &Path) -> Result<String, BridgeError> {
    let mut reader = BufReader::with_capacity(EXECUTABLE_HASH_BUFFER_BYTES, file);
    let mut header = [0_u8; NATIVE_HEADER_BYTES];
    reader.read_exact(&mut header).map_err(|error| {
        BridgeError::configuration(format!(
            "cannot read Claude Code executable {}: {error}",
            path.display()
        ))
    })?;
    let native_magic_matches = if cfg!(target_os = "macos") {
        header == MACH_O_64_HEADER
    } else {
        header == ELF_HEADER
    };
    if !native_magic_matches {
        return Err(BridgeError::configuration(format!(
            "Claude Code executable must be the native release, not a script or wrapper: {}",
            path.display()
        )));
    }
    let mut hasher = Sha256::new();
    hasher.update(header);
    let mut buffer = vec![0_u8; EXECUTABLE_HASH_BUFFER_BYTES].into_boxed_slice();
    loop {
        let read = reader.read(&mut buffer).map_err(|error| {
            BridgeError::configuration(format!(
                "cannot read Claude Code executable {}: {error}",
                path.display()
            ))
        })?;
        if read == 0 {
            break;
        }
        hasher.update(buffer.get(..read).ok_or_else(|| {
            BridgeError::configuration("Claude Code read exceeded the hashing buffer")
        })?);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn executable_too_large(path: &Path) -> BridgeError {
    BridgeError::configuration(format!(
        "Claude Code executable exceeds {MAX_CLAUDE_EXECUTABLE_BYTES} bytes at {}",
        path.display()
    ))
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};

    use serde_json::json;
    use sha2::{Digest, Sha256};

    use super::{CLAUDE_PLATFORM, release_version, verify_release};

    struct TemporaryRelease {
        root: PathBuf,
        executable: PathBuf,
    }

    impl TemporaryRelease {
        fn create(version: &str, bytes: &[u8]) -> Result<Self, Box<dyn std::error::Error>> {
            let root = std::env::temp_dir().join(format!(
                "model-rocket-claude-release-{}-{}",
                std::process::id(),
                uuid::Uuid::now_v7()
            ));
            let executable = root.join(version);
            fs::create_dir_all(&root)?;
            fs::write(&executable, bytes)?;
            let mut permissions = fs::metadata(&executable)?.permissions();
            permissions.set_mode(0o700);
            fs::set_permissions(&executable, permissions)?;
            Ok(Self { root, executable })
        }
    }

    impl Drop for TemporaryRelease {
        fn drop(&mut self) {
            let _removed = fs::remove_dir_all(&self.root);
        }
    }

    fn native_bytes() -> Vec<u8> {
        let mut bytes = if cfg!(target_os = "macos") {
            vec![0xcf, 0xfa, 0xed, 0xfe]
        } else {
            b"\x7fELF".to_vec()
        };
        bytes.extend_from_slice(b"synthetic official release");
        bytes
    }

    fn manifest(version: &str, bytes: &[u8]) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(&json!({
            "version": version,
            "platforms": {
                CLAUDE_PLATFORM: {
                    "binary": "claude",
                    "checksum": format!("{:x}", Sha256::digest(bytes)),
                    "size": bytes.len(),
                }
            }
        }))
    }

    #[test]
    fn future_official_release_is_accepted_from_its_manifest()
    -> Result<(), Box<dyn std::error::Error>> {
        let bytes = native_bytes();
        let release = TemporaryRelease::create("99.42.7", &bytes)?;
        let version = release_version(&release.executable)?;
        verify_release(&release.executable, &version, &manifest(&version, &bytes)?)?;
        assert_eq!(version, "99.42.7");
        Ok(())
    }

    #[test]
    fn non_numeric_release_version_fails_before_manifest_lookup()
    -> Result<(), Box<dyn std::error::Error>> {
        let release = TemporaryRelease::create("2.1.225-beta", &native_bytes())?;
        let error = release_version(&release.executable)
            .err()
            .ok_or("non-numeric Claude release was accepted")?;
        assert!(error.to_string().contains("invalid version"));
        Ok(())
    }

    #[test]
    fn tampered_release_fails_the_official_checksum() -> Result<(), Box<dyn std::error::Error>> {
        let original = native_bytes();
        let mut tampered = original.clone();
        tampered.extend_from_slice(b"tampered");
        let release = TemporaryRelease::create("2.1.999", &tampered)?;
        let error = verify_release(
            &release.executable,
            "2.1.999",
            &manifest("2.1.999", &original)?,
        )
        .err()
        .ok_or("tampered Claude release was accepted")?;
        assert!(
            error.to_string().contains("size does not match")
                || error.to_string().contains("digest does not match")
        );
        Ok(())
    }
}
