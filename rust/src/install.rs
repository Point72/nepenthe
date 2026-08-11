//! Install / download side.
//!
//! Turns a published lock into a usable environment on disk **without requiring
//! conda** — the heavy lifting (fetching packages, linking them into a prefix,
//! patching hardcoded paths) is done by rattler's installer. This module also
//! covers the read-only lifecycle commands that compare and inspect a prefix
//! against its lock (`diff`, `status`), prefix removal, and cross-platform
//! activation-script generation via `rattler_shell`.
//!
//! The pure data operations — extracting a lock's package set, diffing it
//! against an installed prefix, reading a prefix's installed packages — are
//! synchronous and run fully offline. Only [`install_lock`] (and the
//! registry-driven [`create`]) touch the network, to fetch packages.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use rattler_conda_types::{Platform, PrefixRecord, RepoDataRecord};
use rattler_lock::LockFile;
use rattler_shell::activation::{ActivationVariables, Activator, PathModificationBehavior};
use rattler_shell::shell::ShellEnum;

use crate::registry::{Coordinates, Label, Registry, RegistryError};

/// Errors raised by the install side.
#[derive(Debug)]
pub enum InstallError {
    /// The lock could not be parsed, or lacks the requested environment/platform.
    Lock(String),
    /// A registry lookup or pull failed.
    Registry(RegistryError),
    /// The rattler installer failed.
    Install(String),
    /// A filesystem operation failed.
    Io(std::io::Error),
    /// Generating an activation script failed.
    Activation(String),
    /// The prefix is a protected location (filesystem root, home, or current
    /// directory) and was refused.
    UnsafePrefix(String),
    /// The prefix does not look like an environment (no `conda-meta`) and the
    /// removal was not forced.
    NotAnEnvironment(String),
}

impl fmt::Display for InstallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InstallError::Lock(msg) => write!(f, "invalid lock: {msg}"),
            InstallError::Registry(e) => write!(f, "registry failed: {e}"),
            InstallError::Install(msg) => write!(f, "install failed: {msg}"),
            InstallError::Io(e) => write!(f, "filesystem error: {e}"),
            InstallError::Activation(msg) => write!(f, "activation failed: {msg}"),
            InstallError::UnsafePrefix(p) => write!(f, "refusing to remove protected path: {p}"),
            InstallError::NotAnEnvironment(p) => write!(
                f,
                "{p} does not look like an environment (no conda-meta); pass --force to remove anyway"
            ),
        }
    }
}

impl std::error::Error for InstallError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            InstallError::Registry(e) => Some(e),
            InstallError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<RegistryError> for InstallError {
    fn from(e: RegistryError) -> Self {
        InstallError::Registry(e)
    }
}

impl From<std::io::Error> for InstallError {
    fn from(e: std::io::Error) -> Self {
        InstallError::Io(e)
    }
}

/// The identity of one package: name, version, and build string. Used to
/// compare a lock's desired package set against what is installed in a prefix.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct PackageId {
    /// Normalized package name.
    pub name: String,
    /// Package version.
    pub version: String,
    /// Build string.
    pub build: String,
}

impl PackageId {
    fn from_record(record: &RepoDataRecord) -> Self {
        let pr = &record.package_record;
        Self {
            name: pr.name.as_normalized().to_string(),
            version: pr.version.as_str().to_string(),
            build: pr.build.clone(),
        }
    }
}

impl fmt::Display for PackageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}={}={}", self.name, self.version, self.build)
    }
}

/// Extract the binary conda packages a lock pins for one `environment` on one
/// `platform`, as installable [`RepoDataRecord`]s.
pub fn lock_records(
    lock: &LockFile,
    environment: &str,
    platform: &str,
) -> Result<Vec<RepoDataRecord>, InstallError> {
    let subdir = Platform::from_str(platform)
        .map_err(|e| InstallError::Lock(format!("bad platform '{platform}': {e}")))?;
    let env = lock
        .environment(environment)
        .ok_or_else(|| InstallError::Lock(format!("lock has no environment '{environment}'")))?;
    let handle = env
        .platforms()
        .find(|p| p.subdir() == subdir)
        .ok_or_else(|| {
            InstallError::Lock(format!(
                "environment '{environment}' has no platform '{platform}'"
            ))
        })?;
    env.conda_repodata_records(handle)
        .map_err(|e| InstallError::Lock(format!("converting lock records: {e}")))?
        .ok_or_else(|| {
            InstallError::Lock(format!(
                "environment '{environment}' has no packages for '{platform}'"
            ))
        })
}

/// The platforms a lock covers for one `environment`, as conda subdir strings.
pub fn lock_platforms(lock: &LockFile, environment: &str) -> Result<Vec<String>, InstallError> {
    let env = lock
        .environment(environment)
        .ok_or_else(|| InstallError::Lock(format!("lock has no environment '{environment}'")))?;
    Ok(env.platforms().map(|p| p.subdir().to_string()).collect())
}

/// The sorted package set a lock pins for one `environment` on one `platform`.
pub fn lock_packages(
    lock: &LockFile,
    environment: &str,
    platform: &str,
) -> Result<Vec<PackageId>, InstallError> {
    let mut ids: Vec<PackageId> = lock_records(lock, environment, platform)?
        .iter()
        .map(PackageId::from_record)
        .collect();
    ids.sort();
    Ok(ids)
}

/// Read the sorted package set currently installed in `prefix` (from its
/// `conda-meta` directory). An empty vec is returned for a non-existent or
/// empty prefix.
pub fn prefix_packages(prefix: &Path) -> Result<Vec<PackageId>, InstallError> {
    let records: Vec<PrefixRecord> = PrefixRecord::collect_from_prefix(prefix)?;
    let mut ids: Vec<PackageId> = records
        .iter()
        .map(|r| PackageId::from_record(&r.repodata_record))
        .collect();
    ids.sort();
    Ok(ids)
}

/// The difference between a lock's desired package set and a prefix's installed
/// set: packages to add, packages to remove, and packages whose version or
/// build changed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InstallDiff {
    /// In the lock, not installed.
    pub added: Vec<PackageId>,
    /// Installed, not in the lock.
    pub removed: Vec<PackageId>,
    /// Same name, different version/build: `(installed, desired)`.
    pub changed: Vec<(PackageId, PackageId)>,
}

impl InstallDiff {
    /// Whether the prefix already matches the lock (nothing to add, remove, or
    /// change).
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }
}

/// Compute the diff between a `desired` package set (from a lock) and an
/// `installed` set (from a prefix), keyed by package name. Pure and offline.
pub fn diff_packages(desired: &[PackageId], installed: &[PackageId]) -> InstallDiff {
    let desired_by_name: BTreeMap<&str, &PackageId> =
        desired.iter().map(|p| (p.name.as_str(), p)).collect();
    let installed_by_name: BTreeMap<&str, &PackageId> =
        installed.iter().map(|p| (p.name.as_str(), p)).collect();

    let mut diff = InstallDiff::default();

    for (name, want) in &desired_by_name {
        match installed_by_name.get(name) {
            None => diff.added.push((*want).clone()),
            Some(have) if have.version != want.version || have.build != want.build => {
                diff.changed.push(((*have).clone(), (*want).clone()));
            }
            Some(_) => {}
        }
    }
    for (name, have) in &installed_by_name {
        if !desired_by_name.contains_key(name) {
            diff.removed.push((*have).clone());
        }
    }

    diff.added.sort();
    diff.removed.sort();
    diff.changed.sort();
    diff
}

/// Compare a `prefix` against the lock's `environment`/`platform` package set.
pub fn diff(
    lock: &LockFile,
    environment: &str,
    platform: &str,
    prefix: &Path,
) -> Result<InstallDiff, InstallError> {
    let desired = lock_packages(lock, environment, platform)?;
    let installed = prefix_packages(prefix)?;
    Ok(diff_packages(&desired, &installed))
}

/// A summary of what is installed in a prefix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrefixStatus {
    /// The prefix path.
    pub prefix: PathBuf,
    /// Whether the prefix exists and has a `conda-meta` directory.
    pub exists: bool,
    /// The installed package set, sorted.
    pub packages: Vec<PackageId>,
}

/// Report the install status of `prefix`: whether it exists and what it
/// contains.
pub fn status(prefix: &Path) -> Result<PrefixStatus, InstallError> {
    let exists = prefix.join("conda-meta").is_dir();
    let packages = if exists {
        prefix_packages(prefix)?
    } else {
        Vec::new()
    };
    Ok(PrefixStatus {
        prefix: prefix.to_path_buf(),
        exists,
        packages,
    })
}

/// Remove an environment prefix. A non-existent prefix is a no-op.
///
/// Refuses to delete obviously dangerous locations — a filesystem root, the
/// user's home directory, or the current working directory — regardless of
/// `force`. Unless `force` is set, the prefix must contain a `conda-meta`
/// directory, so a mistyped `--prefix` cannot recursively delete an unrelated
/// tree.
pub fn remove_prefix(prefix: &Path, force: bool) -> Result<(), InstallError> {
    if !prefix.exists() {
        return Ok(());
    }
    let canonical = prefix.canonicalize().map_err(InstallError::Io)?;
    if is_protected_path(&canonical) {
        return Err(InstallError::UnsafePrefix(canonical.display().to_string()));
    }
    if !force && !canonical.join("conda-meta").is_dir() {
        return Err(InstallError::NotAnEnvironment(
            canonical.display().to_string(),
        ));
    }
    std::fs::remove_dir_all(&canonical)?;
    Ok(())
}

/// Whether `path` (already canonicalized) is too dangerous to remove
/// recursively: a filesystem root, the user's home directory, or the current
/// working directory.
fn is_protected_path(path: &Path) -> bool {
    if path.parent().is_none() {
        return true;
    }
    for var in ["HOME", "USERPROFILE"] {
        if let Some(home) = std::env::var_os(var).filter(|h| !h.is_empty()) {
            if Path::new(&home).canonicalize().ok().as_deref() == Some(path) {
                return true;
            }
        }
    }
    if std::env::current_dir()
        .and_then(|cwd| cwd.canonicalize())
        .ok()
        .as_deref()
        == Some(path)
    {
        return true;
    }
    false
}

/// A summary of an install: the prefix and the package set now pinned there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstallSummary {
    /// The prefix the environment was installed into.
    pub prefix: PathBuf,
    /// The environment name installed.
    pub environment: String,
    /// The platform installed.
    pub platform: String,
    /// The package set installed (from the lock).
    pub packages: Vec<PackageId>,
}

/// Install the `environment`/`platform` packages from `lock` into `prefix`,
/// using rattler's installer (no conda required). Packages are fetched into the
/// shared package cache and linked into the prefix.
///
/// This performs network I/O, so it must be awaited inside a tokio runtime.
pub async fn install_lock(
    lock: &LockFile,
    environment: &str,
    platform: &str,
    prefix: &Path,
) -> Result<InstallSummary, InstallError> {
    let records = lock_records(lock, environment, platform)?;
    install_records(records, environment, platform, prefix).await
}

/// Render `error` and its `source` chain as `outer: cause: root cause`.
///
/// `thiserror` renders only the outermost message, so flattening a rattler
/// error into a `String` otherwise drops why it failed: `failed to fetch
/// <package>` keeps the package name but discards the HTTP status, timeout or
/// checksum mismatch underneath it.
fn error_chain(error: &dyn std::error::Error) -> String {
    let mut message = error.to_string();
    let mut next = error.source();
    while let Some(cause) = next {
        let text = cause.to_string();
        // `#[error(transparent)]` repeats its source verbatim; don't say it twice.
        if !message.ends_with(&text) {
            message.push_str(": ");
            message.push_str(&text);
        }
        next = cause.source();
    }
    message
}

/// How many packages may be fetched into the package cache at once.
///
/// rattler leaves this unbounded, so every package in the environment can be
/// in flight simultaneously and each one holds a `.lock` file open in the
/// package cache. File-descriptor use then scales with environment size and a
/// large environment exhausts `RLIMIT_NOFILE`, surfacing as a fetch failure:
///
/// ```text
/// failed to fetch daft-0.1.3-pyhd8ed1ab_0.conda: ... failed to open cache
/// metadata file: '.../daft-0.1.3-pyhd8ed1ab_0.lock': Too many open files
/// ```
///
/// Bounding it decouples descriptor use from package count. Downloads are
/// network-bound well before this many are in flight, so it costs no
/// meaningful throughput.
const MAX_CONCURRENT_FETCHES: usize = 50;

/// Descriptors to ask for before installing, capped by the hard limit.
///
/// rattler's package cache holds one `.lock` file open per package for the
/// duration of an install, so descriptor use tracks environment size — a
/// 1277-package environment peaks around 1300 open descriptors. Against the
/// common 1024 soft limit that fails partway through as:
///
/// ```text
/// failed to fetch <pkg>: ... failed to open cache metadata file:
/// '.../<pkg>.lock': Too many open files (os error 24)
/// ```
///
/// Well clear of any environment we publish, with room for growth.
const DESIRED_OPEN_FILES: u64 = 65536;

/// Raise this process's soft `RLIMIT_NOFILE` toward [`DESIRED_OPEN_FILES`].
///
/// A process may raise its own soft limit up to the hard limit without
/// privileges, so doing it here fixes every caller — the CLI, the Python
/// module, and anything embedding this crate — rather than requiring each
/// container image and CI job to set `ulimit` for itself.
///
/// Best effort: the hard limit is the ceiling and cannot be raised without
/// privileges, so a process confined to a low hard limit is left as it was and
/// the install fails with the error above rather than something more obscure.
#[cfg(unix)]
fn raise_open_file_limit() {
    // SAFETY: both calls take a valid, correctly sized `rlimit` and are checked
    // for failure. No invariant of the caller depends on the outcome.
    unsafe {
        let mut limit: libc::rlimit = std::mem::zeroed();
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) != 0 {
            return;
        }
        let target = (DESIRED_OPEN_FILES as libc::rlim_t).min(limit.rlim_max);
        if limit.rlim_cur >= target {
            return;
        }
        let raised = libc::rlimit {
            rlim_cur: target,
            rlim_max: limit.rlim_max,
        };
        libc::setrlimit(libc::RLIMIT_NOFILE, &raised);
    }
}

/// No-op: Windows has no `RLIMIT_NOFILE`.
#[cfg(not(unix))]
fn raise_open_file_limit() {}

/// Install pre-extracted `records` for one `environment`/`platform` into
/// `prefix` with rattler's installer (no conda required). Records whose `url`
/// is a `file://` path are read locally (no network) — this is what lets a
/// [packed bundle](crate::pack) install fully offline.
///
/// Performs I/O (and network for non-`file://` records); await inside a tokio
/// runtime.
pub async fn install_records(
    records: Vec<RepoDataRecord>,
    environment: &str,
    platform: &str,
    prefix: &Path,
) -> Result<InstallSummary, InstallError> {
    let target = Platform::from_str(platform)
        .map_err(|e| InstallError::Lock(format!("bad platform '{platform}': {e}")))?;
    raise_open_file_limit();
    let packages = {
        let mut ids: Vec<PackageId> = records.iter().map(PackageId::from_record).collect();
        ids.sort();
        ids
    };

    rattler::install::Installer::new()
        .with_download_client(crate::net::authenticated_client().map_err(InstallError::Install)?)
        .with_max_concurrent_requests(MAX_CONCURRENT_FETCHES)
        .with_target_platform(target)
        .install(prefix, records)
        .await
        .map_err(|e| InstallError::Install(error_chain(&e)))?;

    Ok(InstallSummary {
        prefix: prefix.to_path_buf(),
        environment: environment.to_string(),
        platform: platform.to_string(),
        packages,
    })
}

/// Parse lock bytes (the YAML produced by the exporter / registry) into a
/// [`LockFile`].
pub fn parse_lock(bytes: &[u8]) -> Result<LockFile, InstallError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|e| InstallError::Lock(format!("lock is not utf-8: {e}")))?;
    LockFile::from_str_with_base_directory(text, None)
        .map_err(|e| InstallError::Lock(format!("parsing lock: {e}")))
}

/// Resolve `label` within `coords` in a registry, pull the lock, and install it
/// into `prefix`. Ties the registry to the installer (resolve → pull → install).
///
/// Performs network I/O; await inside a tokio runtime.
pub async fn create(
    registry: &Registry,
    coords: &Coordinates,
    label: &Label,
    prefix: &Path,
) -> Result<InstallSummary, InstallError> {
    let bytes = registry.pull(coords, label)?;
    let lock = parse_lock(&bytes)?;
    let summary = install_lock(&lock, &coords.environment, &coords.platform, prefix).await?;
    // Materialize the environment's activation hooks, recovered from the
    // manifest the lock was solved from: the embedded comment band if present,
    // else the registry's manifest sidecar. Best-effort: a release with no
    // recoverable manifest simply gets no hooks.
    let manifest_yaml = crate::embed::extract_manifest(&bytes)
        .ok()
        .flatten()
        .or_else(|| {
            registry
                .pull_manifest(coords, label)
                .ok()
                .flatten()
                .and_then(|b| String::from_utf8(b).ok())
        });
    if let Some(yaml) = manifest_yaml {
        if let Ok(manifest) = crate::manifest::Manifest::from_yaml_str(&yaml) {
            let selector = crate::manifest::Selector {
                variant: coords.variant.clone(),
                python: coords.python.clone(),
            };
            if let Ok(resolved) = manifest.resolve(&coords.environment, &selector) {
                let version = registry.resolve(coords, label).ok().map(|r| r.version);
                write_activation_hooks(
                    prefix,
                    &coords.environment,
                    version.as_deref(),
                    &coords.platform,
                    &resolved.activation,
                )?;
            }
        }
    }
    Ok(summary)
}

/// The name of the lock's only environment, or an error if it declares none or
/// several. Lets a file-based install infer the environment when the caller
/// omits it — a per-cell lock written by `build` has exactly one.
pub fn sole_environment(lock: &LockFile) -> Result<String, InstallError> {
    let names: Vec<String> = lock
        .environments()
        .map(|(name, _)| name.to_string())
        .collect();
    match names.as_slice() {
        [only] => Ok(only.clone()),
        [] => Err(InstallError::Lock("lock declares no environments".into())),
        _ => Err(InstallError::Lock(format!(
            "lock declares multiple environments ({}); pass one explicitly",
            names.join(", ")
        ))),
    }
}

/// Materialize activation hooks recovered from a lock's embedded manifest band
/// (best-effort, no registry). Composing the hooks does not solve packages. When
/// the lock carries no embedded manifest, or it declares no hooks, nothing is
/// written. `version` is optional: a bare lock file (unlike a registry release)
/// carries no version label.
pub fn write_hooks_from_lock(
    lock_bytes: &[u8],
    environment: &str,
    platform: &str,
    python: Option<&str>,
    variant: Option<&str>,
    version: Option<&str>,
    prefix: &Path,
) -> Result<(), InstallError> {
    let Some(yaml) = crate::embed::extract_manifest(lock_bytes).ok().flatten() else {
        return Ok(());
    };
    let Ok(manifest) = crate::manifest::Manifest::from_yaml_str(&yaml) else {
        return Ok(());
    };
    let selector = crate::manifest::Selector {
        variant: variant.map(String::from),
        python: python.map(String::from),
    };
    if let Ok(resolved) = manifest.resolve(environment, &selector) {
        write_activation_hooks(prefix, environment, version, platform, &resolved.activation)?;
    }
    Ok(())
}

/// Materialize an environment's [activation hooks](crate::manifest::Activation)
/// into `prefix`'s `etc/conda/activate.d/` so a full activation runs them.
///
/// Writes a single `nepenthe-activate.{sh,bat}` (shell for `platform`) that
/// exports the hook's env vars and runs its scripts. nepenthe always injects the
/// environment's identity (`NEPENTHE_ENVIRONMENT`, `NEPENTHE_PLATFORM`, and
/// `NEPENTHE_VERSION` when known) so hooks can reference it. When the manifest
/// declares no hooks, nothing is written.
pub fn write_activation_hooks(
    prefix: &Path,
    environment: &str,
    version: Option<&str>,
    platform: &str,
    activation: &crate::manifest::Activation,
) -> Result<(), InstallError> {
    if activation.is_empty() {
        return Ok(());
    }
    let is_windows = Platform::from_str(platform)
        .map(|p| p.is_windows())
        .unwrap_or(cfg!(windows));

    // Identity vars first, then the manifest's declared env (declared wins).
    let mut env: Vec<(String, String)> = vec![
        ("NEPENTHE_ENVIRONMENT".to_string(), environment.to_string()),
        ("NEPENTHE_PLATFORM".to_string(), platform.to_string()),
    ];
    if let Some(v) = version {
        env.push(("NEPENTHE_VERSION".to_string(), v.to_string()));
    }
    for (k, v) in &activation.env {
        env.push((k.clone(), v.clone()));
    }

    let dir = prefix.join("etc").join("conda").join("activate.d");
    std::fs::create_dir_all(&dir).map_err(InstallError::Io)?;
    let (file, body) = if is_windows {
        let mut body = String::from("@echo off\r\n");
        for (k, v) in &env {
            body.push_str(&format!("set \"{k}={v}\"\r\n"));
        }
        for line in &activation.scripts {
            body.push_str(line);
            body.push_str("\r\n");
        }
        ("nepenthe-activate.bat", body)
    } else {
        let mut body = String::from("#!/bin/sh\n");
        for (k, v) in &env {
            body.push_str(&format!("export {k}={}\n", sh_single_quote(v)));
        }
        for line in &activation.scripts {
            body.push_str(line);
            body.push('\n');
        }
        ("nepenthe-activate.sh", body)
    };
    std::fs::write(dir.join(file), body).map_err(InstallError::Io)
}

/// Quote a value for safe use in a POSIX `export KEY=<value>` line by wrapping
/// it in single quotes (escaping any embedded single quote as `'\''`).
fn sh_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Generate an activation script for `prefix`, targeting `shell` on `platform`.
/// The script sets `PATH`, exports the environment's variables, and runs its
/// `activate.d` hooks — cross-platform, with no conda required.
pub fn activation_script(
    prefix: &Path,
    shell: ShellEnum,
    platform: &str,
) -> Result<String, InstallError> {
    let target = Platform::from_str(platform)
        .map_err(|e| InstallError::Activation(format!("bad platform '{platform}': {e}")))?;
    let activator = Activator::from_path(prefix, shell, target)
        .map_err(|e| InstallError::Activation(e.to_string()))?;
    let variables = ActivationVariables {
        conda_prefix: None,
        path: None,
        path_modification_behavior: PathModificationBehavior::Prepend,
        current_env: Default::default(),
    };
    let result = activator
        .activation(variables)
        .map_err(|e| InstallError::Activation(e.to_string()))?;
    result
        .script
        .contents()
        .map_err(|e| InstallError::Activation(e.to_string()))
}

/// Parse a shell name (e.g. `bash`, `zsh`, `powershell`) into a [`ShellEnum`].
pub fn parse_shell(name: &str) -> Result<ShellEnum, InstallError> {
    use rattler_shell::shell::{Bash, CmdExe, Fish, NuShell, PowerShell, Xonsh, Zsh};
    Ok(match name.to_lowercase().as_str() {
        "bash" => Bash::default().into(),
        "zsh" => Zsh.into(),
        "fish" => Fish.into(),
        "xonsh" => Xonsh.into(),
        "cmd" | "cmdexe" => CmdExe.into(),
        "powershell" | "pwsh" => PowerShell::default().into(),
        "nu" | "nushell" => NuShell.into(),
        other => return Err(InstallError::Activation(format!("unknown shell '{other}'"))),
    })
}

/// Like [`activation_script`], but resolve the shell from a name (or the current
/// shell when `None`). Centralizes shell selection for the CLI and the Python
/// binding.
pub fn activation_script_for(
    prefix: &Path,
    shell: Option<&str>,
    platform: &str,
) -> Result<String, InstallError> {
    let shell = match shell {
        None => ShellEnum::from_env().unwrap_or_default(),
        Some(name) => parse_shell(name)?,
    };
    activation_script(prefix, shell, platform)
}

/// The directories a conda `prefix` puts on `PATH` for `platform`, most-specific
/// first. On Windows the conda layout exposes several `Library`/`Scripts` dirs;
/// elsewhere it is just `<prefix>/bin`.
pub fn prefix_path_entries(prefix: &Path, platform: &str) -> Vec<PathBuf> {
    let is_windows = Platform::from_str(platform)
        .map(|p| p.is_windows())
        .unwrap_or(cfg!(windows));
    if is_windows {
        vec![
            prefix.to_path_buf(),
            prefix.join("Library").join("mingw-w64").join("bin"),
            prefix.join("Library").join("usr").join("bin"),
            prefix.join("Library").join("bin"),
            prefix.join("Scripts"),
            prefix.join("bin"),
        ]
    } else {
        vec![prefix.join("bin")]
    }
}

/// The path to the Python interpreter inside a conda `prefix` for `platform`.
/// On Windows the interpreter sits at the prefix root; elsewhere in `bin/`.
pub fn prefix_python(prefix: &Path, platform: &str) -> PathBuf {
    let is_windows = Platform::from_str(platform)
        .map(|p| p.is_windows())
        .unwrap_or(cfg!(windows));
    if is_windows {
        prefix.join("python.exe")
    } else {
        prefix.join("bin").join("python")
    }
}

/// Run `program` (with `args`) inside `prefix`, activated for `platform`,
/// inheriting the current stdio, and wait for it to finish.
///
/// The environment is built deterministically: the prefix's `bin` directories
/// are prepended to `PATH`, `CONDA_PREFIX` is set, and `extra_pythonpath`
/// entries are prepended to `PYTHONPATH` (for editable / working-tree overlays).
/// `activate.d` hook scripts are **not** run — this is path activation, which
/// covers running the environment's interpreter and tools.
pub fn exec_in_prefix(
    prefix: &Path,
    platform: &str,
    program: &std::ffi::OsStr,
    args: &[std::ffi::OsString],
    extra_pythonpath: &[PathBuf],
) -> Result<std::process::ExitStatus, InstallError> {
    use std::env::{join_paths, split_paths, var_os};

    let mut path_entries = prefix_path_entries(prefix, platform);
    if let Some(existing) = var_os("PATH") {
        path_entries.extend(split_paths(&existing));
    }
    let path = join_paths(&path_entries)
        .map_err(|e| InstallError::Activation(format!("building PATH: {e}")))?;

    let mut command = std::process::Command::new(program);
    command.args(args);
    command.env("PATH", path);
    command.env("CONDA_PREFIX", prefix);

    if !extra_pythonpath.is_empty() {
        let mut py_entries: Vec<PathBuf> = extra_pythonpath.to_vec();
        if let Some(existing) = var_os("PYTHONPATH") {
            py_entries.extend(split_paths(&existing));
        }
        let pythonpath = join_paths(&py_entries)
            .map_err(|e| InstallError::Activation(format!("building PYTHONPATH: {e}")))?;
        command.env("PYTHONPATH", pythonpath);
    }

    command.status().map_err(InstallError::Io)
}

/// Clone the environment prefix `src` to a new prefix `dst` using copy-on-write
/// where the filesystem supports it (reflinks), falling back to a full recursive
/// copy otherwise.
///
/// On a reflink-capable filesystem (btrfs, XFS with reflink, APFS) the clone is
/// near-instant and space-efficient — shared blocks are copied only on write.
/// On filesystems without reflink support (e.g. NFS, ext4) this is a regular
/// copy. `dst` must not already exist.
pub fn clone_prefix(src: &Path, dst: &Path) -> Result<(), InstallError> {
    #[cfg(unix)]
    {
        // GNU cp: reflink where possible, plain copy otherwise, in one pass.
        let ok = std::process::Command::new("cp")
            .arg("--reflink=auto")
            .arg("-a")
            .arg("-T")
            .arg(src)
            .arg(dst)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            return Ok(());
        }
        // Non-GNU cp (e.g. macOS) or a partial failure: start clean and copy.
        let _ = std::fs::remove_dir_all(dst);
    }
    copy_dir_all(src, dst).map_err(InstallError::Io)
}

/// Recursively copy a directory tree from `src` to `dst` (the reflink fallback).
fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let target = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_all(&entry.path(), &target)?;
        } else if file_type.is_symlink() {
            #[cfg(unix)]
            std::os::unix::fs::symlink(std::fs::read_link(entry.path())?, &target)?;
            #[cfg(not(unix))]
            {
                std::fs::copy(entry.path(), &target)?;
            }
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkg(name: &str, version: &str, build: &str) -> PackageId {
        PackageId {
            name: name.to_string(),
            version: version.to_string(),
            build: build.to_string(),
        }
    }

    #[test]
    fn prefix_path_entries_for_unix_and_windows() {
        let prefix = Path::new("/opt/envs/app");
        // Unix: just <prefix>/bin.
        assert_eq!(
            prefix_path_entries(prefix, "linux-64"),
            vec![PathBuf::from("/opt/envs/app/bin")]
        );
        // Windows: the conda Library/Scripts layout, most-specific first.
        let win = prefix_path_entries(prefix, "win-64");
        assert_eq!(win.first(), Some(&prefix.to_path_buf()));
        assert!(win.contains(&prefix.join("Scripts")));
        assert!(win.contains(&prefix.join("Library").join("bin")));
        assert!(win.len() > 1);
    }

    #[test]
    fn prefix_python_for_unix_and_windows() {
        let prefix = Path::new("/opt/envs/app");
        assert_eq!(
            prefix_python(prefix, "linux-64"),
            PathBuf::from("/opt/envs/app/bin/python")
        );
        assert_eq!(
            prefix_python(prefix, "win-64"),
            PathBuf::from("/opt/envs/app/python.exe")
        );
    }

    #[test]
    fn package_id_displays_as_conda_triplet() {
        assert_eq!(
            pkg("numpy", "2.1.0", "py311h0").to_string(),
            "numpy=2.1.0=py311h0"
        );
    }

    /// Raising is best effort, but it must never *lower* the limit, and must
    /// leave the hard limit alone.
    #[cfg(unix)]
    #[test]
    fn raising_the_file_limit_never_lowers_it() {
        // SAFETY: reads the current limits into a valid, correctly sized value.
        let read = || unsafe {
            let mut limit: libc::rlimit = std::mem::zeroed();
            assert_eq!(libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit), 0);
            (limit.rlim_cur, limit.rlim_max)
        };

        let (before_soft, before_hard) = read();
        raise_open_file_limit();
        let (after_soft, after_hard) = read();

        assert!(
            after_soft >= before_soft,
            "soft limit went backwards: {before_soft} -> {after_soft}"
        );
        assert_eq!(after_hard, before_hard, "hard limit must be untouched");
        // It should reach the target, or the hard ceiling if that is lower.
        let expected = (DESIRED_OPEN_FILES as libc::rlim_t).min(before_hard);
        assert!(after_soft >= expected.min(before_soft.max(expected)));
    }

    #[test]
    fn error_chain_appends_every_cause() {
        #[derive(Debug)]
        struct Layer(&'static str, Option<Box<Layer>>);

        impl fmt::Display for Layer {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }

        impl std::error::Error for Layer {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                self.1.as_deref().map(|e| e as &dyn std::error::Error)
            }
        }

        let root = Layer("connection refused", None);
        let middle = Layer("error sending request", Some(Box::new(root)));
        let outer = Layer(
            "failed to fetch libsolv-0.7.39-h9463b59_0.conda",
            Some(Box::new(middle)),
        );
        assert_eq!(
            error_chain(&outer),
            "failed to fetch libsolv-0.7.39-h9463b59_0.conda: error sending request: connection refused"
        );
    }

    #[test]
    fn error_chain_does_not_repeat_a_transparent_wrapper() {
        #[derive(Debug)]
        struct Inner;
        #[derive(Debug)]
        struct Transparent(Inner);

        impl fmt::Display for Inner {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "the real problem")
            }
        }
        impl std::error::Error for Inner {}

        impl fmt::Display for Transparent {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }
        impl std::error::Error for Transparent {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }

        assert_eq!(error_chain(&Transparent(Inner)), "the real problem");
    }

    #[test]
    fn error_chain_of_a_lone_error_is_its_message() {
        assert_eq!(
            error_chain(&InstallError::Lock("no such environment".into())),
            "invalid lock: no such environment"
        );
    }

    #[test]
    fn clone_prefix_reproduces_the_tree() {
        let base = std::env::temp_dir().join(format!("nepenthe-clone-{}", std::process::id()));
        let src = base.join("src");
        let dst = base.join("dst");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(src.join("bin")).unwrap();
        std::fs::write(src.join("bin").join("python"), b"#!/bin/sh\n").unwrap();
        std::fs::create_dir_all(src.join("conda-meta")).unwrap();
        std::fs::write(src.join("conda-meta").join("numpy.json"), b"{}").unwrap();

        clone_prefix(&src, &dst).unwrap();

        assert_eq!(
            std::fs::read(dst.join("bin").join("python")).unwrap(),
            b"#!/bin/sh\n"
        );
        assert!(dst.join("conda-meta").join("numpy.json").is_file());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn write_activation_hooks_emits_env_and_scripts() {
        use crate::manifest::Activation;
        let dir = std::env::temp_dir().join(format!("nepenthe-hooks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut activation = Activation::default();
        activation
            .env
            .insert("MY_ENV_NAME".to_string(), "team".to_string());
        activation.scripts.push("echo hi".to_string());

        write_activation_hooks(&dir, "team", Some("1.3.8"), "linux-64", &activation).unwrap();
        let body =
            std::fs::read_to_string(dir.join("etc/conda/activate.d/nepenthe-activate.sh")).unwrap();
        // Injected identity + declared env + script, all present.
        assert!(body.contains("export NEPENTHE_ENVIRONMENT='team'"));
        assert!(body.contains("export NEPENTHE_VERSION='1.3.8'"));
        assert!(body.contains("export MY_ENV_NAME='team'"));
        assert!(body.contains("echo hi"));

        // An empty activation writes nothing.
        let empty = dir.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        write_activation_hooks(&empty, "team", None, "linux-64", &Activation::default()).unwrap();
        assert!(!empty
            .join("etc/conda/activate.d/nepenthe-activate.sh")
            .exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn diff_is_empty_when_sets_match() {
        let desired = vec![pkg("numpy", "2.1.0", "h0"), pkg("python", "3.11.5", "h1")];
        let installed = desired.clone();
        let d = diff_packages(&desired, &installed);
        assert!(d.is_empty());
    }

    #[test]
    fn diff_reports_added_removed_and_changed() {
        let desired = vec![
            pkg("numpy", "2.1.0", "h0"),   // changed build
            pkg("python", "3.11.5", "h1"), // unchanged
            pkg("ruff", "0.6.0", "h2"),    // added
        ];
        let installed = vec![
            pkg("numpy", "2.1.0", "OLD"),  // changed
            pkg("python", "3.11.5", "h1"), // unchanged
            pkg("pip", "24.0", "h3"),      // removed
        ];
        let d = diff_packages(&desired, &installed);

        assert_eq!(d.added, vec![pkg("ruff", "0.6.0", "h2")]);
        assert_eq!(d.removed, vec![pkg("pip", "24.0", "h3")]);
        assert_eq!(d.changed.len(), 1);
        let (have, want) = &d.changed[0];
        assert_eq!(have.build, "OLD");
        assert_eq!(want.build, "h0");
        assert!(!d.is_empty());
    }

    #[test]
    fn diff_detects_version_change() {
        let desired = vec![pkg("numpy", "2.2.0", "h0")];
        let installed = vec![pkg("numpy", "2.1.0", "h0")];
        let d = diff_packages(&desired, &installed);
        assert_eq!(d.changed.len(), 1);
        assert!(d.added.is_empty() && d.removed.is_empty());
    }

    #[test]
    fn status_of_missing_prefix_reports_absent_and_empty() {
        let prefix =
            std::env::temp_dir().join(format!("nepenthe-install-missing-{}", std::process::id()));
        let st = status(&prefix).expect("status should succeed for a missing prefix");
        assert!(!st.exists);
        assert!(st.packages.is_empty());
        assert_eq!(st.prefix, prefix);
    }

    #[test]
    fn prefix_packages_empty_for_missing_prefix() {
        let prefix = std::env::temp_dir().join("nepenthe-install-nonexistent-xyz");
        assert!(prefix_packages(&prefix).expect("ok").is_empty());
    }

    #[test]
    fn remove_missing_prefix_is_a_noop() {
        let prefix = std::env::temp_dir().join("nepenthe-install-remove-missing-xyz");
        assert!(remove_prefix(&prefix, false).is_ok());
    }

    #[test]
    fn remove_refuses_non_environment_without_force() {
        let dir = std::env::temp_dir().join(format!(
            "nepenthe-install-remove-guard-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(dir.join("important-data")).expect("setup");
        // No conda-meta: refused unless forced, and the tree is left intact.
        assert!(matches!(
            remove_prefix(&dir, false),
            Err(InstallError::NotAnEnvironment(_))
        ));
        assert!(dir.join("important-data").is_dir());
        // conda-meta present: removal is allowed.
        std::fs::create_dir_all(dir.join("conda-meta")).expect("setup");
        remove_prefix(&dir, false).expect("remove env");
        assert!(!dir.exists());
    }

    #[test]
    fn remove_refuses_current_directory() {
        let cwd = std::env::current_dir().expect("cwd");
        assert!(matches!(
            remove_prefix(&cwd, true),
            Err(InstallError::UnsafePrefix(_))
        ));
        assert!(cwd.exists());
    }

    #[test]
    fn diff_against_missing_prefix_is_all_added() {
        // Build a tiny lock with one conda package, then diff against an empty
        // prefix: every desired package should be reported as "added".
        use crate::export::to_lockfile_string;
        use crate::solve::{ChannelPriorityMode, SolveOutcome};
        use rattler_conda_types::package::DistArchiveIdentifier;
        use rattler_conda_types::{PackageName, PackageRecord, RepoDataRecord, VersionWithSource};
        use url::Url;

        let mut pr = PackageRecord::new(
            PackageName::from_str("numpy").unwrap(),
            VersionWithSource::from_str("2.1.0").unwrap(),
            "py311h0".to_string(),
        );
        pr.subdir = "linux-64".to_string();
        let record = RepoDataRecord {
            package_record: pr,
            identifier: "numpy-2.1.0-py311h0.conda"
                .parse::<DistArchiveIdentifier>()
                .unwrap(),
            url: Url::parse("https://example.com/conda-forge/linux-64/numpy-2.1.0-py311h0.conda")
                .unwrap(),
            channel: Some("https://example.com/conda-forge".to_string()),
        };
        let outcome = SolveOutcome {
            records: vec![record],
            channels: vec!["https://example.com/conda-forge".to_string()],
            platform: "linux-64".to_string(),
            virtual_packages: vec![],
            channel_priority: ChannelPriorityMode::Disabled,
            exclude_newer: None,
        };
        let lock_yaml = to_lockfile_string(&outcome, "app").expect("render lock");
        let lock = parse_lock(lock_yaml.as_bytes()).expect("parse lock");

        // the lock pins one package for linux-64
        let desired = lock_packages(&lock, "app", "linux-64").expect("lock packages");
        assert_eq!(desired, vec![pkg("numpy", "2.1.0", "py311h0")]);

        // diff against an empty prefix → that package is "added"
        let empty_prefix = std::env::temp_dir().join("nepenthe-install-empty-prefix-xyz");
        let d = diff(&lock, "app", "linux-64", &empty_prefix).expect("diff");
        assert_eq!(d.added, vec![pkg("numpy", "2.1.0", "py311h0")]);
        assert!(d.removed.is_empty() && d.changed.is_empty());

        // an unknown platform/environment is reported, not panicked
        assert!(lock_packages(&lock, "app", "win-64").is_err());
        assert!(lock_packages(&lock, "nope", "linux-64").is_err());
    }

    #[test]
    fn sole_environment_infers_the_single_environment() {
        use crate::export::to_lockfile_string;
        use crate::solve::{ChannelPriorityMode, SolveOutcome};
        use rattler_conda_types::package::DistArchiveIdentifier;
        use rattler_conda_types::{PackageName, PackageRecord, RepoDataRecord, VersionWithSource};
        use url::Url;

        let mut pr = PackageRecord::new(
            PackageName::from_str("numpy").unwrap(),
            VersionWithSource::from_str("2.1.0").unwrap(),
            "py311h0".to_string(),
        );
        pr.subdir = "linux-64".to_string();
        let record = RepoDataRecord {
            package_record: pr,
            identifier: "numpy-2.1.0-py311h0.conda"
                .parse::<DistArchiveIdentifier>()
                .unwrap(),
            url: Url::parse("https://example.com/conda-forge/linux-64/numpy-2.1.0-py311h0.conda")
                .unwrap(),
            channel: Some("https://example.com/conda-forge".to_string()),
        };
        let outcome = SolveOutcome {
            records: vec![record],
            channels: vec!["https://example.com/conda-forge".to_string()],
            platform: "linux-64".to_string(),
            virtual_packages: vec![],
            channel_priority: ChannelPriorityMode::Disabled,
            exclude_newer: None,
        };
        // A per-cell lock (one environment) is inferred without an explicit name.
        let lock_yaml = to_lockfile_string(&outcome, "myenv").expect("render lock");
        let lock = parse_lock(lock_yaml.as_bytes()).expect("parse lock");
        assert_eq!(sole_environment(&lock).unwrap(), "myenv");
    }

    #[test]
    fn multi_platform_lock_extracts_per_platform_records() {
        // A single multi-platform lock should yield each platform's own package
        // set, so `create --platform <p>` installs the right records.
        use crate::export::to_multi_platform_lockfile;
        use crate::solve::{ChannelPriorityMode, SolveOutcome};
        use rattler_conda_types::package::DistArchiveIdentifier;
        use rattler_conda_types::{PackageName, PackageRecord, RepoDataRecord, VersionWithSource};
        use url::Url;

        fn outcome(platform: &str, build: &str) -> SolveOutcome {
            let mut pr = PackageRecord::new(
                PackageName::from_str("python").unwrap(),
                VersionWithSource::from_str("3.11.14").unwrap(),
                build.to_string(),
            );
            pr.subdir = platform.to_string();
            let rec = RepoDataRecord {
                package_record: pr,
                identifier: format!("python-3.11.14-{build}.conda")
                    .parse::<DistArchiveIdentifier>()
                    .unwrap(),
                url: Url::parse(&format!(
                    "https://example.com/conda-forge/{platform}/python-3.11.14-{build}.conda"
                ))
                .unwrap(),
                channel: Some("https://example.com/conda-forge".to_string()),
            };
            SolveOutcome {
                records: vec![rec],
                channels: vec!["https://example.com/conda-forge".to_string()],
                platform: platform.to_string(),
                virtual_packages: vec![],
                channel_priority: ChannelPriorityMode::Disabled,
                exclude_newer: None,
            }
        }

        let outcomes = [outcome("linux-64", "linux0"), outcome("osx-arm64", "osx0")];
        let lock = to_multi_platform_lockfile(&outcomes, "app").expect("build lock");
        let yaml = lock.render_to_string().expect("render");
        let lock = parse_lock(yaml.as_bytes()).expect("reparse");

        // each platform extracts its own build of python
        assert_eq!(
            lock_packages(&lock, "app", "linux-64").unwrap(),
            vec![pkg("python", "3.11.14", "linux0")]
        );
        assert_eq!(
            lock_packages(&lock, "app", "osx-arm64").unwrap(),
            vec![pkg("python", "3.11.14", "osx0")]
        );
        // a platform not in the lock is an error, not a panic
        assert!(lock_packages(&lock, "app", "win-64").is_err());
    }

    /// Capstone: solve python live, export a real lock, install
    /// it into a temp prefix with **no conda involved**, then exercise
    /// `status` / `diff` / activation / `remove`. Ignored by default so CI
    /// stays offline; run with `cargo test -- --ignored`.
    #[ignore = "requires network access to conda channels and links a real prefix"]
    #[tokio::test]
    async fn real_install_python_into_prefix() {
        use crate::export::to_lockfile_string;
        use crate::solve::{solve, ChannelSettings, SolveRequest};

        // 1) solve a tiny environment from public conda-forge
        let request = SolveRequest {
            channels: vec!["conda-forge".to_string()],
            platform: Platform::current().to_string(),
            specs: vec!["python 3.11.*".to_string()],
            ..Default::default()
        };
        let outcome = solve(&request, &ChannelSettings::default())
            .await
            .expect("solve should succeed");

        // 2) export a lock and parse it back
        let lock_yaml = to_lockfile_string(&outcome, "app").expect("render lock");
        let lock = parse_lock(lock_yaml.as_bytes()).expect("parse lock");
        let platform = Platform::current().to_string();

        // 3) install into a fresh temp prefix — no conda required
        let prefix =
            std::env::temp_dir().join(format!("nepenthe-install-capstone-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&prefix);

        let summary = install_lock(&lock, "app", &platform, &prefix)
            .await
            .expect("install should succeed");
        assert!(!summary.packages.is_empty());

        // 4) status reports the installed packages; diff against the lock is empty
        let st = status(&prefix).expect("status");
        assert!(st.exists);
        assert!(st.packages.iter().any(|p| p.name == "python"));

        let d = diff(&lock, "app", &platform, &prefix).expect("diff");
        assert!(
            d.is_empty(),
            "a freshly-installed prefix should match its lock: {d:?}"
        );

        // 5) an activation script can be generated for the prefix
        let script =
            activation_script(&prefix, ShellEnum::default(), &platform).expect("activation script");
        assert!(script.contains(prefix.to_str().unwrap()));

        // 6) remove the prefix
        remove_prefix(&prefix, false).expect("remove");
        assert!(!status(&prefix).expect("status").exists);
    }

    /// Solve `python` for **two platforms** (the host plus a
    /// foreign one) from a single host, combine them into **one multi-platform
    /// lock**, confirm both platforms are present, then install the host
    /// platform from that shared lock. Proves cross-platform solving and the
    /// multi-platform install matrix. Ignored by default; run with
    /// `cargo test -- --ignored`.
    #[ignore = "requires network access to conda channels and links a real prefix"]
    #[tokio::test]
    async fn real_cross_platform_multi_lock_installs() {
        use crate::export::to_multi_platform_lockfile;
        use crate::solve::{solve, ChannelSettings, SolveRequest};

        // The host platform plus a deliberately different one (cross-compile).
        let host = Platform::current().to_string();
        let foreign = if host == "osx-arm64" {
            "linux-64"
        } else {
            "osx-arm64"
        };

        // 1) solve the same spec for both platforms from this one host
        let mut outcomes = Vec::new();
        for platform in [host.as_str(), foreign] {
            let request = SolveRequest {
                channels: vec!["conda-forge".to_string()],
                platform: platform.to_string(),
                specs: vec!["python 3.11.*".to_string()],
                ..Default::default()
            };
            outcomes.push(
                solve(&request, &ChannelSettings::default())
                    .await
                    .unwrap_or_else(|e| panic!("solve {platform} should succeed: {e}")),
            );
        }

        // 2) combine into ONE multi-platform lock and reparse
        let lock_yaml = to_multi_platform_lockfile(&outcomes, "app")
            .expect("build multi-platform lock")
            .render_to_string()
            .expect("render");
        let lock = parse_lock(lock_yaml.as_bytes()).expect("reparse multi-platform lock");

        // 3) both platforms resolve a package set from the single lock
        assert!(!lock_packages(&lock, "app", &host)
            .expect("host packages")
            .is_empty());
        assert!(!lock_packages(&lock, "app", foreign)
            .expect("foreign packages")
            .is_empty());

        // 4) install the host platform from the shared lock — no conda required
        let prefix =
            std::env::temp_dir().join(format!("nepenthe-install-xplat-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&prefix);
        let summary = install_lock(&lock, "app", &host, &prefix)
            .await
            .expect("install should succeed");
        assert!(!summary.packages.is_empty());
        assert!(diff(&lock, "app", &host, &prefix).expect("diff").is_empty());

        remove_prefix(&prefix, false).expect("remove");
    }
}
