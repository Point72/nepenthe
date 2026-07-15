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
/// `%environment` puts the prefix on `PATH` with `CONDA_PREFIX` set, and
/// `%runscript` execs the caller's command.
pub fn apptainer_definition(
    prefix: &Path,
    base: &str,
    env: &str,
    platform: &str,
    label: &str,
    lazy: bool,
) -> String {
    let p = prefix.display();
    let (bootstrap, from) = bootstrap_for(base);
    let files = if lazy {
        String::new()
    } else {
        format!("%files\n    {p} {p}\n\n")
    };
    format!(
        "Bootstrap: {bootstrap}\n\
         From: {from}\n\
         \n\
         {files}\
         %environment\n    export PATH=\"{p}/bin:$PATH\"\n    export CONDA_PREFIX=\"{p}\"\n\
         \n\
         %runscript\n    exec \"$@\"\n\
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
/// environment at the *same* absolute path inside the image (no relocation). The
/// default command is `python`; pass another to `docker`/`podman run` to override.
pub fn containerfile(prefix: &Path, base: &str, env: &str, platform: &str, label: &str) -> String {
    let p = prefix.display();
    format!(
        "FROM {base}\n\
         COPY . {p}\n\
         ENV PATH=\"{p}/bin:$PATH\"\n\
         ENV CONDA_PREFIX=\"{p}\"\n\
         LABEL org.nepenthe.environment=\"{env}\"\n\
         LABEL org.nepenthe.platform=\"{platform}\"\n\
         LABEL org.nepenthe.label=\"{label}\"\n\
         CMD [\"python\"]\n",
    )
}

/// The `apptainer` program to invoke (`NEPENTHE_APPTAINER`, else `apptainer`).
fn apptainer_program() -> OsString {
    std::env::var_os("NEPENTHE_APPTAINER").unwrap_or_else(|| OsString::from("apptainer"))
}

/// Package a materialized `prefix` into a SIF at `output`, bootstrapping `base`.
/// When `lazy`, the env is not copied in (bind it at run time).
pub fn package_sif(
    prefix: &Path,
    base: &str,
    output: &Path,
    env: &str,
    platform: &str,
    label: &str,
    lazy: bool,
) -> Result<(), ImageError> {
    let def = apptainer_definition(prefix, base, env, platform, label, lazy);
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
) -> Result<(), ImageError> {
    let engine = resolve_oci_engine()?;
    let content = containerfile(prefix, base, env, platform, label);
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
/// into `target` (SIF or OCI), bootstrapping from `base`.
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
    // Materialize the environment (self-contained: every package on disk).
    if !prefix.join("conda-meta").is_dir() {
        install::create(registry, coords, label, prefix).await?;
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
        let lazy =
            apptainer_definition(p, "debian:bookworm-slim", "app", "linux-64", "1.0.0", true);
        // No %files: the prefix is bound at run time, not copied in.
        assert!(!lazy.contains("%files"));
        // But PATH/CONDA_PREFIX still point at the (bound) prefix.
        assert!(lazy.contains("export CONDA_PREFIX=\"/cache/nepenthe-env/app-linux-64-py3.11\""));
        // The eager form does include %files.
        let eager =
            apptainer_definition(p, "debian:bookworm-slim", "app", "linux-64", "1.0.0", false);
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
        let def = apptainer_definition(p, "localimage:base.sif", "app", "linux-64", "1.0.0", false);
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
        );
        assert!(cf.contains("FROM debian:bookworm-slim"));
        // The build context is the prefix; COPY reconstructs it at the same path.
        assert!(cf.contains("COPY . /cache/nepenthe-env/app-linux-64-py3.11"));
        assert!(cf.contains("ENV PATH=\"/cache/nepenthe-env/app-linux-64-py3.11/bin:$PATH\""));
        assert!(cf.contains("ENV CONDA_PREFIX=\"/cache/nepenthe-env/app-linux-64-py3.11\""));
        assert!(cf.contains("LABEL org.nepenthe.environment=\"app\""));
        assert!(cf.contains("CMD [\"python\"]"));
    }
}
