//! `nepenthe run`: execute a command in a versioned, pre-solved environment with
//! an optional lockable **overlay** of extra dependencies.
//!
//! A run materializes a published base environment (cached, no conda), layers a
//! small conda overlay on top, and execs a command in the combined prefix. The
//! reproducibility unit is `(base lock + overlay specs + command)`: the base is
//! frozen, and the overlay is re-solved with the base pinned as constraints so
//! the union is always consistent.
//!
//! Two config sources, one schema:
//!
//! - **`[tool.nepenthe.run]`** in a `pyproject.toml` (structured, for a tool).
//! - **An inline PEP 723-style block** in a script:
//!
//!   ```text
//!   # /// nepenthe
//!   # environment = "ccrt"
//!   # registry = "file:///srv/nepenthe"
//!   # with = ["polars>=1"]
//!   # ///
//!   ```
//!
//! `editable` directories are prepended to `PYTHONPATH`, so a working tree runs
//! against a base that does **not** contain it — no shadowing, no skew. PyPI
//! overlays are layered on top with [uv](https://docs.astral.sh/uv/): after the
//! conda base is materialized, `uv pip install` resolves the requested
//! requirements into the run prefix against the interpreter and packages the
//! base already provides. uv is invoked as a subprocess (its CLI is the stable
//! surface); point `NEPENTHE_UV` at a binary, else `uv` on `PATH` is used.

use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::backend::SpecStore;
use crate::image::{self, ImageError};
use crate::install::{self, InstallError};
use crate::manifest::Manifest;
use crate::registry::{Coordinates, Label, Registry, RegistryError};
use crate::solve::{solve, ChannelSettings, SolveError, SolveRequest};

/// The leading marker of an inline PEP 723-style nepenthe block.
const INLINE_OPEN: &str = "# /// nepenthe";
/// The closing marker of an inline block.
const INLINE_CLOSE: &str = "# ///";

/// Errors raised while configuring or running a `nepenthe run`.
#[derive(Debug)]
pub enum RunError {
    /// Reading the config (pyproject or script) failed.
    Io(std::io::Error),
    /// The config TOML was invalid.
    Toml(String),
    /// Neither a `[tool.nepenthe.run]` table nor an inline block was found.
    NoConfig,
    /// No command to run was given (config and CLI both omitted it).
    NoCommand,
    /// The `uv` binary (used for PyPI overlays) was not found.
    UvNotFound,
    /// A `uv` invocation for a PyPI overlay failed.
    Uv(String),
    /// Resolving or pulling from the registry failed.
    Registry(RegistryError),
    /// Solving the overlay failed.
    Solve(SolveError),
    /// Installing the prefix failed.
    Install(InstallError),
    /// Building or running the environment's image failed.
    Image(ImageError),
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RunError::Io(e) => write!(f, "{e}"),
            RunError::Toml(msg) => write!(f, "invalid run config: {msg}"),
            RunError::NoConfig => write!(
                f,
                "no run config found (expected a [tool.nepenthe.run] table or an inline \
                 `# /// nepenthe` block)"
            ),
            RunError::NoCommand => write!(
                f,
                "no command to run (set `command` in the config or pass one after `--`)"
            ),
            RunError::UvNotFound => write!(
                f,
                "uv not found; install uv (https://docs.astral.sh/uv/) or set NEPENTHE_UV to its \
                 path to use PyPI overlays (`overlay.pip` / `--with-pip`)"
            ),
            RunError::Uv(msg) => write!(f, "uv overlay failed: {msg}"),
            RunError::Registry(e) => write!(f, "{e}"),
            RunError::Solve(e) => write!(f, "{e}"),
            RunError::Install(e) => write!(f, "{e}"),
            RunError::Image(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for RunError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RunError::Io(e) => Some(e),
            RunError::Registry(e) => Some(e),
            RunError::Solve(e) => Some(e),
            RunError::Install(e) => Some(e),
            RunError::Image(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for RunError {
    fn from(e: std::io::Error) -> Self {
        RunError::Io(e)
    }
}

impl From<RegistryError> for RunError {
    fn from(e: RegistryError) -> Self {
        RunError::Registry(e)
    }
}

impl From<SolveError> for RunError {
    fn from(e: SolveError) -> Self {
        RunError::Solve(e)
    }
}

impl From<InstallError> for RunError {
    fn from(e: InstallError) -> Self {
        RunError::Install(e)
    }
}

impl From<ImageError> for RunError {
    fn from(e: ImageError) -> Self {
        RunError::Image(e)
    }
}

/// The conda/pip overlay declared by a run config.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct RawOverlay {
    #[serde(default)]
    conda: Vec<String>,
    #[serde(default)]
    pip: Vec<String>,
}

/// A command, given as a single string (whitespace-split) or an argv array.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
enum CommandSpec {
    Line(String),
    Argv(Vec<String>),
}

impl CommandSpec {
    fn into_argv(self) -> Vec<String> {
        match self {
            CommandSpec::Line(line) => line.split_whitespace().map(String::from).collect(),
            CommandSpec::Argv(argv) => argv,
        }
    }
}

/// The raw `[tool.nepenthe.run]` / inline-block schema.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct RawRunConfig {
    environment: String,
    registry: String,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    platform: Option<String>,
    #[serde(default)]
    python: Option<String>,
    #[serde(default)]
    variant: Option<String>,
    #[serde(default)]
    overlay: RawOverlay,
    /// Inline shorthand for `overlay.conda`.
    #[serde(default, rename = "with")]
    with_: Vec<String>,
    #[serde(default)]
    editable: Vec<PathBuf>,
    #[serde(default)]
    command: Option<CommandSpec>,
    #[serde(default)]
    prefix: Option<PathBuf>,
    /// Run inside a SIF image of the environment instead of a prefix.
    #[serde(default)]
    image: bool,
    /// Base OS image for `image` mode (must provide glibc + `/bin/sh`).
    #[serde(default)]
    base: Option<String>,
    /// Lazy image: don't bake the env in, bind it at run time (small/fast).
    #[serde(default)]
    lazy: bool,
    /// Give the image an ephemeral in-memory writable layer.
    #[serde(default)]
    writable: bool,
    /// Persistent EXT3 overlay image path for writes over a read-only base.
    #[serde(default)]
    overlay_image: Option<PathBuf>,
    /// Materialize via a copy-on-write clone of the base prefix (reflink FS).
    #[serde(default)]
    clone: bool,
}

/// A resolved run request: which environment to materialize, the overlay to lay
/// on top, and the command to execute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunConfig {
    /// Environment name to run in.
    pub environment: String,
    /// Registry root URL the environment is published to.
    pub registry: String,
    /// Version label (`latest`, exact, or range). Defaults to `latest`.
    pub version: Option<String>,
    /// Target platform (defaults to the current platform).
    pub platform: Option<String>,
    /// Python axis value, if any.
    pub python: Option<String>,
    /// Variant axis value, if any.
    pub variant: Option<String>,
    /// Conda match-specs to overlay on the base environment.
    pub overlay_conda: Vec<String>,
    /// PyPI requirements to overlay (not yet supported).
    pub overlay_pip: Vec<String>,
    /// Directories to prepend to `PYTHONPATH` (editable / working-tree overlay).
    pub editable: Vec<PathBuf>,
    /// The command to run (program + args).
    pub command: Vec<String>,
    /// Explicit prefix to materialize into (defaults to a content-keyed cache).
    pub prefix: Option<PathBuf>,
    /// Run inside a SIF image of the environment instead of a prefix.
    pub use_image: bool,
    /// Base OS image for image mode (defaults to the standard glibc base).
    pub image_base: Option<String>,
    /// Lazy image mode: bind the env at run time instead of baking it in.
    pub image_lazy: bool,
    /// Give the image an ephemeral in-memory writable layer.
    pub image_writable: bool,
    /// Persistent EXT3 overlay image path for writes over a read-only base.
    pub image_overlay: Option<PathBuf>,
    /// Materialize via a copy-on-write clone of the base prefix (reflink FS).
    pub use_clone: bool,
    /// The directory the config was loaded from, used to resolve relative paths.
    pub base_dir: PathBuf,
}

impl RunConfig {
    fn from_raw(
        raw: RawRunConfig,
        base_dir: PathBuf,
        default_command: Option<Vec<String>>,
    ) -> Self {
        let mut overlay_conda = raw.overlay.conda;
        overlay_conda.extend(raw.with_);
        let command = raw
            .command
            .map(CommandSpec::into_argv)
            .or(default_command)
            .unwrap_or_default();
        RunConfig {
            environment: raw.environment,
            registry: raw.registry,
            version: raw.version,
            platform: raw.platform,
            python: raw.python,
            variant: raw.variant,
            overlay_conda,
            overlay_pip: raw.overlay.pip,
            editable: raw.editable,
            command,
            prefix: raw.prefix,
            use_image: raw.image,
            image_base: raw.base,
            image_lazy: raw.lazy,
            image_writable: raw.writable,
            image_overlay: raw.overlay_image,
            use_clone: raw.clone,
            base_dir,
        }
    }

    /// Load a run config from a `pyproject.toml`'s `[tool.nepenthe.run]` table.
    pub fn from_pyproject(pyproject: &Path) -> Result<Self, RunError> {
        let text = std::fs::read_to_string(pyproject)?;
        let doc: toml::Value = toml::from_str(&text).map_err(|e| RunError::Toml(e.to_string()))?;
        let table = doc
            .get("tool")
            .and_then(|t| t.get("nepenthe"))
            .and_then(|n| n.get("run"))
            .ok_or(RunError::NoConfig)?;
        let raw: RawRunConfig = table
            .clone()
            .try_into()
            .map_err(|e: toml::de::Error| RunError::Toml(e.to_string()))?;
        let base_dir = pyproject
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        Ok(RunConfig::from_raw(raw, base_dir, None))
    }

    /// Load a run config from a script's inline `# /// nepenthe` block, if
    /// present. The command defaults to running the script with `python`.
    pub fn from_inline_script(script: &Path) -> Result<Option<Self>, RunError> {
        let text = std::fs::read_to_string(script)?;
        let Some(toml_text) = extract_inline_block(&text) else {
            return Ok(None);
        };
        let raw: RawRunConfig =
            toml::from_str(&toml_text).map_err(|e| RunError::Toml(e.to_string()))?;
        let base_dir = script
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let default_command = vec!["python".to_string(), script.to_string_lossy().into_owned()];
        Ok(Some(RunConfig::from_raw(
            raw,
            base_dir,
            Some(default_command),
        )))
    }

    fn coordinates(&self) -> Coordinates {
        let platform = self
            .platform
            .clone()
            .unwrap_or_else(crate::current_platform);
        let mut coords = Coordinates::new(self.environment.clone(), platform);
        if let Some(python) = &self.python {
            coords = coords.with_python(python.clone());
        }
        if let Some(variant) = &self.variant {
            coords = coords.with_variant(variant.clone());
        }
        coords
    }

    fn label(&self) -> Label {
        Label::parse(self.version.as_deref().unwrap_or("latest"))
    }

    /// The editable directories, resolved against the config's base directory.
    fn editable_paths(&self) -> Vec<PathBuf> {
        self.editable
            .iter()
            .map(|p| {
                if p.is_absolute() {
                    p.clone()
                } else {
                    self.base_dir.join(p)
                }
            })
            .collect()
    }
}

/// A summary of a completed [`run`].
#[derive(Debug, Clone)]
pub struct RunSummary {
    /// The prefix the environment was materialized into.
    pub prefix: PathBuf,
    /// The platform run for.
    pub platform: String,
    /// The number of conda overlay packages added on top of the base.
    pub overlay_packages: usize,
    /// The number of PyPI requirements overlaid with uv.
    pub pip_overlay: usize,
    /// The persisted standalone overlay lock (conda solved set + uv pip compile),
    /// when the run had any overlay.
    pub overlay_lock: Option<PathBuf>,
    /// The image the command ran in, when `--image` was used.
    pub image: Option<PathBuf>,
    /// The exit status of the executed command.
    pub status: ExitStatus,
}

/// Materialize `config`'s environment (base + conda overlay) into a prefix and
/// execute its command with `extra_args` appended. Performs network and file
/// I/O; await inside a tokio runtime.
pub async fn run(config: &RunConfig, extra_args: &[String]) -> Result<RunSummary, RunError> {
    let mut command = config.command.clone();
    command.extend(extra_args.iter().cloned());
    if command.is_empty() {
        return Err(RunError::NoCommand);
    }

    let coords = config.coordinates();
    let platform = coords.platform.clone();
    let registry = Registry::new(SpecStore::new(), config.registry.clone());
    let label = config.label();

    let base_bytes = registry.pull(&coords, &label)?;
    let base_lock = install::parse_lock(&base_bytes)?;
    let base_records = install::lock_records(&base_lock, &config.environment, &platform)?;

    // Lay the conda overlay on top: solve it with the base pinned as
    // constraints, then keep only the packages the base doesn't already have.
    let mut records = base_records.clone();
    let mut overlay_added = 0usize;
    let mut conda_overlay_lock: Vec<String> = Vec::new();
    if !config.overlay_conda.is_empty() {
        let (channels, settings) = base_channels(&registry, &coords, &label, &base_bytes);
        let constraints: Vec<String> =
            install::lock_packages(&base_lock, &config.environment, &platform)?
                .iter()
                .map(|p| format!("{} =={}", p.name, p.version))
                .collect();
        let request = SolveRequest {
            channels,
            platform: platform.clone(),
            specs: config.overlay_conda.clone(),
            constraints,
            ..Default::default()
        };
        let outcome = solve(&request, &settings).await?;
        let base_names: std::collections::BTreeSet<String> = base_records
            .iter()
            .map(|r| r.package_record.name.as_normalized().to_string())
            .collect();
        for record in outcome.records {
            if !base_names.contains(record.package_record.name.as_normalized()) {
                let pr = &record.package_record;
                conda_overlay_lock.push(format!(
                    "{}={}={}",
                    pr.name.as_normalized(),
                    pr.version,
                    pr.build
                ));
                records.push(record);
                overlay_added += 1;
            }
        }
    }

    let prefix = match &config.prefix {
        Some(p) => {
            if p.is_absolute() {
                p.clone()
            } else {
                config.base_dir.join(p)
            }
        }
        None => default_run_prefix(&base_bytes, &config.overlay_conda, &config.overlay_pip)?,
    };

    // Reuse an already-materialized prefix; otherwise install the combined set.
    if !prefix.join("conda-meta").is_dir() {
        if config.use_clone {
            // Copy-on-write tier: materialize the base once into its own
            // content-keyed prefix, clone it (near-instant on a reflink FS),
            // then let rattler install only the overlay delta on top.
            let base_prefix = default_run_prefix(&base_bytes, &[], &[])?;
            if !base_prefix.join("conda-meta").is_dir() {
                install::install_records(
                    base_records.clone(),
                    &config.environment,
                    &platform,
                    &base_prefix,
                )
                .await?;
            }
            if base_prefix != prefix {
                install::clone_prefix(&base_prefix, &prefix)?;
                install::install_records(records, &config.environment, &platform, &prefix).await?;
            }
        } else {
            install::install_records(records, &config.environment, &platform, &prefix).await?;
        }
    }

    // Lay the PyPI overlay on top with uv, once per content-keyed prefix. uv
    // resolves the requirements against the interpreter and packages the base
    // already provides, installing the delta into the prefix's site-packages.
    let pip_lock_path = prefix.join(".nepenthe-overlay-pip.lock");
    let pip_ready = prefix.join(".nepenthe-pip-ready");
    let pip_lock = if !config.overlay_pip.is_empty() {
        if pip_ready.exists() {
            std::fs::read_to_string(&pip_lock_path).ok()
        } else {
            let text = uv_pip_overlay(&prefix, &platform, &config.overlay_pip)?;
            std::fs::write(&pip_lock_path, &text)?;
            std::fs::write(&pip_ready, b"")?;
            Some(text)
        }
    } else {
        None
    };

    // Persist a standalone overlay lock (the conda solved set + the uv-compiled
    // PyPI set) so the delta on top of the frozen base is itself reproducible.
    let overlay_lock = write_overlay_lock(&prefix, &conda_overlay_lock, pip_lock.as_deref())?;

    let program = std::ffi::OsString::from(&command[0]);
    let args: Vec<std::ffi::OsString> = command[1..].iter().map(std::ffi::OsString::from).collect();
    let editable = config.editable_paths();

    // Execute either inside a content-keyed SIF image of the prefix (`--image`),
    // or directly in the activated prefix on the host.
    let (status, image) = if config.use_image {
        let base = config
            .image_base
            .as_deref()
            .unwrap_or(image::DEFAULT_BASE_IMAGE);
        let suffix = if config.image_lazy {
            ".lazy.sif"
        } else {
            ".sif"
        };
        let mut sif = prefix.clone().into_os_string();
        sif.push(suffix);
        let sif = PathBuf::from(sif);
        if !sif.exists() {
            image::package_sif(
                &prefix,
                base,
                &sif,
                &config.environment,
                &platform,
                config.version.as_deref().unwrap_or("latest"),
                config.image_lazy,
            )?;
        }
        // Editable dirs are bound + on PYTHONPATH; a lazy image also binds the
        // (uncopied) prefix so the container sees the environment.
        let mut binds = editable.clone();
        if config.image_lazy {
            binds.push(prefix.clone());
        }
        let opts = image::ExecOptions {
            binds,
            pythonpath: editable.clone(),
            writable_tmpfs: config.image_writable,
            overlay_image: config.image_overlay.clone(),
        };
        let status = image::exec_in_image(&sif, &program, &args, &opts)?;
        (status, Some(sif))
    } else {
        let status = install::exec_in_prefix(&prefix, &platform, &program, &args, &editable)?;
        (status, None)
    };

    Ok(RunSummary {
        prefix,
        platform,
        overlay_packages: overlay_added,
        pip_overlay: config.overlay_pip.len(),
        overlay_lock,
        image,
        status,
    })
}

/// The channels (and channel settings) to solve an overlay against: the base
/// environment's manifest if it is recoverable (embedded band or registry
/// sidecar), else a plain `conda-forge`.
fn base_channels(
    registry: &Registry,
    coords: &Coordinates,
    label: &Label,
    base_bytes: &[u8],
) -> (Vec<String>, ChannelSettings) {
    let manifest_yaml = crate::embed::extract_manifest(base_bytes)
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
        if let Ok(manifest) = Manifest::from_yaml_str(&yaml) {
            let settings = ChannelSettings::from_manifest(&manifest);
            let channels = manifest.project.channels.clone();
            if !channels.is_empty() {
                return (channels, settings);
            }
        }
    }
    (vec!["conda-forge".to_string()], ChannelSettings::default())
}

/// The content key for a `(base lock, conda overlay, pip overlay)` triple, so
/// identical runs reuse the same materialized environment.
fn run_digest(base_bytes: &[u8], overlay_conda: &[String], overlay_pip: &[String]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(base_bytes);
    for spec in overlay_conda {
        hasher.update(b"\x00c");
        hasher.update(spec.as_bytes());
    }
    for spec in overlay_pip {
        hasher.update(b"\x00p");
        hasher.update(spec.as_bytes());
    }
    hex::encode(&hasher.finalize()[..16])
}

/// A content-keyed cache prefix for a `(base lock, conda overlay, pip overlay)`
/// triple, so identical runs reuse the same materialized environment.
fn default_run_prefix(
    base_bytes: &[u8],
    overlay_conda: &[String],
    overlay_pip: &[String],
) -> Result<PathBuf, RunError> {
    let digest = run_digest(base_bytes, overlay_conda, overlay_pip);
    let cache = rattler_cache::default_cache_dir()
        .map_err(|e| RunError::Io(std::io::Error::other(e.to_string())))?;
    Ok(cache.join("nepenthe-run").join(digest))
}

/// Overlay PyPI `specs` onto an already-materialized `prefix` with uv. uv
/// targets the prefix's interpreter directly (a conda environment), resolving
/// against and reusing the packages the base already installed, and writes the
/// delta into the prefix's site-packages. The `uv` CLI is the stable surface:
/// `NEPENTHE_UV` overrides the binary, else `uv` is resolved on `PATH`.
///
/// Returns the overlay's pinned PyPI lock (`uv pip compile`), capturing the
/// delta as a standalone, reproducible artifact. The install itself uses
/// `uv pip install <specs>`, which respects what the conda base already provides
/// (so only the true delta lands), while the compiled lock records the full
/// resolved closure of the requirements.
fn uv_pip_overlay(prefix: &Path, platform: &str, specs: &[String]) -> Result<String, RunError> {
    let python = install::prefix_python(prefix, platform);
    let lock = uv_pip_compile(&python, specs)?;
    let program = std::env::var_os("NEPENTHE_UV").unwrap_or_else(|| std::ffi::OsString::from("uv"));
    let status = std::process::Command::new(program)
        .arg("pip")
        .arg("install")
        .arg("--python")
        .arg(&python)
        .args(specs)
        .status()
        .map_err(map_uv_spawn_error)?;
    if !status.success() {
        return Err(RunError::Uv(format!(
            "`uv pip install` exited with {}",
            status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".to_string())
        )));
    }
    Ok(lock)
}

/// Resolve `specs` into a pinned PyPI lock with `uv pip compile`, reading the
/// requirements from stdin and resolving for `python`'s interpreter.
fn uv_pip_compile(python: &Path, specs: &[String]) -> Result<String, RunError> {
    let program = std::env::var_os("NEPENTHE_UV").unwrap_or_else(|| std::ffi::OsString::from("uv"));
    let mut child = std::process::Command::new(program)
        .arg("pip")
        .arg("compile")
        .arg("-")
        .arg("--python")
        .arg(python)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .map_err(map_uv_spawn_error)?;
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(specs.join("\n").as_bytes())?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(RunError::Uv(format!(
            "`uv pip compile` exited with {}",
            output
                .status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".to_string())
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Map a `uv` spawn error to [`RunError::UvNotFound`] when the binary is missing.
fn map_uv_spawn_error(e: std::io::Error) -> RunError {
    if e.kind() == std::io::ErrorKind::NotFound {
        RunError::UvNotFound
    } else {
        RunError::Io(e)
    }
}

/// Write a standalone overlay lock (`<prefix>/.nepenthe-overlay.lock`) recording
/// the conda solved set and the uv-compiled PyPI set. Returns its path, or
/// `None` when there is no overlay to record.
fn write_overlay_lock(
    prefix: &Path,
    conda: &[String],
    pip: Option<&str>,
) -> Result<Option<PathBuf>, RunError> {
    if conda.is_empty() && pip.is_none() {
        return Ok(None);
    }
    let mut body = String::from("# nepenthe overlay lock\n");
    if !conda.is_empty() {
        body.push_str("# conda (solved against the base)\n");
        for line in conda {
            body.push_str(line);
            body.push('\n');
        }
    }
    if let Some(pip) = pip {
        body.push_str("# pypi (uv pip compile)\n");
        body.push_str(pip);
        if !pip.ends_with('\n') {
            body.push('\n');
        }
    }
    let path = prefix.join(".nepenthe-overlay.lock");
    std::fs::write(&path, body)?;
    Ok(Some(path))
}

/// Extract the TOML body of an inline `# /// nepenthe … # ///` block, with the
/// `# ` comment prefix stripped from each line.
fn extract_inline_block(text: &str) -> Option<String> {
    let mut lines = text.lines();
    lines.by_ref().find(|line| line.trim_end() == INLINE_OPEN)?;
    let mut body = String::new();
    for line in lines {
        let trimmed = line.trim_end();
        if trimmed == INLINE_CLOSE {
            return Some(body);
        }
        // Strip the comment prefix: `# foo` → `foo`, `#` → ``.
        let stripped = line
            .strip_prefix("# ")
            .or_else(|| line.strip_prefix('#'))
            .unwrap_or(line);
        body.push_str(stripped);
        body.push('\n');
    }
    // Unterminated block: ignore it rather than misparse.
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tool_nepenthe_run_table() {
        let text = r#"
[project]
name = "demo"

[tool.nepenthe.run]
environment = "ccrt"
registry = "file:///srv/nepenthe"
version = "1.3.0"
python = "3.11"
overlay = { conda = ["polars>=1"], pip = [] }
editable = ["."]
command = "python -m mytool"
"#;
        let dir = std::env::temp_dir().join(format!("nepenthe-run-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pyproject.toml");
        std::fs::write(&path, text).unwrap();

        let config = RunConfig::from_pyproject(&path).unwrap();
        assert_eq!(config.environment, "ccrt");
        assert_eq!(config.python.as_deref(), Some("3.11"));
        assert_eq!(config.overlay_conda, vec!["polars>=1"]);
        assert_eq!(config.command, vec!["python", "-m", "mytool"]);
        assert_eq!(config.editable, vec![PathBuf::from(".")]);
        assert!(matches!(config.label(), Label::Exact(v) if v == "1.3.0"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parses_inline_block_with_with_shorthand() {
        let script = "# /// nepenthe\n# environment = \"ccrt\"\n# registry = \"file:///srv/nepenthe\"\n# with = [\"rich\", \"polars>=1\"]\n# ///\nimport rich\n";
        let dir = std::env::temp_dir().join(format!("nepenthe-run-inline-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("script.py");
        std::fs::write(&path, script).unwrap();

        let config = RunConfig::from_inline_script(&path).unwrap().unwrap();
        assert_eq!(config.environment, "ccrt");
        // `with` is folded into the conda overlay.
        assert_eq!(config.overlay_conda, vec!["rich", "polars>=1"]);
        // The default command runs the script with python.
        assert_eq!(config.command[0], "python");
        assert!(config.command[1].ends_with("script.py"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn no_inline_block_returns_none() {
        let dir = std::env::temp_dir().join(format!("nepenthe-run-plain-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("plain.py");
        std::fs::write(&path, "import os\nprint('hi')\n").unwrap();
        assert!(RunConfig::from_inline_script(&path).unwrap().is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn extract_inline_block_strips_comment_prefix() {
        let text =
            "#!/usr/bin/env python\n# /// nepenthe\n# environment = \"x\"\n# ///\nprint(1)\n";
        let body = extract_inline_block(text).unwrap();
        assert_eq!(body, "environment = \"x\"\n");
    }

    #[test]
    fn command_spec_accepts_string_or_array() {
        let line: CommandSpec = toml::from_str("v = \"python x.py\"")
            .map(|t: toml::Value| t["v"].clone().try_into().unwrap())
            .unwrap();
        assert_eq!(line.into_argv(), vec!["python", "x.py"]);
        let argv: CommandSpec = toml::from_str("v = [\"python\", \"x.py\"]")
            .map(|t: toml::Value| t["v"].clone().try_into().unwrap())
            .unwrap();
        assert_eq!(argv.into_argv(), vec!["python", "x.py"]);
    }

    #[test]
    fn parses_pip_overlay_separately_from_conda() {
        let text = r#"
[tool.nepenthe.run]
environment = "ccrt"
registry = "file:///srv/nepenthe"
overlay = { conda = ["polars>=1"], pip = ["rich", "httpx>=0.27"] }
"#;
        let dir = std::env::temp_dir().join(format!("nepenthe-run-pip-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pyproject.toml");
        std::fs::write(&path, text).unwrap();

        let config = RunConfig::from_pyproject(&path).unwrap();
        assert_eq!(config.overlay_conda, vec!["polars>=1"]);
        assert_eq!(config.overlay_pip, vec!["rich", "httpx>=0.27"]);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn run_digest_distinguishes_conda_and_pip_overlays() {
        let base = b"lockbytes";
        let only_conda = run_digest(base, &["numpy".into()], &[]);
        let only_pip = run_digest(base, &[], &["numpy".into()]);
        let both = run_digest(base, &["numpy".into()], &["rich".into()]);
        // The same spec on a different overlay axis keys a different prefix.
        assert_ne!(only_conda, only_pip);
        assert_ne!(only_conda, both);
        assert_ne!(only_pip, both);
        // Deterministic.
        assert_eq!(only_conda, run_digest(base, &["numpy".into()], &[]));
    }
}
