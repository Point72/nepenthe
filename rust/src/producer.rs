//! Producer pipeline: solve a manifest environment and turn the result into
//! lock files, optionally publishing them to a registry.
//!
//! This is the orchestration behind the `nepenthe build` command and the
//! `nepenthe.build` Python binding. It ties [`manifest`](crate::manifest),
//! [`solve`](crate::solve), [`export`](crate::export), and
//! [`registry`](crate::registry) together so callers don't repeat the wiring.

use std::fmt;
use std::path::PathBuf;

use crate::backend::SpecStore;
use crate::export::{matrix_to_lockfiles, ExportError};
use crate::manifest::{Manifest, ManifestError, Overrides, Selector};
use crate::registry::{Coordinates, Registry, RegistryError, Release};
use crate::solve::{
    solve, solve_environment, ChannelPriorityMode, ChannelSettings, SolveError, SolveOutcome,
    SolveRequest, SolvedPackage,
};

/// What can go wrong while building an environment.
#[derive(Debug)]
pub enum BuildError {
    /// Neither an output directory nor a registry + version was requested, so
    /// the build would produce nothing.
    NothingToDo,
    /// A registry URL was given without a version, or a version without a
    /// registry — they must be provided together.
    RegistryVersionMismatch,
    /// The environment resolved to no build cells.
    EmptyEnvironment(String),
    /// Loading or composing the manifest failed.
    Manifest(ManifestError),
    /// Solving failed.
    Solve(SolveError),
    /// Building a lock file failed.
    Export(ExportError),
    /// Publishing to the registry failed.
    Registry(RegistryError),
    /// Reading or writing a file failed.
    Io(std::io::Error),
    /// Fetching a manifest or override layer from a spec backend (a
    /// `file://`/`s3://`/`https://` URL) failed.
    SpecFetch(String),
    /// A manifest loaded from a URL declared `imports`, which are not yet
    /// resolvable for remote manifests (only local manifests resolve imports).
    RemoteImports(String),
    /// A manifest-derived name would produce an unsafe lock filename (path
    /// separators or parent components that could escape the output directory).
    UnsafeName(String),
    /// Embedding the manifest into a lock's comment band failed.
    Embed(crate::embed::EmbedError),
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BuildError::NothingToDo => write!(
                f,
                "nothing to do: request an output directory to write locks and/or a registry and \
                 version to publish"
            ),
            BuildError::RegistryVersionMismatch => {
                write!(f, "a registry and a version must be provided together")
            }
            BuildError::EmptyEnvironment(env) => {
                write!(f, "environment '{env}' produced no build cells")
            }
            BuildError::Manifest(e) => write!(f, "{e}"),
            BuildError::Solve(e) => write!(f, "{e}"),
            BuildError::Export(e) => write!(f, "{e}"),
            BuildError::Registry(e) => write!(f, "{e}"),
            BuildError::Io(e) => write!(f, "{e}"),
            BuildError::SpecFetch(msg) => write!(f, "{msg}"),
            BuildError::RemoteImports(loc) => write!(
                f,
                "manifest loaded from {loc} declares `imports`, which are not supported for remote \
                 manifests; inline the imported definitions or load the manifest from a local path"
            ),
            BuildError::UnsafeName(name) => {
                write!(
                    f,
                    "unsafe environment/axis name for a lock filename: {name:?}"
                )
            }
            BuildError::Embed(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for BuildError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            BuildError::Manifest(e) => Some(e),
            BuildError::Solve(e) => Some(e),
            BuildError::Export(e) => Some(e),
            BuildError::Registry(e) => Some(e),
            BuildError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<crate::embed::EmbedError> for BuildError {
    fn from(e: crate::embed::EmbedError) -> Self {
        BuildError::Embed(e)
    }
}

impl From<ManifestError> for BuildError {
    fn from(e: ManifestError) -> Self {
        BuildError::Manifest(e)
    }
}

impl From<SolveError> for BuildError {
    fn from(e: SolveError) -> Self {
        BuildError::Solve(e)
    }
}

impl From<ExportError> for BuildError {
    fn from(e: ExportError) -> Self {
        BuildError::Export(e)
    }
}

impl From<RegistryError> for BuildError {
    fn from(e: RegistryError) -> Self {
        BuildError::Registry(e)
    }
}

impl From<std::io::Error> for BuildError {
    fn from(e: std::io::Error) -> Self {
        BuildError::Io(e)
    }
}

/// A request to build one environment from a manifest.
#[derive(Clone, Debug)]
pub struct BuildRequest {
    /// The manifest YAML to solve: a local path, or a spec-backend URL
    /// (`file://`, `s3://`, `https://`). A remote manifest must be
    /// self-contained (no `imports`).
    pub manifest: String,
    /// Optional override layer (YAML) applied before solving: a local path or a
    /// spec-backend URL, same as `manifest`.
    pub overrides: Option<String>,
    /// Environment name within the manifest to build.
    pub environment: String,
    /// Directory to write one lock file per build cell into, if any.
    pub output_dir: Option<PathBuf>,
    /// Registry root URL to publish to, if any (requires `version`).
    pub registry: Option<String>,
    /// Semver version to publish the locks under, if any (requires `registry`).
    pub version: Option<String>,
    /// Channel-priority policy for the solve.
    pub channel_priority: ChannelPriorityMode,
    /// Repodata cutoff (RFC3339) pinning the solve for reproducibility.
    pub exclude_newer: Option<String>,
    /// Build only the cell(s) matching this Python axis value, if set. `None`
    /// builds every Python in the environment's axis.
    pub python: Option<String>,
    /// Build only the cell(s) matching this variant axis value, if set. `None`
    /// builds every variant in the environment's axis.
    pub variant: Option<String>,
}

/// One built cell (a variant × python combination) of an environment.
#[derive(Clone, Debug)]
pub struct BuiltCell {
    /// Lock filename stem: `<env>[-<variant>][-py<python>]`.
    pub stem: String,
    /// The cell's selector (its variant / python axis values).
    pub selector: Selector,
    /// The platforms covered by this cell's (multi-platform) lock.
    pub platforms: Vec<String>,
    /// Where the lock was written, if an output directory was requested.
    pub lock_path: Option<PathBuf>,
    /// The releases published for this cell (one per platform), if any.
    pub releases: Vec<Release>,
}

/// Solve `request.environment` from its manifest and produce one lock per build
/// cell, writing to `output_dir` and/or publishing to `registry` under
/// `version`. At least one destination must be requested.
///
/// Performs network and file I/O; await inside a tokio runtime.
pub async fn build(request: &BuildRequest) -> Result<Vec<BuiltCell>, BuildError> {
    if request.output_dir.is_none() && request.registry.is_none() {
        return Err(BuildError::NothingToDo);
    }
    if request.registry.is_some() != request.version.is_some() {
        return Err(BuildError::RegistryVersionMismatch);
    }

    let store = SpecStore::new();
    let mut manifest = load_manifest(&store, &request.manifest)?;
    if let Some(overrides_loc) = &request.overrides {
        let overrides = load_overrides(&store, overrides_loc)?;
        manifest.apply(&overrides);
    }

    let settings = ChannelSettings::from_manifest(&manifest);
    let select = crate::manifest::Selector {
        variant: request.variant.clone(),
        python: request.python.clone(),
    };
    let matrix = solve_environment(
        &manifest,
        &request.environment,
        &settings,
        request.channel_priority,
        request.exclude_newer.clone(),
        &select,
    )
    .await?;

    if matrix.is_empty() {
        return Err(BuildError::EmptyEnvironment(request.environment.clone()));
    }

    // The platforms solved for each build cell, in the same first-seen selector
    // order that matrix_to_lockfiles groups by, so the two zip correctly.
    let cell_platforms = platforms_per_cell(&matrix);
    let locks = matrix_to_lockfiles(&matrix, &request.environment)?;

    // The composed (post-override) manifest is the exact input that was solved,
    // so embedding it lets a consumer recover and re-solve it. Band it into the
    // written lock files (portable), and publish it as a registry sidecar
    // (deduped) — either mechanism recovers the same manifest.
    let manifest_yaml = manifest.to_yaml_string()?;

    if let Some(dir) = &request.output_dir {
        std::fs::create_dir_all(dir)?;
    }

    let registry = request
        .registry
        .as_ref()
        .map(|url| Registry::new(SpecStore::new(), url.clone()));

    let mut built = Vec::with_capacity(locks.len());
    for ((selector, lock), (_, platforms)) in locks.iter().zip(cell_platforms.iter()) {
        let rendered = lock.render_to_string()?;
        let stem = cell_stem(&request.environment, selector);

        let lock_path = match &request.output_dir {
            Some(dir) => {
                // `stem` is built from manifest-controlled names; refuse any
                // value that could escape `output_dir`.
                if !is_safe_stem(&stem) {
                    return Err(BuildError::UnsafeName(stem.clone()));
                }
                let file = dir.join(format!("{stem}.lock"));
                let banded = crate::embed::embed_manifest(&rendered, &manifest_yaml)?;
                std::fs::write(&file, banded.as_bytes())?;
                Some(file)
            }
            None => None,
        };

        let mut releases = Vec::new();
        if let (Some(registry), Some(version)) = (&registry, &request.version) {
            for platform in platforms {
                let coords = cell_coordinates(&request.environment, platform, selector);
                releases.push(registry.publish_with_manifest(
                    &coords,
                    version,
                    rendered.as_bytes(),
                    Some(manifest_yaml.as_bytes()),
                )?);
            }
        }

        built.push(BuiltCell {
            stem,
            selector: selector.clone(),
            platforms: platforms.clone(),
            lock_path,
            releases,
        });
    }

    Ok(built)
}

/// Whether `location` is a spec-backend URL (vs a local filesystem path). Only
/// the schemes the [`SpecStore`] understands count; anything else (including a
/// bare or relative path) is treated as a local file.
fn is_spec_url(location: &str) -> bool {
    ["file://", "s3://", "http://", "https://"]
        .iter()
        .any(|scheme| location.starts_with(scheme))
}

/// Read a spec's bytes from `location`, which is either a spec-backend URL
/// (fetched via `store`) or a local filesystem path.
fn fetch_spec(store: &SpecStore, location: &str) -> Result<Vec<u8>, BuildError> {
    if is_spec_url(location) {
        store
            .get(location)
            .map_err(|e| BuildError::SpecFetch(e.to_string()))
    } else {
        std::fs::read(location).map_err(BuildError::Io)
    }
}

/// Load and parse the manifest at `location` (a local path or a spec-backend
/// URL). A local manifest resolves its `imports` relative to its own directory,
/// as before; a remote manifest must be self-contained (imports are rejected).
fn load_manifest(store: &SpecStore, location: &str) -> Result<Manifest, BuildError> {
    if !is_spec_url(location) {
        return Manifest::load(location).map_err(BuildError::Manifest);
    }
    let bytes = fetch_spec(store, location)?;
    let text = String::from_utf8(bytes)
        .map_err(|e| BuildError::SpecFetch(format!("manifest is not valid UTF-8: {e}")))?;
    let manifest = Manifest::from_yaml_str(&text)?;
    if !manifest.imports.is_empty() {
        return Err(BuildError::RemoteImports(crate::backend::mask_url(
            location,
        )));
    }
    Ok(manifest)
}

/// Load and parse the override layer at `location` (a local path or a
/// spec-backend URL). Overrides have no imports, so a URL is always supported.
fn load_overrides(store: &SpecStore, location: &str) -> Result<Overrides, BuildError> {
    let bytes = fetch_spec(store, location)?;
    let text = String::from_utf8(bytes)
        .map_err(|e| BuildError::SpecFetch(format!("override layer is not valid UTF-8: {e}")))?;
    Ok(Overrides::from_yaml_str(&text)?)
}

/// Registry coordinates for one cell on one platform.
fn cell_coordinates(environment: &str, platform: &str, selector: &Selector) -> Coordinates {
    let mut coords = Coordinates::new(environment.to_string(), platform.to_string());
    if let Some(python) = &selector.python {
        coords = coords.with_python(python.clone());
    }
    if let Some(variant) = &selector.variant {
        coords = coords.with_variant(variant.clone());
    }
    coords
}

/// Build a lock filename stem for one cell: `<env>[-<variant>][-py<python>]`.
pub fn cell_stem(environment: &str, selector: &Selector) -> String {
    let mut stem = environment.to_string();
    if let Some(variant) = &selector.variant {
        stem.push('-');
        stem.push_str(variant);
    }
    if let Some(python) = &selector.python {
        stem.push_str("-py");
        stem.push_str(python);
    }
    stem
}

/// Whether a lock filename stem is safe to write under an output directory:
/// non-empty, with no path separators or parent components that could escape it.
fn is_safe_stem(stem: &str) -> bool {
    !stem.is_empty() && !stem.contains('/') && !stem.contains('\\') && !stem.contains("..")
}

/// The outcome of a [`trial_solve`]: the platform solved and the resulting
/// package set, including the versions any injected specs landed at.
#[derive(Clone, Debug)]
pub struct TrialOutcome {
    /// The platform the trial was solved for.
    pub platform: String,
    /// The full solved package set (base environment plus injected specs).
    pub packages: Vec<SolvedPackage>,
}

/// Re-solve one cell of `environment` from `manifest` with `extra_specs`
/// (additional conda match-specs) injected, to check whether the environment
/// *would still solve* with those requirements — the producer-side preflight
/// behind `nepenthe try`.
///
/// Unlike [`crate::project::check`] (which compares declared deps against an
/// already-frozen lock), this actually runs the solver against the manifest, so
/// it answers "will this environment solve on its next build with my added
/// requirement?". A solver conflict surfaces as [`SolveError::Solve`].
///
/// `selector` picks the variant/python cell; `platform` defaults to the
/// environment's first resolved platform. Performs network I/O; await inside a
/// tokio runtime.
#[allow(clippy::too_many_arguments)]
pub async fn trial_solve(
    manifest: &Manifest,
    environment: &str,
    selector: &Selector,
    extra_specs: &[String],
    settings: &ChannelSettings,
    channel_priority: ChannelPriorityMode,
    platform: Option<&str>,
    exclude_newer: Option<String>,
) -> Result<TrialOutcome, BuildError> {
    let resolved = manifest
        .resolve(environment, selector)
        .map_err(BuildError::Manifest)?;
    if !resolved.pypi_dependencies.is_empty() {
        return Err(BuildError::Solve(SolveError::Unsupported(format!(
            "environment '{environment}' has pypi-dependencies {:?}; PyPI resolution is not \
             implemented",
            resolved.pypi_dependencies
        ))));
    }

    let platform = platform
        .map(str::to_string)
        .or_else(|| resolved.platforms.first().cloned())
        .unwrap_or_else(|| "linux-64".to_string());

    let mut vpkgs = manifest.virtual_packages.clone();
    vpkgs.extend(resolved.virtual_packages.clone());

    let mut request =
        SolveRequest::from_resolved(&resolved, manifest.project.channels.clone(), vpkgs)
            .with_channel_priority(channel_priority);
    request.platform = platform.clone();
    request.specs.extend(extra_specs.iter().cloned());
    if let Some(cutoff) = exclude_newer {
        request = request.with_exclude_newer(cutoff);
    }

    let outcome = solve(&request, settings).await.map_err(BuildError::Solve)?;
    Ok(TrialOutcome {
        platform,
        packages: outcome.packages(),
    })
}

/// Collect the platforms solved for each build cell, keyed by selector in
/// first-seen order (matching [`matrix_to_lockfiles`](crate::export::matrix_to_lockfiles)).
fn platforms_per_cell(matrix: &[(Selector, String, SolveOutcome)]) -> Vec<(Selector, Vec<String>)> {
    let mut cells: Vec<(Selector, Vec<String>)> = Vec::new();
    for (selector, platform, _) in matrix {
        match cells.iter_mut().find(|(s, _)| s == selector) {
            Some((_, platforms)) => {
                if !platforms.contains(platform) {
                    platforms.push(platform.clone());
                }
            }
            None => cells.push((selector.clone(), vec![platform.clone()])),
        }
    }
    cells
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cell_stem_includes_present_axes() {
        assert_eq!(cell_stem("app", &Selector::default()), "app");
        assert_eq!(
            cell_stem(
                "app",
                &Selector {
                    variant: Some("cpu".into()),
                    python: None
                }
            ),
            "app-cpu"
        );
        assert_eq!(
            cell_stem(
                "app",
                &Selector {
                    variant: None,
                    python: Some("3.11".into())
                }
            ),
            "app-py3.11"
        );
        assert_eq!(
            cell_stem(
                "app",
                &Selector {
                    variant: Some("gpu".into()),
                    python: Some("3.12".into())
                }
            ),
            "app-gpu-py3.12"
        );
    }

    #[test]
    fn unsafe_stems_are_rejected() {
        assert!(is_safe_stem("app"));
        assert!(is_safe_stem("ccrt-win-py3.11"));
        assert!(!is_safe_stem(""));
        assert!(!is_safe_stem(".."));
        assert!(!is_safe_stem("../etc/passwd"));
        assert!(!is_safe_stem("a/b"));
        assert!(!is_safe_stem("a\\b"));
    }

    #[test]
    fn is_spec_url_recognizes_backend_schemes_only() {
        assert!(is_spec_url("file:///srv/overrides.yaml"));
        assert!(is_spec_url("s3://bucket/overrides.yaml"));
        assert!(is_spec_url("https://host/overrides.yaml"));
        assert!(is_spec_url("http://host/overrides.yaml"));
        // Bare and relative paths are local, not URLs.
        assert!(!is_spec_url("overrides.yaml"));
        assert!(!is_spec_url("./overrides.yaml"));
        assert!(!is_spec_url("/abs/overrides.yaml"));
    }

    #[test]
    fn load_overrides_from_file_url_and_local_path_agree() {
        let dir = std::env::temp_dir().join(format!("nep-ovr-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("overrides.yaml");
        std::fs::write(&path, "pins:\n  grpcio: \">=1.73,<1.74\"\n").unwrap();
        let store = SpecStore::new();

        let via_path = load_overrides(&store, path.to_str().unwrap()).unwrap();
        let url = format!("file://{}", path.display());
        let via_url = load_overrides(&store, &url).unwrap();

        assert_eq!(
            via_path.pins.get("grpcio").map(String::as_str),
            Some(">=1.73,<1.74")
        );
        assert_eq!(via_path, via_url);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn remote_manifest_with_imports_is_rejected() {
        let dir = std::env::temp_dir().join(format!("nep-imp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("manifest.yaml");
        std::fs::write(&path, "imports:\n  - other.yaml\nproject:\n  name: m\n").unwrap();
        let store = SpecStore::new();

        // A local manifest may declare imports (resolved relative to its dir);
        // the same manifest loaded from a URL is rejected.
        let url = format!("file://{}", path.display());
        let err = load_manifest(&store, &url).unwrap_err();
        assert!(matches!(err, BuildError::RemoteImports(_)));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn platforms_per_cell_groups_first_seen() {
        let cell = |variant: &str, python: &str| Selector {
            variant: Some(variant.into()),
            python: Some(python.into()),
        };
        // Two cells, each across two platforms, interleaved in the matrix.
        let matrix = vec![
            (cell("cpu", "3.11"), "linux-64".to_string(), outcome()),
            (cell("cpu", "3.11"), "osx-arm64".to_string(), outcome()),
            (cell("gpu", "3.11"), "linux-64".to_string(), outcome()),
            (cell("gpu", "3.11"), "osx-arm64".to_string(), outcome()),
            // a duplicate platform for the first cell must not be added twice
            (cell("cpu", "3.11"), "linux-64".to_string(), outcome()),
        ];
        let cells = platforms_per_cell(&matrix);
        assert_eq!(cells.len(), 2);
        assert_eq!(cells[0].0, cell("cpu", "3.11"));
        assert_eq!(cells[0].1, vec!["linux-64", "osx-arm64"]);
        assert_eq!(cells[1].0, cell("gpu", "3.11"));
        assert_eq!(cells[1].1, vec!["linux-64", "osx-arm64"]);
    }

    /// A minimal, empty outcome — `platforms_per_cell` ignores its contents.
    fn outcome() -> SolveOutcome {
        SolveOutcome {
            records: Vec::new(),
            channels: Vec::new(),
            platform: String::new(),
            virtual_packages: Vec::new(),
            channel_priority: ChannelPriorityMode::default(),
            exclude_newer: None,
        }
    }
}
