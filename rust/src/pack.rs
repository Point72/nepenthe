//! Air-gapped bundles: pack a lock's packages into one archive, install offline.
//!
//! [`pack`] resolves a lock, downloads every package it pins (verifying each
//! against the lock's sha256), and writes a single `.tar` bundle containing the
//! lock, a manifest, and every package archive. [`install_pack`] takes that
//! bundle to a disconnected host, rewrites each package URL to the bundle's
//! local copy, and installs into a prefix with **no network and no conda**.
//!
//! The bundle layout is:
//!
//! ```text
//! nepenthe-pack.yml        # manifest: format, environment, platforms, packages
//! environment.lock         # the lock that was packed
//! pkgs/<filename>          # every package archive (.conda / .tar.bz2)
//! ```
//!
//! Packages inside `.conda` archives are already compressed, so the outer tar
//! is left uncompressed.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::File;
use std::path::{Path, PathBuf};

use rattler_conda_types::RepoDataRecord;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

use crate::install::{self, InstallError, InstallSummary, LinkScripts};

/// Bundle format version written into the manifest.
const PACK_FORMAT: u32 = 1;
/// Manifest filename inside the bundle.
const MANIFEST_NAME: &str = "nepenthe-pack.yml";
/// Lock filename inside the bundle.
const LOCK_NAME: &str = "environment.lock";
/// Directory holding the package archives inside the bundle.
const PKGS_DIR: &str = "pkgs";

/// Errors raised while packing or installing a bundle.
#[derive(Debug)]
pub enum PackError {
    /// The environment resolved to no packages (unknown env or empty platform).
    EmptyEnvironment(String),
    /// The lock could not be parsed or lacks the requested environment/platform.
    Lock(String),
    /// Installing the bundle failed.
    Install(InstallError),
    /// Downloading a package failed.
    Download { url: String, message: String },
    /// A downloaded package did not match the lock's recorded hash.
    Integrity {
        file: String,
        expected: String,
        actual: String,
    },
    /// A package URL had no filename to pack under.
    BadUrl(String),
    /// The bundle is missing a package the lock requires.
    MissingPackage(String),
    /// The bundle declares a format version this build cannot read.
    UnsupportedFormat(u32),
    /// Reading or writing the bundle failed.
    Io(std::io::Error),
    /// Serialising the manifest failed.
    Manifest(String),
}

impl fmt::Display for PackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PackError::EmptyEnvironment(env) => {
                write!(f, "environment '{env}' has no packages to pack")
            }
            PackError::Lock(msg) => write!(f, "invalid lock: {msg}"),
            PackError::Install(e) => write!(f, "{e}"),
            PackError::Download { url, message } => {
                write!(f, "failed to download {url}: {message}")
            }
            PackError::Integrity {
                file,
                expected,
                actual,
            } => write!(
                f,
                "package {file} failed integrity check: expected sha256 {expected}, got {actual}"
            ),
            PackError::BadUrl(url) => write!(f, "package URL has no filename: {url}"),
            PackError::MissingPackage(file) => {
                write!(f, "bundle is missing package '{file}'")
            }
            PackError::UnsupportedFormat(v) => write!(
                f,
                "unsupported bundle format {v} (this build supports format {PACK_FORMAT})"
            ),
            PackError::Io(e) => write!(f, "filesystem error: {e}"),
            PackError::Manifest(msg) => write!(f, "manifest error: {msg}"),
        }
    }
}

impl std::error::Error for PackError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PackError::Install(e) => Some(e),
            PackError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<InstallError> for PackError {
    fn from(e: InstallError) -> Self {
        match e {
            InstallError::Lock(msg) => PackError::Lock(msg),
            other => PackError::Install(other),
        }
    }
}

impl From<std::io::Error> for PackError {
    fn from(e: std::io::Error) -> Self {
        PackError::Io(e)
    }
}

/// One package recorded in a bundle's manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackedPackage {
    /// The package archive filename (under `pkgs/`).
    pub file: String,
    /// The expected sha256 (lowercase hex), if the lock recorded one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// The package archive size in bytes.
    pub size: u64,
}

/// The manifest written at the root of a bundle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackManifest {
    /// Bundle format version.
    pub format: u32,
    /// The environment the bundle was packed for.
    pub environment: String,
    /// The platforms covered by the bundle.
    pub platforms: Vec<String>,
    /// Every package archive in the bundle.
    pub packages: Vec<PackedPackage>,
}

/// A summary of a completed [`pack`].
#[derive(Debug, Clone)]
pub struct PackSummary {
    /// The bundle that was written.
    pub output: PathBuf,
    /// The environment packed.
    pub environment: String,
    /// The platforms covered.
    pub platforms: Vec<String>,
    /// The number of unique packages bundled.
    pub packages: usize,
    /// The total size of the bundled package archives, in bytes.
    pub bytes: u64,
}

/// Pack the packages a lock pins for `environment` into a self-contained bundle
/// at `output`. When `platforms` is empty, every platform the lock covers for
/// the environment is included; otherwise only the listed platforms are.
///
/// Each package is downloaded from its channel and verified against the lock's
/// recorded sha256 before being written into the bundle. Performs network I/O;
/// await inside a tokio runtime.
pub async fn pack(
    lock_bytes: &[u8],
    environment: &str,
    platforms: &[String],
    output: &Path,
) -> Result<PackSummary, PackError> {
    let lock = install::parse_lock(lock_bytes)?;

    let platforms: Vec<String> = if platforms.is_empty() {
        install::lock_platforms(&lock, environment)?
    } else {
        platforms.to_vec()
    };
    if platforms.is_empty() {
        return Err(PackError::EmptyEnvironment(environment.to_string()));
    }

    // Gather the unique package archives across all requested platforms; a
    // multi-platform lock shares noarch packages between platforms.
    let mut seen = BTreeSet::new();
    let mut unique: Vec<(String, RepoDataRecord)> = Vec::new();
    for platform in &platforms {
        for record in install::lock_records(&lock, environment, platform)? {
            let filename = url_filename(&record.url)?;
            if seen.insert(filename.clone()) {
                unique.push((filename, record));
            }
        }
    }
    if unique.is_empty() {
        return Err(PackError::EmptyEnvironment(environment.to_string()));
    }

    let client = reqwest::Client::builder()
        .user_agent(concat!("nepenthe/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| PackError::Download {
            url: "<client>".to_string(),
            message: e.to_string(),
        })?;

    let file = File::create(output)?;
    let mut builder = tar::Builder::new(file);
    let mut manifest_packages = Vec::with_capacity(unique.len());
    let mut total_bytes: u64 = 0;

    for (filename, record) in &unique {
        let bytes = download(&client, &record.url).await?;
        let actual = verify_sha256(filename, record, &bytes)?;
        append_bytes(&mut builder, &format!("{PKGS_DIR}/{filename}"), &bytes)?;
        total_bytes += bytes.len() as u64;
        manifest_packages.push(PackedPackage {
            file: filename.clone(),
            sha256: actual,
            size: bytes.len() as u64,
        });
    }

    let manifest = PackManifest {
        format: PACK_FORMAT,
        environment: environment.to_string(),
        platforms: platforms.clone(),
        packages: manifest_packages,
    };
    let manifest_yaml =
        serde_yaml::to_string(&manifest).map_err(|e| PackError::Manifest(e.to_string()))?;

    append_bytes(&mut builder, LOCK_NAME, lock_bytes)?;
    append_bytes(&mut builder, MANIFEST_NAME, manifest_yaml.as_bytes())?;
    builder.finish()?;

    Ok(PackSummary {
        output: output.to_path_buf(),
        environment: environment.to_string(),
        platforms,
        packages: unique.len(),
        bytes: total_bytes,
    })
}

/// Read a bundle's manifest without extracting the whole archive.
pub fn read_manifest(pack_path: &Path) -> Result<PackManifest, PackError> {
    let file = File::open(pack_path)?;
    let mut archive = tar::Archive::new(file);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?;
        if path.as_ref() == Path::new(MANIFEST_NAME) {
            let mut text = String::new();
            std::io::Read::read_to_string(&mut entry, &mut text)?;
            let manifest: PackManifest =
                serde_yaml::from_str(&text).map_err(|e| PackError::Manifest(e.to_string()))?;
            if manifest.format != PACK_FORMAT {
                return Err(PackError::UnsupportedFormat(manifest.format));
            }
            return Ok(manifest);
        }
    }
    Err(PackError::Manifest(format!(
        "bundle has no {MANIFEST_NAME}"
    )))
}

/// Install an environment from a bundle into `prefix`, fully offline.
///
/// The bundle is extracted under `stage_dir` (a temporary directory is created
/// and removed afterwards when `stage_dir` is `None`), each package URL is
/// rewritten to the bundle's local copy, and the lock is installed with no
/// network. `environment` defaults to the bundle's environment and `platform`
/// to the current platform.
///
/// Await inside a tokio runtime.
pub async fn install_pack(
    pack_path: &Path,
    environment: Option<&str>,
    platform: Option<&str>,
    prefix: &Path,
    stage_dir: Option<&Path>,
    link_scripts: LinkScripts,
) -> Result<InstallSummary, PackError> {
    let (staging, created_temp) = match stage_dir {
        Some(dir) => (dir.to_path_buf(), false),
        None => {
            let dir = std::env::temp_dir().join(format!(
                "nepenthe-unpack-{}-{}",
                std::process::id(),
                unique_suffix()
            ));
            (dir, true)
        }
    };
    std::fs::create_dir_all(&staging)?;

    let result = install_from_staging(
        pack_path,
        &staging,
        environment,
        platform,
        prefix,
        link_scripts,
    )
    .await;

    if created_temp {
        let _ = std::fs::remove_dir_all(&staging);
    }
    result
}

async fn install_from_staging(
    pack_path: &Path,
    staging: &Path,
    environment: Option<&str>,
    platform: Option<&str>,
    prefix: &Path,
    link_scripts: LinkScripts,
) -> Result<InstallSummary, PackError> {
    tar::Archive::new(File::open(pack_path)?).unpack(staging)?;

    let manifest_text = std::fs::read_to_string(staging.join(MANIFEST_NAME))?;
    let manifest: PackManifest =
        serde_yaml::from_str(&manifest_text).map_err(|e| PackError::Manifest(e.to_string()))?;
    if manifest.format != PACK_FORMAT {
        return Err(PackError::UnsupportedFormat(manifest.format));
    }

    let environment = environment.unwrap_or(&manifest.environment);
    let platform = platform
        .map(str::to_string)
        .unwrap_or_else(crate::current_platform);

    let lock_bytes = std::fs::read(staging.join(LOCK_NAME))?;
    let lock = install::parse_lock(&lock_bytes)?;
    let mut records = install::lock_records(&lock, environment, &platform)?;

    // Index the manifest's declared package set so each bundled archive is
    // checked against a known filename, size, and hash before it reaches the
    // installer — tampered or unexpected bytes never get linked into a prefix.
    let declared: BTreeMap<&str, &PackedPackage> = manifest
        .packages
        .iter()
        .map(|p| (p.file.as_str(), p))
        .collect();

    let pkgs = staging.join(PKGS_DIR);
    for record in &mut records {
        let filename = url_filename(&record.url)?;
        let entry = declared
            .get(filename.as_str())
            .ok_or_else(|| PackError::MissingPackage(filename.clone()))?;
        let path = pkgs.join(&filename);
        if !path.exists() {
            return Err(PackError::MissingPackage(filename.clone()));
        }
        let bytes = std::fs::read(&path)?;
        if bytes.len() as u64 != entry.size {
            return Err(PackError::Integrity {
                file: filename.clone(),
                expected: format!("{} bytes", entry.size),
                actual: format!("{} bytes", bytes.len()),
            });
        }
        // Verify against the lock's recorded hash, then the manifest's.
        let actual = verify_sha256(&filename, record, &bytes)?;
        if let (Some(expected), Some(actual)) = (entry.sha256.as_ref(), actual.as_ref()) {
            if expected != actual {
                return Err(PackError::Integrity {
                    file: filename.clone(),
                    expected: expected.clone(),
                    actual: actual.clone(),
                });
            }
        }
        record.url = Url::from_file_path(&path)
            .map_err(|()| PackError::BadUrl(path.display().to_string()))?;
    }

    install::install_records(records, environment, &platform, prefix, link_scripts)
        .await
        .map_err(PackError::from)
}

/// Download a package archive into memory.
async fn download(client: &reqwest::Client, url: &Url) -> Result<Vec<u8>, PackError> {
    let response = client
        .get(url.clone())
        .send()
        .await
        .map_err(|e| PackError::Download {
            url: url.to_string(),
            message: e.to_string(),
        })?;
    let response = response
        .error_for_status()
        .map_err(|e| PackError::Download {
            url: url.to_string(),
            message: e.to_string(),
        })?;
    response
        .bytes()
        .await
        .map(|b| b.to_vec())
        .map_err(|e| PackError::Download {
            url: url.to_string(),
            message: e.to_string(),
        })
}

/// Verify `bytes` against the record's recorded sha256, returning the actual
/// hash (lowercase hex). When the lock recorded no hash, returns `None` and
/// skips the check.
fn verify_sha256(
    filename: &str,
    record: &RepoDataRecord,
    bytes: &[u8],
) -> Result<Option<String>, PackError> {
    let actual = hex::encode(Sha256::digest(bytes));
    if let Some(expected) = record.package_record.sha256.as_ref() {
        let expected = hex::encode(expected);
        if expected != actual {
            return Err(PackError::Integrity {
                file: filename.to_string(),
                expected,
                actual,
            });
        }
    }
    Ok(Some(actual))
}

/// The filename of a package archive from its canonical URL (the last path
/// segment).
fn url_filename(url: &Url) -> Result<String, PackError> {
    url.path_segments()
        .and_then(|mut segments| segments.next_back())
        .filter(|segment| !segment.is_empty())
        .map(|segment| segment.to_string())
        .ok_or_else(|| PackError::BadUrl(url.to_string()))
}

/// Append in-memory `data` to the tar `builder` at `path`. `append_data` sets
/// the entry path and recomputes the checksum.
fn append_bytes<W: std::io::Write>(
    builder: &mut tar::Builder<W>,
    path: &str,
    data: &[u8],
) -> std::io::Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_size(data.len() as u64);
    header.set_mode(0o644);
    header.set_mtime(0);
    builder.append_data(&mut header, path, data)
}

/// A short, process-unique suffix for the temporary staging directory.
fn unique_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_manifest_rejects_unknown_format() {
        let manifest = PackManifest {
            format: PACK_FORMAT + 1,
            environment: "app".into(),
            platforms: vec!["linux-64".into()],
            packages: vec![],
        };
        let yaml = serde_yaml::to_string(&manifest).unwrap();
        let path =
            std::env::temp_dir().join(format!("nepenthe-pack-fmt-{}.tar", std::process::id()));
        {
            let file = File::create(&path).unwrap();
            let mut builder = tar::Builder::new(file);
            append_bytes(&mut builder, MANIFEST_NAME, yaml.as_bytes()).unwrap();
            builder.finish().unwrap();
        }
        assert!(matches!(
            read_manifest(&path),
            Err(PackError::UnsupportedFormat(v)) if v == PACK_FORMAT + 1
        ));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn url_filename_takes_last_segment() {
        let url =
            Url::parse("https://conda.anaconda.org/conda-forge/linux-64/numpy-2.1.0-py311h0.conda")
                .unwrap();
        assert_eq!(url_filename(&url).unwrap(), "numpy-2.1.0-py311h0.conda");
    }

    #[test]
    fn url_filename_rejects_directory_url() {
        let url = Url::parse("https://conda.anaconda.org/conda-forge/linux-64/").unwrap();
        assert!(url_filename(&url).is_err());
    }

    #[test]
    fn manifest_round_trips_through_yaml() {
        let manifest = PackManifest {
            format: PACK_FORMAT,
            environment: "app".to_string(),
            platforms: vec!["linux-64".to_string(), "osx-arm64".to_string()],
            packages: vec![
                PackedPackage {
                    file: "numpy-2.1.0-py311h0.conda".to_string(),
                    sha256: Some("abcdef".to_string()),
                    size: 1234,
                },
                PackedPackage {
                    file: "python-3.11.5-h0.conda".to_string(),
                    sha256: None,
                    size: 5678,
                },
            ],
        };
        let yaml = serde_yaml::to_string(&manifest).unwrap();
        let parsed: PackManifest = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed, manifest);
    }

    #[test]
    fn tar_round_trips_in_memory_bytes() {
        // append_bytes writes a valid entry that reads back unchanged.
        let mut buffer = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut buffer);
            append_bytes(&mut builder, "pkgs/a.conda", b"hello").unwrap();
            append_bytes(&mut builder, MANIFEST_NAME, b"format: 1").unwrap();
            builder.finish().unwrap();
        }
        let mut archive = tar::Archive::new(std::io::Cursor::new(buffer));
        let mut found = std::collections::BTreeMap::new();
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            let path = entry.path().unwrap().display().to_string();
            let mut contents = Vec::new();
            std::io::Read::read_to_end(&mut entry, &mut contents).unwrap();
            found.insert(path, contents);
        }
        assert_eq!(
            found.get("pkgs/a.conda").map(|v| v.as_slice()),
            Some(&b"hello"[..])
        );
        assert_eq!(
            found.get(MANIFEST_NAME).map(|v| v.as_slice()),
            Some(&b"format: 1"[..])
        );
    }

    /// Capstone: solve python live, export a lock, pack every package into a
    /// bundle, then install that bundle into a prefix from its **local** copies
    /// (URLs rewritten to `file://`, no channel access at install time).
    /// Ignored by default so CI stays offline; run with `cargo test -- --ignored`.
    #[ignore = "requires network access to conda channels and links a real prefix"]
    #[tokio::test]
    async fn real_pack_then_install_offline() {
        use crate::export::to_lockfile_string;
        use crate::solve::{solve, ChannelSettings, SolveRequest};
        use rattler_conda_types::Platform;

        let platform = Platform::current().to_string();

        // 1) solve a tiny environment and export a lock
        let request = SolveRequest {
            channels: vec!["conda-forge".to_string()],
            platform: platform.clone(),
            specs: vec!["python 3.11.*".to_string()],
            ..Default::default()
        };
        let outcome = solve(&request, &ChannelSettings::default())
            .await
            .expect("solve should succeed");
        let lock_yaml = to_lockfile_string(&outcome, "app").expect("render lock");

        // 2) pack the lock's packages into a bundle (downloads + verifies)
        let base = std::env::temp_dir().join(format!("nepenthe-pack-capstone-{}", unique_suffix()));
        std::fs::create_dir_all(&base).expect("temp dir");
        let bundle = base.join("app.tar");
        let summary = pack(lock_yaml.as_bytes(), "app", &[], &bundle)
            .await
            .expect("pack should succeed");
        assert!(summary.packages > 0);
        assert!(bundle.exists());

        // the manifest is readable without a full extract
        let manifest = read_manifest(&bundle).expect("read manifest");
        assert_eq!(manifest.environment, "app");
        assert_eq!(manifest.packages.len(), summary.packages);

        // 3) install from the bundle into a fresh prefix — offline
        let prefix = base.join("env");
        let install = install_pack(&bundle, None, None, &prefix, None, LinkScripts::Skip)
            .await
            .expect("install from bundle should succeed");
        assert!(install.packages.iter().any(|p| p.name == "python"));

        // 4) the installed prefix matches the packed lock
        let lock = install::parse_lock(lock_yaml.as_bytes()).expect("parse lock");
        assert!(install::diff(&lock, "app", &platform, &prefix)
            .expect("diff")
            .is_empty());

        let _ = std::fs::remove_dir_all(&base);
    }
}
