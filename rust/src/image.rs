//! `nepenthe image build`: turn a published lock into a self-contained,
//! reproducible **Apptainer/SIF** image.
//!
//! The environment is materialized into a prefix (all packages on disk, no
//! conda) and packaged into a SIF by shelling out to `apptainer build` — its
//! CLI is the stable integration surface, mirroring how PyPI overlays delegate
//! to `uv`. Point `NEPENTHE_APPTAINER` at a binary, else `apptainer` on `PATH`
//! is used.
//!
//! **Why a base image, not `scratch`.** conda-forge binaries are linked against
//! the system dynamic loader (`/lib64/ld-linux-*`) and the SIF runscript is
//! interpreted by `/bin/sh`; an empty `scratch` root has neither. So the image
//! bootstraps from a small **glibc** base (default `debian:bookworm-slim`) and
//! the materialized environment is copied in on top — "self-contained" means all
//! *packages* are baked in, atop a minimal OS that supplies the loader and shell.
//!
//! **No prefix relocation.** The environment is copied to the *same* absolute
//! path inside the image as it occupies on the host, so conda's baked prefixes,
//! shebangs, and `RPATH`s resolve unchanged.
//!
//! **Activated images.** An [`ImageConfig`] bakes environment variables into the
//! image (`ENV` / `%environment`), so they hold however the container starts.
//! With `activate`, the OCI entrypoint and the SIF runscript also source the
//! prefix's `etc/conda/activate.d` hooks before running the command, as a full
//! activation would.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

use crate::install::{self, InstallError};
use crate::registry::{Coordinates, Label, Registry, RegistryError};

/// The default base image: a small glibc OS supplying the loader and `/bin/sh`.
pub const DEFAULT_BASE_IMAGE: &str = "debian:bookworm-slim";

/// Errors raised while building an image.
#[derive(Debug)]
pub enum ImageError {
    /// Resolving or pulling from the registry failed.
    Registry(RegistryError),
    /// Materializing the environment prefix failed.
    Install(InstallError),
    /// A filesystem operation failed.
    Io(std::io::Error),
    /// The `apptainer` binary was not found.
    ApptainerNotFound,
    /// `apptainer build` exited non-zero.
    Apptainer(String),
    /// No OCI engine (`podman`/`docker`) was found.
    OciNotFound,
    /// The OCI engine `build` exited non-zero.
    Oci(String),
    /// The image configuration is invalid.
    Config(String),
}

impl fmt::Display for ImageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImageError::Registry(e) => write!(f, "{e}"),
            ImageError::Install(e) => write!(f, "{e}"),
            ImageError::Io(e) => write!(f, "{e}"),
            ImageError::ApptainerNotFound => write!(
                f,
                "apptainer not found; install apptainer (https://apptainer.org/) or set \
                 NEPENTHE_APPTAINER to its path to build images"
            ),
            ImageError::Apptainer(msg) => write!(f, "apptainer build failed: {msg}"),
            ImageError::OciNotFound => write!(
                f,
                "no OCI engine found; install podman or docker, or set NEPENTHE_OCI_ENGINE to one, \
                 to build OCI images"
            ),
            ImageError::Oci(msg) => write!(f, "OCI build failed: {msg}"),
            ImageError::Config(msg) => write!(f, "invalid image configuration: {msg}"),
        }
    }
}

impl std::error::Error for ImageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ImageError::Registry(e) => Some(e),
            ImageError::Install(e) => Some(e),
            ImageError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<RegistryError> for ImageError {
    fn from(e: RegistryError) -> Self {
        ImageError::Registry(e)
    }
}

impl From<InstallError> for ImageError {
    fn from(e: InstallError) -> Self {
        ImageError::Install(e)
    }
}

impl From<std::io::Error> for ImageError {
    fn from(e: std::io::Error) -> Self {
        ImageError::Io(e)
    }
}

/// A completed image build.
#[derive(Debug, Clone)]
pub struct ImageSummary {
    /// The artifact produced: a SIF file path, or an OCI image tag.
    pub artifact: String,
    /// The prefix the environment was materialized into (and its in-image path).
    pub prefix: PathBuf,
    /// The platform built for.
    pub platform: String,
}

/// What to package a materialized environment into.
#[derive(Debug, Clone)]
pub enum ImageTarget {
    /// An Apptainer/SIF image file at the given path.
    Sif {
        /// Output `.sif` path.
        output: PathBuf,
        /// Build a *lazy* image: the environment is not copied in, but bound at
        /// run time. Small and fast, but not portable (needs the host prefix).
        lazy: bool,
    },
    /// An OCI image loaded into the local engine store under the given tag.
    Oci {
        /// Image tag, e.g. `app:1.0.0`.
        tag: String,
    },
}

/// Runtime settings baked into an image.
#[derive(Debug, Clone, Default)]
pub struct ImageConfig {
    /// Environment variables set in the image, in order; a later key wins.
    pub env: Vec<(String, String)>,
    /// Source the prefix's `etc/conda/activate.d` hooks before the command (the
    /// OCI entrypoint and the SIF runscript).
    pub activate: bool,
    /// Run an OCI image as this user (`USER`). The base must provide it.
    pub user: Option<String>,
}

impl ImageConfig {
    /// Check that every variable name is a portable shell identifier, values
    /// are single-line, and the user is a single token.
    pub fn validate(&self) -> Result<(), ImageError> {
        for (key, value) in &self.env {
            let mut chars = key.chars();
            let valid_name = chars
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
            if !valid_name {
                return Err(ImageError::Config(format!(
                    "environment variable name `{key}` is not a valid identifier"
                )));
            }
            if value.chars().any(|c| matches!(c, '\n' | '\r' | '\0')) {
                return Err(ImageError::Config(format!(
                    "environment variable `{key}` must be a single line"
                )));
            }
        }
        if let Some(user) = &self.user {
            if user.is_empty() || user.chars().any(|c| c.is_whitespace() || c.is_control()) {
                return Err(ImageError::Config(format!("invalid image user `{user}`")));
            }
        }
        Ok(())
    }
}

/// The shell command that sources a prefix's `activate.d` hooks.
fn activation_command(prefix: &Path) -> String {
    let p = crate::install::sh_single_quote(&prefix.display().to_string());
    format!(
        "for __nepenthe_hook in {p}/etc/conda/activate.d/*.sh; do \
         [ -f \"$__nepenthe_hook\" ] && . \"$__nepenthe_hook\"; done"
    )
}

/// Quote `value` for a Containerfile `ENV` instruction.
fn containerfile_quote(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for c in value.chars() {
        if matches!(c, '"' | '\\' | '$') {
            quoted.push('\\');
        }
        quoted.push(c);
    }
    quoted.push('"');
    quoted
}

/// Make the prefix's package metadata readable by every user: images run as
/// users other than the one who installed the prefix.
#[cfg(unix)]
fn make_metadata_readable(prefix: &Path) -> Result<(), ImageError> {
    use std::os::unix::fs::PermissionsExt;

    let meta = prefix.join("conda-meta");
    let entries = match std::fs::read_dir(&meta) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        let metadata = entry.metadata()?;
        if metadata.is_file() {
            let mut permissions = metadata.permissions();
            permissions.set_mode(permissions.mode() | 0o444);
            std::fs::set_permissions(entry.path(), permissions)?;
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn make_metadata_readable(_prefix: &Path) -> Result<(), ImageError> {
    Ok(())
}

/// Split a `--base` spec into an Apptainer `(bootstrap, from)` pair.
///
/// A `localimage:` prefix selects Apptainer's `localimage` bootstrap agent — the
/// base is an existing local SIF — which is what lets a nepenthe image layer on
/// top of another SIF. Anything else is a Docker/OCI reference pulled via
/// `Bootstrap: docker`.
pub fn bootstrap_for(base: &str) -> (&str, &str) {
    match base.strip_prefix("localimage:") {
        Some(path) => ("localimage", path),
        None => ("docker", base),
    }
}

/// Render the Apptainer definition that packages a materialized conda `prefix`
/// into an image atop `base`.
///
/// When `lazy` is false the environment is copied to the *same* absolute path
/// inside the image (no relocation, self-contained). When `lazy` is true the
/// `%files` section is omitted — the prefix is expected to be bind-mounted at
/// run time, yielding a tiny image that shares the host's packages.
/// `%environment` puts the prefix on `PATH` with `CONDA_PREFIX` and `config.env`
/// set, and `%runscript` execs the caller's command, after sourcing the
/// activation hooks when `config.activate`.
pub fn apptainer_definition(
    prefix: &Path,
    base: &str,
    env: &str,
    platform: &str,
    label: &str,
    lazy: bool,
    config: &ImageConfig,
) -> String {
    let p = prefix.display();
    let (bootstrap, from) = bootstrap_for(base);
    let files = if lazy {
        String::new()
    } else {
        format!("%files\n    {p} {p}\n\n")
    };
    let mut environment = String::new();
    for (key, value) in &config.env {
        environment.push_str(&format!(
            "    export {key}={}\n",
            crate::install::sh_single_quote(value)
        ));
    }
    let activate = if config.activate {
        format!("    {}\n", activation_command(prefix))
    } else {
        String::new()
    };
    format!(
        "Bootstrap: {bootstrap}\n\
         From: {from}\n\
         \n\
         {files}\
         %environment\n    export PATH=\"{p}/bin:$PATH\"\n    export CONDA_PREFIX=\"{p}\"\n\
         {environment}\
         \n\
         %runscript\n{activate}    exec \"$@\"\n\
         \n\
         %labels\n    \
         org.nepenthe.environment {env}\n    \
         org.nepenthe.platform {platform}\n    \
         org.nepenthe.label {label}\n",
    )
}

/// Render an OCI `Containerfile` (Dockerfile) that packages a materialized conda
/// `prefix` into a self-contained image atop `base`.
///
/// The build context is the prefix itself, so `COPY . <prefix>` reconstructs the
/// environment at the *same* absolute path inside the image (no relocation).
/// `config.env` becomes `ENV` instructions and `config.user` the `USER`. With
/// `config.activate`, the entrypoint sources the activation hooks before running
/// the command. The default command is `python`; pass another to
/// `docker`/`podman run` to override.
pub fn containerfile(
    prefix: &Path,
    base: &str,
    env: &str,
    platform: &str,
    label: &str,
    config: &ImageConfig,
) -> String {
    let p = prefix.display();
    let mut out = format!(
        "FROM {base}\n\
         COPY . {p}\n\
         ENV PATH=\"{p}/bin:$PATH\"\n\
         ENV CONDA_PREFIX=\"{p}\"\n",
    );
    for (key, value) in &config.env {
        out.push_str(&format!("ENV {key}={}\n", containerfile_quote(value)));
    }
    out.push_str(&format!(
        "LABEL org.nepenthe.environment=\"{env}\"\n\
         LABEL org.nepenthe.platform=\"{platform}\"\n\
         LABEL org.nepenthe.label=\"{label}\"\n",
    ));
    if let Some(user) = &config.user {
        out.push_str(&format!("USER {user}\n"));
    }
    if config.activate {
        let entrypoint = [
            "/bin/sh".to_string(),
            "-c".to_string(),
            format!("{}; exec \"$@\"", activation_command(prefix)),
            "nepenthe".to_string(),
        ];
        out.push_str(&format!(
            "ENTRYPOINT {}\n",
            serde_json::to_string(&entrypoint).expect("strings serialize")
        ));
    }
    out.push_str("CMD [\"python\"]\n");
    out
}

/// The `apptainer` program to invoke (`NEPENTHE_APPTAINER`, else `apptainer`).
fn apptainer_program() -> OsString {
    std::env::var_os("NEPENTHE_APPTAINER").unwrap_or_else(|| OsString::from("apptainer"))
}

/// Package a materialized `prefix` into a SIF at `output`, bootstrapping `base`.
/// When `lazy`, the env is not copied in (bind it at run time).
#[allow(clippy::too_many_arguments)]
pub fn package_sif(
    prefix: &Path,
    base: &str,
    output: &Path,
    env: &str,
    platform: &str,
    label: &str,
    lazy: bool,
    config: &ImageConfig,
) -> Result<(), ImageError> {
    config.validate()?;
    if config.user.is_some() {
        return Err(ImageError::Config(
            "a runtime user is only supported for OCI images".into(),
        ));
    }
    make_metadata_readable(prefix)?;
    let def = apptainer_definition(prefix, base, env, platform, label, lazy, config);
    let def_path = prefix.join(".nepenthe-image.def");
    std::fs::write(&def_path, def)?;
    let status = Command::new(apptainer_program())
        .arg("build")
        .arg("--force")
        .arg(output)
        .arg(&def_path)
        .status()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ImageError::ApptainerNotFound
            } else {
                ImageError::Io(e)
            }
        })?;
    if !status.success() {
        return Err(ImageError::Apptainer(format!(
            "`apptainer build` exited with {}",
            status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".to_string())
        )));
    }
    Ok(())
}

/// Resolve the OCI engine to use: `NEPENTHE_OCI_ENGINE`, else `podman`, else
/// `docker`, whichever is found on `PATH` first.
fn resolve_oci_engine() -> Result<OsString, ImageError> {
    if let Some(engine) = std::env::var_os("NEPENTHE_OCI_ENGINE") {
        return Ok(engine);
    }
    for candidate in ["podman", "docker"] {
        let found = Command::new(candidate)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if found {
            return Ok(OsString::from(candidate));
        }
    }
    Err(ImageError::OciNotFound)
}

/// Package a materialized `prefix` into an OCI image tagged `tag`, bootstrapping
/// `base`, by shelling out to `podman`/`docker build` with the prefix as context.
pub fn package_oci(
    prefix: &Path,
    base: &str,
    tag: &str,
    env: &str,
    platform: &str,
    label: &str,
    config: &ImageConfig,
) -> Result<(), ImageError> {
    config.validate()?;
    let engine = resolve_oci_engine()?;
    make_metadata_readable(prefix)?;
    let content = containerfile(prefix, base, env, platform, label, config);
    // Write the Containerfile outside the build context (the prefix) so it is
    // not copied into the image by `COPY .`.
    let name = prefix
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "nepenthe".to_string());
    let cf_path = prefix
        .parent()
        .unwrap_or(prefix)
        .join(format!(".{name}.Containerfile"));
    std::fs::write(&cf_path, content)?;

    let result = Command::new(&engine)
        .arg("build")
        .arg("-t")
        .arg(tag)
        .arg("-f")
        .arg(&cf_path)
        .arg(prefix)
        .status()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ImageError::OciNotFound
            } else {
                ImageError::Io(e)
            }
        });
    let _ = std::fs::remove_file(&cf_path);
    let status = result?;
    if !status.success() {
        return Err(ImageError::Oci(format!(
            "`{} build` exited with {}",
            engine.to_string_lossy(),
            status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".to_string())
        )));
    }
    Ok(())
}

/// Options for [`exec_in_image`].
#[derive(Debug, Clone, Default)]
pub struct ExecOptions {
    /// Host directories to bind into the container at their own paths.
    pub binds: Vec<PathBuf>,
    /// Directories to prepend to `PYTHONPATH` inside the container.
    pub pythonpath: Vec<PathBuf>,
    /// Give the container an ephemeral in-memory writable layer
    /// (`--writable-tmpfs`): a read-only base image plus a throwaway overlay.
    pub writable_tmpfs: bool,
    /// Use a persistent EXT3 overlay image for writes (`--overlay <img>`),
    /// created on first use. The read-only base SIF is never modified.
    pub overlay_image: Option<PathBuf>,
}

/// The default size, in MiB, of a persistent overlay image.
const OVERLAY_SIZE_MIB: u32 = 1024;

/// Create an EXT3 writable overlay image at `path` (if absent) of `size_mib`
/// megabytes, via `apptainer overlay create`.
pub fn create_overlay_image(path: &Path, size_mib: u32) -> Result<(), ImageError> {
    if path.exists() {
        return Ok(());
    }
    let status = Command::new(apptainer_program())
        .arg("overlay")
        .arg("create")
        .arg("--size")
        .arg(size_mib.to_string())
        .arg(path)
        .status()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ImageError::ApptainerNotFound
            } else {
                ImageError::Io(e)
            }
        })?;
    if !status.success() {
        return Err(ImageError::Apptainer(format!(
            "`apptainer overlay create` exited with {}",
            status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".to_string())
        )));
    }
    Ok(())
}

/// Run `program` (with `args`) inside a SIF `image` via `apptainer exec`,
/// applying `opts` (binds, `PYTHONPATH`, and an optional writable overlay layer).
/// Inherits stdio.
pub fn exec_in_image(
    image: &Path,
    program: &OsStr,
    args: &[OsString],
    opts: &ExecOptions,
) -> Result<ExitStatus, ImageError> {
    if let Some(overlay) = &opts.overlay_image {
        create_overlay_image(overlay, OVERLAY_SIZE_MIB)?;
    }
    let mut command = Command::new(apptainer_program());
    command.arg("exec");
    for bind in &opts.binds {
        command.arg("--bind").arg(bind);
    }
    if opts.writable_tmpfs {
        command.arg("--writable-tmpfs");
    }
    if let Some(overlay) = &opts.overlay_image {
        command.arg("--overlay").arg(overlay);
    }
    if !opts.pythonpath.is_empty() {
        let joined = std::env::join_paths(&opts.pythonpath)
            .map_err(|e| ImageError::Io(std::io::Error::other(e.to_string())))?;
        let mut env = OsString::from("PYTHONPATH=");
        env.push(joined);
        command.arg("--env").arg(env);
    }
    command.arg(image).arg(program).args(args);
    command.status().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            ImageError::ApptainerNotFound
        } else {
            ImageError::Io(e)
        }
    })
}

/// Materialize `coords`@`label` from `registry` into `prefix`, then package it
/// into `target` (SIF or OCI), bootstrapping from `base`. The image is activated:
/// it exports the release's activation variables (identity and manifest
/// `activation.env`) and sources the prefix's activation hooks before the command.
///
/// Performs network and filesystem I/O and shells out to the image engine; await
/// inside a tokio runtime.
pub async fn build(
    registry: &Registry,
    coords: &Coordinates,
    label: &Label,
    base: &str,
    prefix: &Path,
    target: &ImageTarget,
    label_text: &str,
) -> Result<ImageSummary, ImageError> {
    build_with_activation_script(
        registry,
        coords,
        label,
        base,
        prefix,
        target,
        label_text,
        true,
        &ImageConfig::default(),
    )
    .await
}

/// Like [`build`], with `activation_script` controlling nepenthe's identity hook
/// and its baked-in variables, and `extra` supplying further variables (which
/// win over the release's) and an OCI runtime user.
#[allow(clippy::too_many_arguments)]
pub async fn build_with_activation_script(
    registry: &Registry,
    coords: &Coordinates,
    label: &Label,
    base: &str,
    prefix: &Path,
    target: &ImageTarget,
    label_text: &str,
    activation_script: bool,
    extra: &ImageConfig,
) -> Result<ImageSummary, ImageError> {
    extra.validate()?;
    if extra.user.is_some() && matches!(target, ImageTarget::Sif { .. }) {
        return Err(ImageError::Config(
            "a runtime user is only supported for OCI images".into(),
        ));
    }
    let mut config = ImageConfig {
        env: Vec::new(),
        activate: true,
        user: extra.user.clone(),
    };
    if activation_script {
        config.env = install::registry_activation_env(registry, coords, label)?;
    }
    config.env.extend(extra.env.iter().cloned());

    // Materialize the environment (self-contained: every package on disk).
    if !prefix.join("conda-meta").is_dir() {
        install::create_with_activation_script(
            registry,
            coords,
            label,
            prefix,
            install::LinkScripts::Skip,
            activation_script,
        )
        .await?;
    } else {
        install::ensure_activation_script(
            prefix,
            &coords.environment,
            &coords.platform,
            activation_script,
        )?;
    }

    let artifact = match target {
        ImageTarget::Sif { output, lazy } => {
            package_sif(
                prefix,
                base,
                output,
                &coords.environment,
                &coords.platform,
                label_text,
                *lazy,
                &config,
            )?;
            output.display().to_string()
        }
        ImageTarget::Oci { tag } => {
            package_oci(
                prefix,
                base,
                tag,
                &coords.environment,
                &coords.platform,
                label_text,
                &config,
            )?;
            tag.clone()
        }
    };

    Ok(ImageSummary {
        artifact,
        prefix: prefix.to_path_buf(),
        platform: coords.platform.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn definition_bootstraps_from_base_and_copies_prefix_in_place() {
        let def = apptainer_definition(
            Path::new("/cache/nepenthe-env/app-linux-64-py3.11"),
            "debian:bookworm-slim",
            "app",
            "linux-64",
            "1.0.0",
            false,
            &ImageConfig::default(),
        );
        assert!(def.contains("Bootstrap: docker"));
        assert!(def.contains("From: debian:bookworm-slim"));
        // The env is copied to the same absolute path inside the image.
        assert!(def.contains(
            "    /cache/nepenthe-env/app-linux-64-py3.11 /cache/nepenthe-env/app-linux-64-py3.11"
        ));
        // It is put on PATH, and the command is exec'd by the runscript.
        assert!(def.contains("export PATH=\"/cache/nepenthe-env/app-linux-64-py3.11/bin:$PATH\""));
        assert!(def.contains("exec \"$@\""));
        // Provenance labels.
        assert!(def.contains("org.nepenthe.environment app"));
        assert!(def.contains("org.nepenthe.platform linux-64"));
        assert!(def.contains("org.nepenthe.label 1.0.0"));
    }

    #[test]
    fn lazy_definition_omits_files_section() {
        let p = Path::new("/cache/nepenthe-env/app-linux-64-py3.11");
        let lazy = apptainer_definition(
            p,
            "debian:bookworm-slim",
            "app",
            "linux-64",
            "1.0.0",
            true,
            &ImageConfig::default(),
        );
        // No %files: the prefix is bound at run time, not copied in.
        assert!(!lazy.contains("%files"));
        // But PATH/CONDA_PREFIX still point at the (bound) prefix.
        assert!(lazy.contains("export CONDA_PREFIX=\"/cache/nepenthe-env/app-linux-64-py3.11\""));
        // The eager form does include %files.
        let eager = apptainer_definition(
            p,
            "debian:bookworm-slim",
            "app",
            "linux-64",
            "1.0.0",
            false,
            &ImageConfig::default(),
        );
        assert!(eager.contains("%files"));
    }

    #[test]
    fn localimage_base_uses_localimage_bootstrap() {
        assert_eq!(
            bootstrap_for("debian:bookworm-slim"),
            ("docker", "debian:bookworm-slim")
        );
        assert_eq!(
            bootstrap_for("localimage:base.sif"),
            ("localimage", "base.sif")
        );

        let p = Path::new("/cache/nepenthe-env/app-linux-64-py3.11");
        let def = apptainer_definition(
            p,
            "localimage:base.sif",
            "app",
            "linux-64",
            "1.0.0",
            false,
            &ImageConfig::default(),
        );
        // Layered on an existing SIF, not pulled from a registry.
        assert!(def.contains("Bootstrap: localimage"));
        assert!(def.contains("From: base.sif"));
        // The current env's prefix is still copied in on top of the base.
        assert!(def.contains("%files"));
    }

    #[test]
    fn containerfile_bootstraps_from_base_and_copies_prefix_in_place() {
        let cf = containerfile(
            Path::new("/cache/nepenthe-env/app-linux-64-py3.11"),
            "debian:bookworm-slim",
            "app",
            "linux-64",
            "1.0.0",
            &ImageConfig::default(),
        );
        assert!(cf.contains("FROM debian:bookworm-slim"));
        // The build context is the prefix; COPY reconstructs it at the same path.
        assert!(cf.contains("COPY . /cache/nepenthe-env/app-linux-64-py3.11"));
        assert!(cf.contains("ENV PATH=\"/cache/nepenthe-env/app-linux-64-py3.11/bin:$PATH\""));
        assert!(cf.contains("ENV CONDA_PREFIX=\"/cache/nepenthe-env/app-linux-64-py3.11\""));
        assert!(cf.contains("LABEL org.nepenthe.environment=\"app\""));
        assert!(cf.contains("CMD [\"python\"]"));
        // Without configuration there is no entrypoint or user.
        assert!(!cf.contains("ENTRYPOINT"));
        assert!(!cf.contains("USER"));
    }

    fn activated() -> ImageConfig {
        ImageConfig {
            env: vec![
                ("NEPENTHE_ENVIRONMENT".into(), "app".into()),
                ("TEAM_NOTE".into(), "it's \"$HOME\" \\ done".into()),
            ],
            activate: true,
            user: Some("ray".into()),
        }
    }

    #[test]
    fn containerfile_bakes_env_user_and_activating_entrypoint() {
        let cf = containerfile(
            Path::new("/opt/app env"),
            "debian:bookworm-slim",
            "app",
            "linux-64",
            "1.0.0",
            &activated(),
        );
        assert!(cf.contains("ENV NEPENTHE_ENVIRONMENT=\"app\"\n"));
        // Quotes, `$`, and backslashes are escaped so the value is literal.
        assert!(cf.contains(r#"ENV TEAM_NOTE="it's \"\$HOME\" \\ done""#));
        assert!(cf.contains("USER ray\n"));
        let entrypoint = cf
            .lines()
            .find_map(|line| line.strip_prefix("ENTRYPOINT "))
            .expect("entrypoint");
        let argv: Vec<String> = serde_json::from_str(entrypoint).unwrap();
        assert_eq!(argv[0], "/bin/sh");
        assert!(argv[2].contains("'/opt/app env'/etc/conda/activate.d/*.sh"));
        assert!(argv[2].ends_with("exec \"$@\""));
        // USER and ENTRYPOINT precede the default command.
        assert!(cf.find("USER").unwrap() < cf.find("CMD").unwrap());
        assert!(cf.ends_with("CMD [\"python\"]\n"));
    }

    #[test]
    fn definition_bakes_env_and_activates_in_runscript() {
        let def = apptainer_definition(
            Path::new("/opt/app"),
            "debian:bookworm-slim",
            "app",
            "linux-64",
            "1.0.0",
            false,
            &ImageConfig {
                user: None,
                ..activated()
            },
        );
        let environment = &def[def.find("%environment").unwrap()..def.find("%runscript").unwrap()];
        assert!(environment.contains("    export NEPENTHE_ENVIRONMENT='app'\n"));
        assert!(environment.contains(r#"    export TEAM_NOTE='it'\''s "$HOME" \ done'"#));
        let runscript = &def[def.find("%runscript").unwrap()..def.find("%labels").unwrap()];
        assert!(runscript.contains("'/opt/app'/etc/conda/activate.d/*.sh"));
        assert!(runscript.find("activate.d").unwrap() < runscript.find("exec").unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn activation_command_sources_hooks_in_order_and_runs_command() {
        let dir = std::env::temp_dir().join(format!("nepenthe-image-hooks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let hooks = dir.join("etc/conda/activate.d");
        std::fs::create_dir_all(&hooks).unwrap();
        std::fs::write(hooks.join("a.sh"), "export ORDER=a\n").unwrap();
        std::fs::write(hooks.join("b.sh"), "export ORDER=\"$ORDER b\"\nfalse\n").unwrap();
        std::fs::write(hooks.join("c.bat"), "exit 1\n").unwrap();
        let output = std::process::Command::new("/bin/sh")
            .args([
                "-c",
                &format!("{}; exec \"$@\"", activation_command(&dir)),
                "nepenthe",
                "sh",
                "-c",
                "printf %s \"$ORDER\"",
            ])
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8(output.stdout).unwrap(), "a b");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_rejects_invalid_names_values_and_users() {
        let bad = |env: Vec<(&str, &str)>, user: Option<&str>| ImageConfig {
            env: env.into_iter().map(|(k, v)| (k.into(), v.into())).collect(),
            activate: true,
            user: user.map(Into::into),
        };
        assert!(bad(vec![("OK_1", "value with spaces")], Some("ray"))
            .validate()
            .is_ok());
        assert!(bad(vec![("1BAD", "x")], None).validate().is_err());
        assert!(bad(vec![("BAD-NAME", "x")], None).validate().is_err());
        assert!(bad(vec![("", "x")], None).validate().is_err());
        assert!(bad(vec![("OK", "two\nlines")], None).validate().is_err());
        assert!(bad(vec![], Some("ray\nRUN rm -rf /")).validate().is_err());
        assert!(bad(vec![], Some("")).validate().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn metadata_becomes_world_readable() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("nepenthe-image-meta-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("conda-meta")).unwrap();
        let record = dir.join("conda-meta/pkg-1.0-0.json");
        std::fs::write(&record, "{}").unwrap();
        std::fs::set_permissions(&record, std::fs::Permissions::from_mode(0o600)).unwrap();
        make_metadata_readable(&dir).unwrap();
        assert_eq!(
            std::fs::metadata(&record).unwrap().permissions().mode() & 0o777,
            0o644
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Publish an empty lock whose embedded manifest declares activation
    /// variables, so an image build needs no packages or network.
    #[cfg(unix)]
    fn activation_registry(root: &Path) -> (Registry, Coordinates) {
        let platform = rattler_conda_types::Platform::current().to_string();
        let lock = format!(
            "version: 6\nplatforms:\n- name: {platform}\n  virtual-packages: []\n\
             environments:\n  research:\n    channels: []\n    packages:\n      {platform}: []\n\
             packages: []\n"
        );
        let manifest = format!(
            "project:\n  name: fixture\n  channels: []\n  platforms: [{platform}]\n\
             activation:\n  env:\n    TEAM_ENV: research\n    SHARED: manifest\n\
             environments:\n  research: {{}}\n"
        );
        let banded = crate::embed::embed_manifest(&lock, &manifest).unwrap();
        let registry = Registry::new(
            crate::backend::SpecStore::new(),
            format!("file://{}", root.join("registry").display()),
        );
        let coords = Coordinates::new("research", platform);
        registry
            .publish(&coords, "1.2.3", banded.as_bytes())
            .unwrap();
        (registry, coords)
    }

    /// Stand-in engine that saves the recipe it was asked to build.
    #[cfg(unix)]
    fn recording_engine(dir: &Path, name: &str, recipe_arg: usize) -> (PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;

        let recipe = dir.join(format!("{name}.recipe"));
        let engine = dir.join(name);
        std::fs::write(
            &engine,
            format!(
                "#!/bin/sh\ncp \"${recipe_arg}\" {}\n",
                crate::install::sh_single_quote(&recipe.display().to_string())
            ),
        )
        .unwrap();
        std::fs::set_permissions(&engine, std::fs::Permissions::from_mode(0o755)).unwrap();
        (engine, recipe)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn build_bakes_release_activation_and_extra_config() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("nepenthe-image-build-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (registry, coords) = activation_registry(&dir);
        let (oci, oci_recipe) = recording_engine(&dir, "oci-engine", 5);
        let (apptainer, sif_recipe) = recording_engine(&dir, "apptainer", 4);
        std::env::set_var("NEPENTHE_OCI_ENGINE", &oci);
        std::env::set_var("NEPENTHE_APPTAINER", &apptainer);

        let prefix = dir.join("prefix");
        let extra = ImageConfig {
            env: vec![("SHARED".into(), "cli".into())],
            activate: false,
            user: Some("ray".into()),
        };
        build_with_activation_script(
            &registry,
            &coords,
            &Label::Latest,
            "debian:bookworm-slim",
            &prefix,
            &ImageTarget::Oci {
                tag: "research:1.2.3".into(),
            },
            "latest",
            true,
            &extra,
        )
        .await
        .unwrap();
        let cf = std::fs::read_to_string(&oci_recipe).unwrap();
        let env: Vec<&str> = cf.lines().filter(|l| l.starts_with("ENV ")).collect();
        assert_eq!(
            env[2..],
            [
                "ENV NEPENTHE_ENVIRONMENT=\"research\"",
                &format!("ENV NEPENTHE_PLATFORM=\"{}\"", coords.platform),
                "ENV NEPENTHE_VERSION=\"1.2.3\"",
                "ENV SHARED=\"manifest\"",
                "ENV TEAM_ENV=\"research\"",
                "ENV SHARED=\"cli\"",
            ]
        );
        assert!(cf.contains("USER ray\n"));
        assert!(cf.contains("ENTRYPOINT [\"/bin/sh\""));
        let meta = std::fs::read_dir(prefix.join("conda-meta")).unwrap();
        for entry in meta {
            let mode = entry.unwrap().metadata().unwrap().permissions().mode();
            assert_eq!(mode & 0o444, 0o444);
        }

        // SIF images take the same variables, but no runtime user.
        let sif = ImageTarget::Sif {
            output: dir.join("research.sif"),
            lazy: false,
        };
        let rejected = build_with_activation_script(
            &registry,
            &coords,
            &Label::Latest,
            "debian:bookworm-slim",
            &prefix,
            &sif,
            "latest",
            true,
            &extra,
        )
        .await;
        assert!(matches!(rejected, Err(ImageError::Config(_))));
        build_with_activation_script(
            &registry,
            &coords,
            &Label::Latest,
            "debian:bookworm-slim",
            &prefix,
            &sif,
            "latest",
            false,
            &ImageConfig::default(),
        )
        .await
        .unwrap();
        let def = std::fs::read_to_string(&sif_recipe).unwrap();
        // With the identity hook off, nothing beyond PATH/CONDA_PREFIX is baked in.
        assert!(!def.contains("NEPENTHE_ENVIRONMENT"));
        assert!(def.contains("activate.d/*.sh"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
