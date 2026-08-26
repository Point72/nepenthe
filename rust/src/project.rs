//! Consumer-side project integration.
//!
//! A repository that *consumes* a published nepenthe environment declares it in
//! its `pyproject.toml` under `[tool.nepenthe]` — a small **reference** (which
//! environment, which version), not the environment definition. From that
//! reference this module can:
//!
//! - [`read`] the `[tool.nepenthe]` stanza and the project's
//!   `[project.dependencies]`,
//! - [`sync`] the referenced environment into a prefix (no conda required),
//! - [`check`] the project's declared dependencies against the environment's
//!   pinned package set, reporting conflicts and gaps.
//!
//! nepenthe environments are **pre-solved shared artifacts** — `sync` installs
//! the exact published lock, it does not re-resolve the environment together
//! with the project's dependencies. [`check`] is the seam that keeps the two in
//! sync: it tells you whether what your project declares is compatible with the
//! environment you reference.

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use rattler_conda_types::{ParseStrictness, Version, VersionSpec};
use serde::Deserialize;

use crate::backend::SpecStore;
use crate::install::{self, InstallError, InstallSummary, LinkScripts, PackageId};
use crate::name_map::{self, normalize_name};
use crate::registry::{Coordinates, Label, Registry, RegistryError};

/// Default prefix to install into when the stanza omits one.
const DEFAULT_PREFIX: &str = ".venv";
/// Default version label when the stanza omits one.
const DEFAULT_LABEL: &str = "latest";

/// Errors raised by the consumer-side project integration.
#[derive(Debug)]
pub enum ProjectError {
    /// The `pyproject.toml` could not be read.
    Io(std::io::Error),
    /// The `pyproject.toml` was not valid TOML.
    Toml(String),
    /// The file has no `[tool.nepenthe]` stanza.
    Missing,
    /// A registry lookup or pull failed.
    Registry(RegistryError),
    /// Installing the environment failed.
    Install(InstallError),
}

impl fmt::Display for ProjectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProjectError::Io(e) => write!(f, "reading pyproject.toml: {e}"),
            ProjectError::Toml(msg) => write!(f, "parsing pyproject.toml: {msg}"),
            ProjectError::Missing => write!(
                f,
                "pyproject.toml has no [tool.nepenthe] section (declare the environment to consume)"
            ),
            ProjectError::Registry(e) => write!(f, "{e}"),
            ProjectError::Install(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ProjectError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ProjectError::Io(e) => Some(e),
            ProjectError::Registry(e) => Some(e),
            ProjectError::Install(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for ProjectError {
    fn from(e: std::io::Error) -> Self {
        ProjectError::Io(e)
    }
}

impl From<RegistryError> for ProjectError {
    fn from(e: RegistryError) -> Self {
        ProjectError::Registry(e)
    }
}

impl From<InstallError> for ProjectError {
    fn from(e: InstallError) -> Self {
        ProjectError::Install(e)
    }
}

/// The `[tool.nepenthe]` stanza: which published environment a project consumes.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectRef {
    /// Environment name to consume.
    pub environment: String,
    /// Registry root URL the environment is published to.
    pub registry: String,
    /// Version label to resolve (`latest`, an exact version, or a range).
    /// Defaults to `latest`.
    #[serde(default)]
    pub version: Option<String>,
    /// Target platform (defaults to the current platform).
    #[serde(default)]
    pub platform: Option<String>,
    /// Python axis value, if the environment fans out over python.
    #[serde(default)]
    pub python: Option<String>,
    /// Variant axis value (e.g. `cpu`/`gpu`), if any.
    #[serde(default)]
    pub variant: Option<String>,
    /// Prefix to install into (defaults to `.venv`).
    #[serde(default)]
    pub prefix: Option<PathBuf>,
}

impl ProjectRef {
    /// The version label, defaulting to `latest`.
    pub fn label(&self) -> Label {
        Label::parse(self.version.as_deref().unwrap_or(DEFAULT_LABEL))
    }

    /// The install prefix, defaulting to `.venv`.
    pub fn prefix(&self) -> PathBuf {
        self.prefix
            .clone()
            .unwrap_or_else(|| PathBuf::from(DEFAULT_PREFIX))
    }

    /// The registry coordinates, defaulting the platform to the current one.
    pub fn coordinates(&self) -> Coordinates {
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

    fn registry(&self) -> Registry {
        Registry::new(SpecStore::new(), self.registry.clone())
    }
}

/// A parsed `pyproject.toml`: the `[tool.nepenthe]` reference plus the project's
/// declared dependencies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectFile {
    /// The consumed-environment reference.
    pub nepenthe: ProjectRef,
    /// The project's declared dependencies (`[project.dependencies]`), verbatim.
    pub dependencies: Vec<String>,
    /// The directory containing the `pyproject.toml`, used to resolve a relative
    /// install prefix.
    pub root: PathBuf,
}

impl ProjectFile {
    /// The install prefix resolved against the project root. An absolute prefix
    /// is used unchanged; a relative one is taken relative to the directory
    /// holding the `pyproject.toml`, not the caller's working directory.
    pub fn resolved_prefix(&self) -> PathBuf {
        let prefix = self.nepenthe.prefix();
        if prefix.is_absolute() {
            prefix
        } else {
            self.root.join(prefix)
        }
    }
}

#[derive(Deserialize)]
struct RawPyProject {
    project: Option<RawProject>,
    tool: Option<RawTool>,
}

#[derive(Deserialize)]
struct RawProject {
    #[serde(default)]
    dependencies: Vec<String>,
}

#[derive(Deserialize)]
struct RawTool {
    nepenthe: Option<ProjectRef>,
}

/// Read a `pyproject.toml`, returning its `[tool.nepenthe]` reference and
/// `[project.dependencies]`. Fails if the file has no `[tool.nepenthe]` stanza.
pub fn read(pyproject: &Path) -> Result<ProjectFile, ProjectError> {
    let text = std::fs::read_to_string(pyproject)?;
    let raw: RawPyProject = toml::from_str(&text).map_err(|e| ProjectError::Toml(e.to_string()))?;
    let nepenthe = raw
        .tool
        .and_then(|t| t.nepenthe)
        .ok_or(ProjectError::Missing)?;
    let dependencies = raw.project.map(|p| p.dependencies).unwrap_or_default();
    let root = pyproject
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    Ok(ProjectFile {
        nepenthe,
        dependencies,
        root,
    })
}

/// Read just `[project.dependencies]` from a `pyproject.toml`, without requiring
/// a `[tool.nepenthe]` stanza. Used by `nepenthe try` to inject a project's
/// declared dependencies into a trial solve.
pub fn read_dependencies(pyproject: &Path) -> Result<Vec<String>, ProjectError> {
    let text = std::fs::read_to_string(pyproject)?;
    let raw: RawPyProject = toml::from_str(&text).map_err(|e| ProjectError::Toml(e.to_string()))?;
    Ok(raw.project.map(|p| p.dependencies).unwrap_or_default())
}

/// Install (or update) the environment referenced by a project into its prefix,
/// resolving the version label against the registry. Performs network I/O; await
/// inside a tokio runtime.
pub async fn sync(
    project: &ProjectFile,
    link_scripts: LinkScripts,
) -> Result<InstallSummary, ProjectError> {
    let reference = &project.nepenthe;
    let summary = install::create(
        &reference.registry(),
        &reference.coordinates(),
        &reference.label(),
        &project.resolved_prefix(),
        link_scripts,
    )
    .await?;
    Ok(summary)
}

/// How a single declared dependency relates to the environment's package set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DependencyStatus {
    /// The environment pins a version that satisfies the requirement.
    Satisfied {
        /// The package name (normalized).
        name: String,
        /// The version the environment pins.
        found: String,
    },
    /// The environment pins the package, but at a version that does not satisfy.
    Conflict {
        /// The package name (normalized).
        name: String,
        /// The version specifier the project requires.
        specifier: String,
        /// The version the environment pins.
        found: String,
    },
    /// The environment has no package with this (normalized) name.
    Missing {
        /// The package name (normalized).
        name: String,
    },
    /// The requirement could not be parsed (e.g. a direct URL reference) or its
    /// specifier was not understood; it was skipped.
    Skipped {
        /// The reason the entry was skipped.
        reason: String,
    },
}

/// One checked dependency: the original requirement string and its status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedDependency {
    /// The requirement exactly as declared in `[project.dependencies]`.
    pub requirement: String,
    /// What the check found.
    pub status: DependencyStatus,
}

/// The result of checking a project's dependencies against an environment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CheckReport {
    /// One entry per declared dependency, in declaration order.
    pub dependencies: Vec<CheckedDependency>,
}

impl CheckReport {
    /// The number of dependencies the environment satisfies.
    pub fn satisfied(&self) -> usize {
        self.count(|s| matches!(s, DependencyStatus::Satisfied { .. }))
    }

    /// The number of dependencies that conflict with the environment.
    pub fn conflicts(&self) -> usize {
        self.count(|s| matches!(s, DependencyStatus::Conflict { .. }))
    }

    /// The number of dependencies absent from the environment.
    pub fn missing(&self) -> usize {
        self.count(|s| matches!(s, DependencyStatus::Missing { .. }))
    }

    /// The number of dependencies that could not be checked.
    pub fn skipped(&self) -> usize {
        self.count(|s| matches!(s, DependencyStatus::Skipped { .. }))
    }

    /// Whether any dependency conflicts with the environment.
    pub fn has_conflicts(&self) -> bool {
        self.conflicts() > 0
    }

    fn count(&self, predicate: impl Fn(&DependencyStatus) -> bool) -> usize {
        self.dependencies
            .iter()
            .filter(|d| predicate(&d.status))
            .count()
    }
}

/// Check declared `dependencies` (PEP 508 strings) against an environment's
/// `packages`. Pure and offline: each requirement is matched by normalized name
/// and its version specifier tested against the pinned version.
///
/// Names are matched after PEP 503 normalization against conda package names.
/// When a PyPI name differs from its conda counterpart (e.g. `opencv-python` vs
/// `opencv`), the [grayskull-derived mapping](crate::name_map) is consulted so
/// the dependency still resolves; only names absent under both spellings report
/// as [`Missing`](DependencyStatus::Missing).
pub fn check_dependencies(dependencies: &[String], packages: &[PackageId]) -> CheckReport {
    let by_name: std::collections::BTreeMap<String, &PackageId> = packages
        .iter()
        .map(|p| (normalize_name(&p.name), p))
        .collect();

    let mut checked = Vec::with_capacity(dependencies.len());
    for requirement in dependencies {
        let status = match parse_requirement(requirement) {
            None => DependencyStatus::Skipped {
                reason: "not a name+version requirement (direct URL or unparseable)".to_string(),
            },
            Some((name, specifier)) => match resolve_package(&name, &by_name) {
                None => DependencyStatus::Missing { name },
                Some(package) => check_version(name, &specifier, &package.version),
            },
        };
        checked.push(CheckedDependency {
            requirement: requirement.clone(),
            status,
        });
    }
    CheckReport {
        dependencies: checked,
    }
}

/// Resolve a normalized requirement name to an environment package: first by a
/// direct name match, then via the PyPI→conda [name mapping](crate::name_map).
fn resolve_package<'a>(
    name: &str,
    by_name: &std::collections::BTreeMap<String, &'a PackageId>,
) -> Option<&'a PackageId> {
    if let Some(package) = by_name.get(name) {
        return Some(package);
    }
    let conda = name_map::pypi_to_conda(name)?;
    by_name.get(&normalize_name(conda)).copied()
}

/// Pull the environment's lock from the registry and check the project's
/// dependencies against its pinned package set. `platform` overrides the
/// project's (or current) platform. Performs network I/O; await inside a tokio
/// runtime.
pub async fn check(
    project: &ProjectFile,
    platform: Option<&str>,
) -> Result<CheckReport, ProjectError> {
    let reference = &project.nepenthe;
    let mut coords = reference.coordinates();
    if let Some(platform) = platform {
        coords.platform = platform.to_string();
    }
    let registry = reference.registry();
    let bytes = registry.pull(&coords, &reference.label())?;
    let lock = install::parse_lock(&bytes)?;
    let packages = install::lock_packages(&lock, &reference.environment, &coords.platform)?;
    Ok(check_dependencies(&project.dependencies, &packages))
}

/// Test a pinned version against a requirement's specifier.
fn check_version(name: String, specifier: &str, found: &str) -> DependencyStatus {
    if specifier.is_empty() {
        return DependencyStatus::Satisfied {
            name,
            found: found.to_string(),
        };
    }
    let version = match Version::from_str(found) {
        Ok(v) => v,
        Err(_) => {
            return DependencyStatus::Skipped {
                reason: format!("environment pins an unparsable version '{found}' for {name}"),
            };
        }
    };
    match VersionSpec::from_str(specifier, ParseStrictness::Lenient) {
        Err(_) => DependencyStatus::Skipped {
            reason: format!("could not parse version specifier '{specifier}' for {name}"),
        },
        Ok(spec) if spec.matches(&version) => DependencyStatus::Satisfied {
            name,
            found: found.to_string(),
        },
        Ok(_) => DependencyStatus::Conflict {
            name,
            specifier: specifier.to_string(),
            found: found.to_string(),
        },
    }
}

/// Extract a `(normalized name, version specifier)` from a PEP 508 requirement.
/// Returns `None` for direct-reference (`name @ url`) or unparseable entries.
/// Environment markers (`; python_version …`) and extras (`[extra]`) are dropped.
fn parse_requirement(requirement: &str) -> Option<(String, String)> {
    // Drop any environment marker.
    let base = requirement.split(';').next().unwrap_or("").trim();
    if base.is_empty() {
        return None;
    }
    // Direct URL references (`name @ https://…`) are not version requirements.
    if base.contains('@') {
        return None;
    }
    // The name is the leading run of PEP 508 name characters.
    let name_end = base
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-'))
        .unwrap_or(base.len());
    let name = &base[..name_end];
    if name.is_empty() {
        return None;
    }
    let rest = base[name_end..].trim();
    // Drop an optional extras group (`[extra1,extra2]`) before the specifier.
    let specifier = if let Some(after_open) = rest.strip_prefix('[') {
        match after_open.find(']') {
            Some(close) => after_open[close + 1..].to_string(),
            None => return None,
        }
    } else {
        rest.to_string()
    };
    // Conda version specs carry no internal whitespace.
    let specifier: String = specifier.split_whitespace().collect();
    Some((normalize_name(name), specifier))
}

/// Convert a project's PEP 508 `[project.dependencies]` into conda match-specs
/// suitable for a trial solve. Each requirement's name is mapped PyPI→conda via
/// the [name map](crate::name_map) (so `opencv-python` becomes `opencv`), and
/// its version specifier is reused (conda and PEP 440 share the common
/// comparison operators). Direct-URL / unparseable entries are skipped.
pub fn requirements_to_conda_specs(dependencies: &[String]) -> Vec<String> {
    let mut specs = Vec::new();
    for requirement in dependencies {
        let Some((name, specifier)) = parse_requirement(requirement) else {
            continue;
        };
        let conda = name_map::pypi_to_conda(&name)
            .map(str::to_string)
            .unwrap_or(name);
        if specifier.is_empty() {
            specs.push(conda);
        } else {
            specs.push(format!("{conda} {specifier}"));
        }
    }
    specs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkg(name: &str, version: &str) -> PackageId {
        PackageId {
            name: name.to_string(),
            version: version.to_string(),
            build: "h0".to_string(),
        }
    }

    #[test]
    fn parses_tool_nepenthe_stanza() {
        let text = r#"
[project]
name = "demo"
dependencies = ["numpy>=2", "requests"]

[tool.nepenthe]
environment = "myenv"
registry = "file:///srv/nepenthe"
version = "1.3.0"
python = "3.11"
"#;
        let dir = std::env::temp_dir().join(format!("nepenthe-proj-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pyproject.toml");
        std::fs::write(&path, text).unwrap();

        let project = read(&path).unwrap();
        assert_eq!(project.nepenthe.environment, "myenv");
        assert_eq!(project.nepenthe.python.as_deref(), Some("3.11"));
        assert_eq!(project.nepenthe.prefix(), PathBuf::from(".venv"));
        // a relative prefix resolves beside the pyproject.toml, not the CWD
        assert_eq!(project.resolved_prefix(), dir.join(".venv"));
        assert!(matches!(project.nepenthe.label(), Label::Exact(v) if v == "1.3.0"));
        assert_eq!(project.dependencies, vec!["numpy>=2", "requests"]);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_stanza_is_reported() {
        let dir = std::env::temp_dir().join(format!("nepenthe-proj-none-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pyproject.toml");
        std::fs::write(&path, "[project]\nname = \"demo\"\n").unwrap();
        assert!(matches!(read(&path), Err(ProjectError::Missing)));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parse_requirement_extracts_name_and_specifier() {
        assert_eq!(
            parse_requirement("numpy>=2.2"),
            Some(("numpy".to_string(), ">=2.2".to_string()))
        );
        assert_eq!(
            parse_requirement("requests"),
            Some(("requests".to_string(), String::new()))
        );
        assert_eq!(
            parse_requirement("Flask-SQLAlchemy >= 3, <4"),
            Some(("flask-sqlalchemy".to_string(), ">=3,<4".to_string()))
        );
        assert_eq!(
            parse_requirement("ruff[extra]==0.6.0"),
            Some(("ruff".to_string(), "==0.6.0".to_string()))
        );
        assert_eq!(
            parse_requirement("pandas==1.5.0; python_version < '3.12'"),
            Some(("pandas".to_string(), "==1.5.0".to_string()))
        );
        // Direct URL references are skipped.
        assert_eq!(parse_requirement("foo @ https://example.com/foo.whl"), None);
    }

    #[test]
    fn check_reports_satisfied_conflict_missing_and_skipped() {
        let packages = vec![pkg("numpy", "2.1.0"), pkg("requests", "2.32.0")];
        let deps = vec![
            "numpy>=2".to_string(),            // satisfied
            "requests".to_string(),            // satisfied (no specifier)
            "numpy<2".to_string(),             // conflict
            "scipy>=1.10".to_string(),         // missing
            "foo @ https://x/foo".to_string(), // skipped
        ];
        let report = check_dependencies(&deps, &packages);
        assert_eq!(report.satisfied(), 2);
        assert_eq!(report.conflicts(), 1);
        assert_eq!(report.missing(), 1);
        assert_eq!(report.skipped(), 1);
        assert!(report.has_conflicts());

        assert!(matches!(
            &report.dependencies[2].status,
            DependencyStatus::Conflict { name, found, .. } if name == "numpy" && found == "2.1.0"
        ));
        assert!(matches!(
            &report.dependencies[3].status,
            DependencyStatus::Missing { name } if name == "scipy"
        ));
    }

    #[test]
    fn check_matches_pypi_names_to_conda_packages() {
        // PEP 503 normalization lets `Ruamel.YAML` match a conda `ruamel-yaml`.
        let packages = vec![pkg("ruamel-yaml", "0.18.6")];
        let report = check_dependencies(&["Ruamel.YAML>=0.18".to_string()], &packages);
        assert_eq!(report.satisfied(), 1);
    }

    #[test]
    fn check_uses_grayskull_mapping_for_divergent_names() {
        // The PyPI name `opencv-python` maps to the conda package `opencv` via
        // the vendored grayskull table; without it this would report missing.
        let packages = vec![pkg("opencv", "4.10.0")];
        let report = check_dependencies(&["opencv-python>=4".to_string()], &packages);
        assert_eq!(report.satisfied(), 1, "{:?}", report.dependencies);
        assert_eq!(report.missing(), 0);
    }

    #[test]
    fn requirements_convert_to_conda_match_specs() {
        let deps = vec![
            "numpy>=2".to_string(),                          // versioned
            "requests".to_string(),                          // bare name
            "opencv-python<5".to_string(),                   // pypi→conda mapped
            "torch @ https://example.com/torch".to_string(), // url ref → skipped
        ];
        let specs = requirements_to_conda_specs(&deps);
        assert_eq!(
            specs,
            vec![
                "numpy >=2".to_string(),
                "requests".to_string(),
                "opencv <5".to_string(),
            ]
        );
    }
}
