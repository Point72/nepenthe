//! Environment manifest model, composition, and validation lints.
//!
//! The manifest is nepenthe's YAML "producer" specification: it declares
//! channels, a base dependency set, composable **features**, and named
//! **environments** that select features.
//!
//! This module covers the manifest: the serde model, YAML loading with
//! cross-file `imports` (so a large environment set can be split across files,
//! and later arbitrary backends), environment composition (base ∪ selected
//! features, conda vs pypi kept separate), build **variants** (`cpu`/`gpu`
//! flavors an environment selects, carrying their own deps + solver
//! constraints), environment **inheritance** (`extends` + per-environment
//! `platforms`/`variant`), and validation lints. Override *layers*
//! (which can supply variant constraints externally) are modeled by
//! [`Overrides`] and applied with [`Manifest::apply`].

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Default ceiling on hard (`==`) pins per resolved environment.
pub const DEFAULT_MAX_HARD_PINS: usize = 10;

/// A full environment manifest (the YAML producer specification).
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Project-level metadata. Optional so a fragment file (pulled in via
    /// [`Manifest::imports`]) can contribute just features or variants.
    #[serde(default, skip_serializing_if = "Project::is_empty")]
    pub project: Project,
    /// Other manifest files to merge in before this one's own definitions.
    /// Each entry is resolved by [`Manifest::load`] relative to this file (a
    /// local path today; arbitrary spec backends later). Lets a large
    /// environment set be split across many files.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub imports: Vec<String>,
    /// Channel definitions, keyed by the names referenced in
    /// [`Project::channels`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub channels: BTreeMap<String, Channel>,
    /// Base conda dependencies, injected into every environment. Each entry is
    /// a conda match-spec string (e.g. `python >=3.11`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dependencies: Vec<String>,
    /// Base PyPI dependencies, injected into every environment.
    #[serde(
        default,
        rename = "pypi-dependencies",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub pypi_dependencies: Vec<String>,
    /// Base activation hooks, applied to every environment. Materialized into
    /// the prefix's `etc/conda/activate.d/` on install so a full activation
    /// runs them.
    #[serde(default, skip_serializing_if = "Activation::is_empty")]
    pub activation: Activation,
    /// Build variants (e.g. `cpu`, `gpu`): a build flavor selected by an
    /// environment. Each bundles its own dependencies, solver constraints, and
    /// virtual-package overrides.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub variants: BTreeMap<String, Variant>,
    /// Composable feature groups, keyed by name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub features: BTreeMap<String, Feature>,
    /// Named environments, each composing features (optionally inheriting from
    /// another environment via `extends`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub environments: BTreeMap<String, EnvironmentSpec>,
    /// Global virtual-package assumptions for the solve (e.g. `cuda: "12.9"`,
    /// `archspec: skylake_avx512`). Normally empty in a hand-written manifest;
    /// populated by applying an override layer (see [`Manifest::apply`]).
    #[serde(
        default,
        rename = "virtual-packages",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub virtual_packages: BTreeMap<String, String>,
    /// Matrix exclusions (denylist), keyed by Python version → the environments
    /// **not** built for it. Normally empty in a hand-written manifest;
    /// populated by applying an override layer. Consulted by
    /// [`Manifest::targets`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub exclude: BTreeMap<String, Vec<String>>,
    /// Matrix inclusions (allowlist), keyed by Python version → the **only**
    /// environments built for it; any environment not listed is excluded. Use
    /// when fewer environments are built for a Python than would be excluded.
    /// An environment named in both `include` and `exclude` is excluded.
    /// Consulted by [`Manifest::targets`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub include: BTreeMap<String, Vec<String>>,
}

/// Project-level metadata.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Project {
    /// Human-readable project name.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// Channel names (keys into [`Manifest::channels`]), in priority order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub channels: Vec<String>,
    /// Base URL that bare channel names resolve against (e.g. an internal
    /// Artifactory conda base). A channel listed in [`Self::channels`] without
    /// its own `url`/`mirror` is fetched from `<channel-alias>/<name>`. Mirrors
    /// conda's `.condarc` `channel_alias`.
    #[serde(
        default,
        rename = "channel-alias",
        skip_serializing_if = "Option::is_none"
    )]
    pub channel_alias: Option<String>,
    /// Target platforms (e.g. `linux-64`, `osx-arm64`, `win-64`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub platforms: Vec<String>,
    /// The default Python axis for every environment: each value produces one
    /// build target (e.g. `["3.11", "3.12", "3.13"]`). An environment may
    /// narrow this. Injected into the solve as `python <ver>.*`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub python: Vec<String>,
    /// The Python version chosen when a selection omits one. Must be a member
    /// of [`Project::python`].
    #[serde(
        default,
        rename = "default-python",
        skip_serializing_if = "Option::is_none"
    )]
    pub default_python: Option<String>,
}

impl Project {
    /// Whether this project block carries no information (used to omit it from
    /// serialized fragments).
    fn is_empty(&self) -> bool {
        self.name.is_empty()
            && self.channels.is_empty()
            && self.channel_alias.is_none()
            && self.platforms.is_empty()
            && self.python.is_empty()
            && self.default_python.is_none()
    }
}

/// A package-source channel definition. Optional: a channel named in
/// [`Project::channels`] needs an entry here only to override how it resolves
/// (an explicit `url`, a `mirror`, or a `priority`); a bare name with no entry
/// resolves against [`Project::channel_alias`].
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Channel {
    /// Explicit base URL for this channel, used as-is instead of resolving the
    /// name against the alias.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Fetch this channel from a different name/URL while keeping its public
    /// identity (e.g. solve `conda-forge` against an internal mirror). A bare
    /// mirror name is itself resolved against [`Project::channel_alias`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mirror: Option<String>,
    /// Optional explicit priority (higher wins). Currently recorded but not yet
    /// consumed by the solver, which orders channels by their position in
    /// [`Project::channels`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<i64>,
}

/// A composable group of dependencies.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Feature {
    /// Conda dependencies contributed by this feature.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dependencies: Vec<String>,
    /// PyPI dependencies contributed by this feature.
    #[serde(
        default,
        rename = "pypi-dependencies",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub pypi_dependencies: Vec<String>,
    /// Activation hooks contributed by this feature.
    #[serde(default, skip_serializing_if = "Activation::is_empty")]
    pub activation: Activation,
}

/// A build variant: a build flavor (e.g. `cpu` vs `gpu`) that an environment
/// selects. It bundles variant-specific dependencies, solver constraints that
/// bound the solve without adding a dependency, and virtual-package overrides.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Variant {
    /// Conda dependencies pulled in when this variant is selected (e.g. `cuda`
    /// for `gpu`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dependencies: Vec<String>,
    /// PyPI dependencies pulled in when this variant is selected.
    #[serde(
        default,
        rename = "pypi-dependencies",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub pypi_dependencies: Vec<String>,
    /// Match-spec constraints applied to the solve.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub constraints: Vec<String>,
    /// Virtual-package overrides (e.g. `cuda: "12.9"`).
    #[serde(
        default,
        rename = "virtual-packages",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub virtual_packages: BTreeMap<String, String>,
    /// Activation hooks pulled in when this variant is selected.
    #[serde(default, skip_serializing_if = "Activation::is_empty")]
    pub activation: Activation,
}

/// Activation hooks: environment variables and shell snippets run when an
/// environment is **activated** (not merely placed on `PATH`). Materialized
/// into the prefix's `etc/conda/activate.d/` on install, so they run via a full
/// [`crate::install::activation_script`] / `conda activate`. Mergeable across
/// the base manifest, features, and the selected variant.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Activation {
    /// Environment variables to export on activation, name → value.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Raw shell snippets run on activation, after the env vars are set.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scripts: Vec<String>,
}

impl Activation {
    /// Whether this block declares no hooks.
    pub fn is_empty(&self) -> bool {
        self.env.is_empty() && self.scripts.is_empty()
    }

    /// Merge `other` into `self`: env vars from `other` win on a key clash, and
    /// its scripts are appended (run after this block's).
    fn merge(&mut self, other: &Activation) {
        for (k, v) in &other.env {
            self.env.insert(k.clone(), v.clone());
        }
        self.scripts.extend(other.scripts.iter().cloned());
    }
}

/// An **override layer**: a separate, pullable specification that adjusts a
/// manifest's solve without editing it.
/// Layered onto a manifest with [`Manifest::apply`].
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Overrides {
    /// Global virtual-package assumptions for the solve (replaces
    /// `CONDA_OVERRIDE_CUDA` / `CONDA_OVERRIDE_ARCHSPEC`).
    #[serde(
        default,
        rename = "virtual-packages",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub virtual_packages: BTreeMap<String, String>,
    /// Version pins, keyed by package name → version spec (replaces
    /// `REQ_OVERRIDES`). When applied, any matching conda dependency has its
    /// version replaced with the pin (the override wins).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub pins: BTreeMap<String, String>,
    /// Per-variant additions, keyed by variant name. Constraints, deps, and
    /// virtual-packages are merged into the manifest's variant (replaces
    /// `CONSTRAINTS_CPU` / `CONSTRAINTS_GPU`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub variants: BTreeMap<String, Variant>,
    /// Matrix exclusions (denylist), keyed by Python version → the environments
    /// not built for it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub exclude: BTreeMap<String, Vec<String>>,
    /// Matrix inclusions (allowlist), keyed by Python version → the only
    /// environments built for it. Shorthand for when fewer environments are
    /// built for a Python than would be excluded.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub include: BTreeMap<String, Vec<String>>,
}

impl Overrides {
    /// Parse an override layer from a YAML string.
    pub fn from_yaml_str(yaml: &str) -> Result<Self, ManifestError> {
        Ok(serde_yaml::from_str(yaml)?)
    }

    /// Read and parse an override layer from a YAML file.
    pub fn from_yaml_path(path: impl AsRef<Path>) -> Result<Self, ManifestError> {
        let text = std::fs::read_to_string(path)?;
        Self::from_yaml_str(&text)
    }

    /// Serialize the override layer to YAML.
    pub fn to_yaml_string(&self) -> Result<String, ManifestError> {
        Ok(serde_yaml::to_string(self)?)
    }
}

/// An environment definition: either a bare list of feature names, or a
/// detailed form that can derive from another environment and target specific
/// platforms.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum EnvironmentSpec {
    /// Shorthand: just the feature names to compose.
    Features(Vec<String>),
    /// Detailed form with inheritance and platform targeting.
    Detailed(EnvironmentDef),
}

impl EnvironmentSpec {
    /// The directly-listed feature names (excluding any inherited via
    /// `extends`).
    pub fn features(&self) -> &[String] {
        match self {
            EnvironmentSpec::Features(f) => f,
            EnvironmentSpec::Detailed(d) => &d.features,
        }
    }

    /// The environment this one derives from, if any.
    pub fn extends(&self) -> Option<&str> {
        match self {
            EnvironmentSpec::Features(_) => None,
            EnvironmentSpec::Detailed(d) => d.extends.as_deref(),
        }
    }

    /// Platform overrides declared directly on this environment.
    pub fn platforms(&self) -> &[String] {
        match self {
            EnvironmentSpec::Features(_) => &[],
            EnvironmentSpec::Detailed(d) => &d.platforms,
        }
    }

    /// Extra channels declared directly on this environment.
    pub fn channels(&self) -> &[String] {
        match self {
            EnvironmentSpec::Features(_) => &[],
            EnvironmentSpec::Detailed(d) => &d.channels,
        }
    }

    /// The build-variant axis declared directly on this environment: the
    /// plural `variants` if present, else the singular `variant` as a
    /// one-element axis, else empty.
    pub fn variant_axis(&self) -> Vec<String> {
        match self {
            EnvironmentSpec::Features(_) => Vec::new(),
            EnvironmentSpec::Detailed(d) => {
                if !d.variants.is_empty() {
                    d.variants.clone()
                } else if let Some(v) = &d.variant {
                    vec![v.clone()]
                } else {
                    Vec::new()
                }
            }
        }
    }

    /// The default variant declared directly on this environment, if any.
    pub fn default_variant(&self) -> Option<&str> {
        match self {
            EnvironmentSpec::Features(_) => None,
            EnvironmentSpec::Detailed(d) => d.default_variant.as_deref().or(d.variant.as_deref()),
        }
    }

    /// The Python axis declared directly on this environment, if any.
    pub fn python_axis(&self) -> &[String] {
        match self {
            EnvironmentSpec::Features(_) => &[],
            EnvironmentSpec::Detailed(d) => &d.python,
        }
    }

    /// The default Python declared directly on this environment, if any.
    pub fn default_python(&self) -> Option<&str> {
        match self {
            EnvironmentSpec::Features(_) => None,
            EnvironmentSpec::Detailed(d) => d.default_python.as_deref(),
        }
    }
}

/// The detailed environment form: compose features, optionally derive from
/// another environment, select build variant(s), choose Python version(s), and
/// target specific platforms. `variants` and `python` are **matrix axes**: an
/// environment with `variants: [cpu, gpu]` and `python: [3.11, 3.12]` produces
/// four build targets, one per `(variant, python)` cell.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentDef {
    /// Features to compose, in addition to any inherited via `extends`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<String>,
    /// A single build variant — shorthand for a one-element [`Self::variants`].
    /// Use this for environments that have exactly one flavor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    /// The build-variant axis (keys into [`Manifest::variants`]): each value
    /// produces a build target. With more than one, set [`Self::default_variant`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub variants: Vec<String>,
    /// The variant chosen when a selection omits one. Must be a member of the
    /// variant axis. Optional when the axis has a single value.
    #[serde(
        default,
        rename = "default-variant",
        skip_serializing_if = "Option::is_none"
    )]
    pub default_variant: Option<String>,
    /// Per-environment Python axis, overriding [`Project::python`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub python: Vec<String>,
    /// Per-environment default Python, overriding [`Project::default_python`].
    #[serde(
        default,
        rename = "default-python",
        skip_serializing_if = "Option::is_none"
    )]
    pub default_python: Option<String>,
    /// Another environment to derive from: its composed feature set and axes
    /// are inherited, and its platforms apply unless overridden here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extends: Option<String>,
    /// Platforms this environment targets, overriding the project defaults and
    /// any inherited from `extends`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub platforms: Vec<String>,
    /// Extra channels this environment solves against, in addition to
    /// [`Project::channels`] — e.g. a private channel only this environment
    /// needs. Listed by name (resolved via [`Project::channel_alias`]); any
    /// credentials are supplied out of band, never here. Unioned across the
    /// `extends` chain.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub channels: Vec<String>,
}

/// The composed dependency set for one environment.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResolvedEnvironment {
    /// Environment name.
    pub name: String,
    /// Conda dependencies (base ∪ features ∪ selected variant), deduped and
    /// sorted.
    pub dependencies: Vec<String>,
    /// PyPI dependencies (base ∪ features ∪ selected variant), deduped and
    /// sorted.
    pub pypi_dependencies: Vec<String>,
    /// Variant constraints applied to the solve (bound it without adding deps).
    pub constraints: Vec<String>,
    /// Virtual-package assumptions contributed by the selected variant (e.g.
    /// `cuda: "12.9"` for a `gpu` variant). Merged with the manifest's global
    /// virtual packages when building a [`crate::solve::SolveRequest`].
    pub virtual_packages: BTreeMap<String, String>,
    /// The selected build variant, if any.
    pub variant: Option<String>,
    /// The selected Python version, if the environment has a Python axis.
    pub python: Option<String>,
    /// Effective target platforms (env override, inherited, or project
    /// default).
    pub platforms: Vec<String>,
    /// Extra channels this environment solves against, in addition to the
    /// project channels (unioned across the `extends` chain). Listed by name;
    /// credentials, if any, are supplied out of band.
    pub channels: Vec<String>,
    /// Activation hooks (base ∪ features ∪ selected variant), materialized into
    /// the prefix on install.
    pub activation: Activation,
}

/// A selection of one cell from an environment's build matrix. Omitted axes
/// fall back to the environment's (or project's) declared defaults. This is
/// what a CLI passes through from `--variant` / `--python` flags.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Selector {
    /// The chosen build variant, or `None` to use the default.
    pub variant: Option<String>,
    /// The chosen Python version, or `None` to use the default.
    pub python: Option<String>,
}

impl Selector {
    /// A selector that chooses a specific variant.
    pub fn variant(variant: impl Into<String>) -> Self {
        Selector {
            variant: Some(variant.into()),
            python: None,
        }
    }

    /// Set the Python version on this selector.
    pub fn with_python(mut self, python: impl Into<String>) -> Self {
        self.python = Some(python.into());
        self
    }

    /// Whether this concrete matrix cell satisfies a `filter`: every axis the
    /// filter pins (`Some`) must match exactly; axes the filter leaves `None`
    /// match any value. Used to narrow an environment's targets to a single
    /// requested cell (e.g. one `--python`).
    pub fn matches(&self, filter: &Selector) -> bool {
        filter
            .variant
            .as_deref()
            .is_none_or(|v| self.variant.as_deref() == Some(v))
            && filter
                .python
                .as_deref()
                .is_none_or(|p| self.python.as_deref() == Some(p))
    }
}

/// Errors raised while loading or composing a manifest.
#[derive(Debug)]
pub enum ManifestError {
    /// Failed to read a manifest file.
    Io(std::io::Error),
    /// Failed to parse the manifest YAML.
    Parse(serde_yaml::Error),
    /// An environment was requested that the manifest does not define.
    UnknownEnvironment(String),
    /// An environment references a feature the manifest does not define.
    UnknownFeature {
        /// The environment doing the referencing.
        environment: String,
        /// The undefined feature name.
        feature: String,
    },
    /// A feature selects a variant the manifest does not define.
    UnknownVariant {
        /// The environment whose selected variant is undefined.
        environment: String,
        /// The undefined variant name.
        variant: String,
    },
    /// An environment `extends` another that the manifest does not define.
    UnknownBaseEnvironment {
        /// The deriving environment.
        environment: String,
        /// The undefined base environment.
        base: String,
    },
    /// An `extends` chain forms a cycle.
    EnvironmentCycle(String),
    /// An `imports` chain forms a cycle.
    ImportCycle(String),
    /// An `imports` entry escapes the manifest tree (absolute or `..`).
    UnsafeImport(String),
    /// A selection named a variant outside the environment's variant axis.
    VariantNotInAxis {
        /// The environment selected.
        environment: String,
        /// The requested variant.
        variant: String,
    },
    /// A selection omitted the variant for a multi-variant environment that
    /// declares no default.
    AmbiguousVariant {
        /// The environment selected.
        environment: String,
    },
    /// A selection named a Python outside the environment's Python axis.
    PythonNotInAxis {
        /// The environment selected.
        environment: String,
        /// The requested Python version.
        python: String,
    },
    /// A selection omitted the Python for a multi-Python environment that
    /// declares no default.
    AmbiguousPython {
        /// The environment selected.
        environment: String,
    },
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ManifestError::Io(e) => write!(f, "failed to read manifest: {e}"),
            ManifestError::Parse(e) => write!(f, "failed to parse manifest: {e}"),
            ManifestError::UnknownEnvironment(name) => {
                write!(f, "unknown environment: {name}")
            }
            ManifestError::UnknownFeature {
                environment,
                feature,
            } => write!(
                f,
                "environment '{environment}' references unknown feature '{feature}'"
            ),
            ManifestError::UnknownVariant {
                environment,
                variant,
            } => write!(
                f,
                "environment '{environment}' selects unknown variant '{variant}'"
            ),
            ManifestError::UnknownBaseEnvironment { environment, base } => write!(
                f,
                "environment '{environment}' extends unknown environment '{base}'"
            ),
            ManifestError::EnvironmentCycle(name) => {
                write!(f, "environment '{name}' is part of an extends cycle")
            }
            ManifestError::ImportCycle(path) => {
                write!(f, "import cycle detected at '{path}'")
            }
            ManifestError::UnsafeImport(import) => {
                write!(f, "unsafe import path '{import}' (absolute or '..' not allowed)")
            }
            ManifestError::VariantNotInAxis {
                environment,
                variant,
            } => write!(
                f,
                "environment '{environment}' has no variant '{variant}' in its axis"
            ),
            ManifestError::AmbiguousVariant { environment } => write!(
                f,
                "environment '{environment}' has multiple variants and no default; specify one"
            ),
            ManifestError::PythonNotInAxis {
                environment,
                python,
            } => write!(
                f,
                "environment '{environment}' has no Python '{python}' in its axis"
            ),
            ManifestError::AmbiguousPython { environment } => write!(
                f,
                "environment '{environment}' has multiple Python versions and no default; specify one"
            ),
        }
    }
}

impl std::error::Error for ManifestError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ManifestError::Io(e) => Some(e),
            ManifestError::Parse(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for ManifestError {
    fn from(e: std::io::Error) -> Self {
        ManifestError::Io(e)
    }
}

impl From<serde_yaml::Error> for ManifestError {
    fn from(e: serde_yaml::Error) -> Self {
        ManifestError::Parse(e)
    }
}

/// Where a lint was found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Location {
    /// The base (top-level) dependency set.
    Base,
    /// A named feature.
    Feature(String),
}

impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Location::Base => write!(f, "base"),
            Location::Feature(name) => write!(f, "feature '{name}'"),
        }
    }
}

/// How seriously to treat a [`Lint`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    /// Advisory; the manifest is still usable.
    Warning,
    /// A policy violation that should block.
    Error,
}

/// A validation finding produced by [`Manifest::lint`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Lint {
    /// A hard (`==`) pin was found. Discouraged: it invites dependency hell.
    HardPin {
        /// Where the pin lives.
        location: Location,
        /// The offending match-spec.
        spec: String,
    },
    /// An environment composes more hard pins than [`DEFAULT_MAX_HARD_PINS`].
    TooManyHardPins {
        /// The over-pinned environment.
        environment: String,
        /// Number of hard pins composed into the environment.
        count: usize,
        /// The configured ceiling.
        max: usize,
    },
    /// A feature pins a package also pinned in the base set, risking drift.
    BaseFeatureCollision {
        /// The feature colliding with base.
        feature: String,
        /// The package name present in both.
        package: String,
    },
}

impl Lint {
    /// The severity of this lint.
    pub fn severity(&self) -> Severity {
        match self {
            Lint::HardPin { .. } => Severity::Warning,
            Lint::TooManyHardPins { .. } => Severity::Error,
            Lint::BaseFeatureCollision { .. } => Severity::Warning,
        }
    }
}

impl fmt::Display for Lint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Lint::HardPin { location, spec } => {
                write!(f, "hard pin '{spec}' in {location}; avoid '==' if possible")
            }
            Lint::TooManyHardPins {
                environment,
                count,
                max,
            } => write!(
                f,
                "environment '{environment}' has {count} hard pins (max {max})"
            ),
            Lint::BaseFeatureCollision { feature, package } => write!(
                f,
                "feature '{feature}' redefines base package '{package}'; consider pushing it into base"
            ),
        }
    }
}

impl Manifest {
    /// Parse a manifest from a YAML string.
    pub fn from_yaml_str(yaml: &str) -> Result<Self, ManifestError> {
        Ok(serde_yaml::from_str(yaml)?)
    }

    /// Read and parse a manifest from a YAML file.
    pub fn from_yaml_path(path: impl AsRef<Path>) -> Result<Self, ManifestError> {
        let text = std::fs::read_to_string(path)?;
        Self::from_yaml_str(&text)
    }

    /// Serialize the manifest back to YAML.
    pub fn to_yaml_string(&self) -> Result<String, ManifestError> {
        Ok(serde_yaml::to_string(self)?)
    }

    /// Layer an [`Overrides`] onto this manifest in place, producing a
    /// self-contained **effective** manifest:
    ///
    /// - **variants** \u2014 the override's per-variant deps, constraints, and
    ///   virtual-packages are merged into the manifest's variants (filling,
    ///   e.g., an empty `cpu: {}`).
    /// - **pins** \u2014 baked into every conda dependency list (base, features,
    ///   variants): any spec whose package name is pinned has its version
    ///   replaced with the pin, so the result shows the pinned versions inline.
    /// - **virtual-packages** \u2014 the global solve assumptions are recorded on
    ///   the manifest.
    /// - **exclude / include** record the matrix denylist and allowlist, and
    ///   are thereafter honored by [`Manifest::targets`].
    pub fn apply(&mut self, overrides: &Overrides) {
        for (name, ov) in &overrides.variants {
            let variant = self.variants.entry(name.clone()).or_default();
            extend_dedup(&mut variant.dependencies, ov.dependencies.clone());
            extend_dedup(&mut variant.pypi_dependencies, ov.pypi_dependencies.clone());
            extend_dedup(&mut variant.constraints, ov.constraints.clone());
            for (k, v) in &ov.virtual_packages {
                variant.virtual_packages.insert(k.clone(), v.clone());
            }
        }

        if !overrides.pins.is_empty() {
            bake_pins(&mut self.dependencies, &overrides.pins);
            for feature in self.features.values_mut() {
                bake_pins(&mut feature.dependencies, &overrides.pins);
            }
            for variant in self.variants.values_mut() {
                bake_pins(&mut variant.dependencies, &overrides.pins);
            }
        }

        for (k, v) in &overrides.virtual_packages {
            self.virtual_packages.insert(k.clone(), v.clone());
        }
        for (python, envs) in &overrides.exclude {
            extend_dedup(
                self.exclude.entry(python.clone()).or_default(),
                envs.clone(),
            );
        }
        for (python, envs) in &overrides.include {
            extend_dedup(
                self.include.entry(python.clone()).or_default(),
                envs.clone(),
            );
        }
    }

    /// Iterate the defined environment names.
    pub fn environment_names(&self) -> impl Iterator<Item = &str> {
        self.environments.keys().map(String::as_str)
    }

    /// Resolve one **cell** of an environment's build matrix, selecting a
    /// variant and Python version (falling back to declared defaults for any
    /// axis the `selector` omits). Composition is base ∪ features (incl. those
    /// inherited via `extends`) ∪ the selected variant's deps, with the chosen
    /// Python injected as `python <ver>.*`. The variant also contributes solver
    /// constraints, and the effective platforms are resolved.
    pub fn resolve(
        &self,
        environment: &str,
        selector: &Selector,
    ) -> Result<ResolvedEnvironment, ManifestError> {
        let mut chain = Vec::new();
        let feature_names = self.collect_features(environment, &mut chain)?;

        let mut conda: BTreeSet<String> = self.dependencies.iter().cloned().collect();
        let mut pypi: BTreeSet<String> = self.pypi_dependencies.iter().cloned().collect();
        let mut constraints: BTreeSet<String> = BTreeSet::new();
        let mut activation = self.activation.clone();

        for feature_name in &feature_names {
            let feature =
                self.features
                    .get(feature_name)
                    .ok_or_else(|| ManifestError::UnknownFeature {
                        environment: environment.to_string(),
                        feature: feature_name.clone(),
                    })?;
            conda.extend(feature.dependencies.iter().cloned());
            pypi.extend(feature.pypi_dependencies.iter().cloned());
            activation.merge(&feature.activation);
        }

        let mut virtual_packages: BTreeMap<String, String> = BTreeMap::new();
        let variant_name = self.choose_variant(environment, selector)?;
        if let Some(variant_name) = &variant_name {
            let variant =
                self.variants
                    .get(variant_name)
                    .ok_or_else(|| ManifestError::UnknownVariant {
                        environment: environment.to_string(),
                        variant: variant_name.clone(),
                    })?;
            conda.extend(variant.dependencies.iter().cloned());
            pypi.extend(variant.pypi_dependencies.iter().cloned());
            constraints.extend(variant.constraints.iter().cloned());
            for (k, v) in &variant.virtual_packages {
                virtual_packages.insert(k.clone(), v.clone());
            }
            activation.merge(&variant.activation);
        }

        let python = self.choose_python(environment, selector)?;
        if let Some(py) = &python {
            conda.insert(format!("python {py}.*"));
        }

        // Extra channels declared on the environment or any it extends, in
        // chain order, deduped.
        let mut channels: Vec<String> = Vec::new();
        for env_name in &chain {
            if let Some(spec) = self.environments.get(env_name) {
                for channel in spec.channels() {
                    if !channels.contains(channel) {
                        channels.push(channel.clone());
                    }
                }
            }
        }

        Ok(ResolvedEnvironment {
            name: environment.to_string(),
            dependencies: conda.into_iter().collect(),
            pypi_dependencies: pypi.into_iter().collect(),
            constraints: constraints.into_iter().collect(),
            virtual_packages,
            variant: variant_name,
            python,
            platforms: self.resolved_platforms(environment),
            channels,
            activation,
        })
    }

    /// Resolve an environment's **default** build cell (all axes defaulted).
    /// Errors if an axis has multiple values and no declared default.
    pub fn resolve_default(&self, environment: &str) -> Result<ResolvedEnvironment, ManifestError> {
        self.resolve(environment, &Selector::default())
    }

    /// Enumerate every cell of an environment's build matrix: the Cartesian
    /// product of its variant axis and Python axis. Axes with no values
    /// contribute a single `None`, so an environment with neither axis yields
    /// one empty selector.
    pub fn targets(&self, environment: &str) -> Result<Vec<Selector>, ManifestError> {
        if !self.environments.contains_key(environment) {
            return Err(ManifestError::UnknownEnvironment(environment.to_string()));
        }
        // Validate the `extends` chain up front: the axis/platform resolvers
        // below walk `extends` with unbounded loops, so a cycle would spin
        // forever. `collect_features` reports `EnvironmentCycle` instead.
        self.collect_features(environment, &mut Vec::new())?;
        let variants = self.resolved_variant_axis(environment);
        let pythons = self.resolved_python_axis(environment);
        let v_axis: Vec<Option<String>> = if variants.is_empty() {
            vec![None]
        } else {
            variants.into_iter().map(Some).collect()
        };
        let p_axis: Vec<Option<String>> = if pythons.is_empty() {
            vec![None]
        } else {
            pythons.into_iter().map(Some).collect()
        };
        let mut out = Vec::new();
        for variant in &v_axis {
            for python in &p_axis {
                // Honor the include/exclude tables: an `include` allowlist for
                // a Python keeps only the named environments; an `exclude`
                // denylist drops the named ones (exclude wins on overlap).
                if let Some(py) = python {
                    if self
                        .include
                        .get(py)
                        .is_some_and(|envs| !envs.iter().any(|e| e == environment))
                    {
                        continue;
                    }
                    if self
                        .exclude
                        .get(py)
                        .is_some_and(|envs| envs.iter().any(|e| e == environment))
                    {
                        continue;
                    }
                }
                out.push(Selector {
                    variant: variant.clone(),
                    python: python.clone(),
                });
            }
        }
        Ok(out)
    }

    /// Like [`targets`](Self::targets) but narrowed to the cells matching
    /// `filter` (see [`Selector::matches`]). A filter with all axes `None`
    /// returns the full matrix; pinning an axis (e.g. a single `--python`)
    /// keeps only the cells with that value, so one build can be scoped to a
    /// single matrix cell instead of the whole environment.
    pub fn targets_filtered(
        &self,
        environment: &str,
        filter: &Selector,
    ) -> Result<Vec<Selector>, ManifestError> {
        Ok(self
            .targets(environment)?
            .into_iter()
            .filter(|t| t.matches(filter))
            .collect())
    }

    /// Pick the variant for a selection: the requested one (validated against
    /// the axis), else the declared default, else the sole axis value, else
    /// `None` (no variant axis) or an ambiguity error.
    fn choose_variant(
        &self,
        environment: &str,
        selector: &Selector,
    ) -> Result<Option<String>, ManifestError> {
        let axis = self.resolved_variant_axis(environment);
        if axis.is_empty() {
            return Ok(None);
        }
        match &selector.variant {
            Some(v) if axis.iter().any(|a| a == v) => Ok(Some(v.clone())),
            Some(v) => Err(ManifestError::VariantNotInAxis {
                environment: environment.to_string(),
                variant: v.clone(),
            }),
            None => {
                if let Some(d) = self.resolved_default_variant(environment) {
                    if axis.iter().any(|a| a == &d) {
                        Ok(Some(d))
                    } else {
                        Err(ManifestError::VariantNotInAxis {
                            environment: environment.to_string(),
                            variant: d,
                        })
                    }
                } else if axis.len() == 1 {
                    Ok(Some(axis[0].clone()))
                } else {
                    Err(ManifestError::AmbiguousVariant {
                        environment: environment.to_string(),
                    })
                }
            }
        }
    }

    /// Pick the Python for a selection, mirroring [`Self::choose_variant`].
    fn choose_python(
        &self,
        environment: &str,
        selector: &Selector,
    ) -> Result<Option<String>, ManifestError> {
        let axis = self.resolved_python_axis(environment);
        if axis.is_empty() {
            return Ok(None);
        }
        match &selector.python {
            Some(p) if axis.iter().any(|a| a == p) => Ok(Some(p.clone())),
            Some(p) => Err(ManifestError::PythonNotInAxis {
                environment: environment.to_string(),
                python: p.clone(),
            }),
            None => {
                if let Some(d) = self.resolved_default_python(environment) {
                    if axis.iter().any(|a| a == &d) {
                        Ok(Some(d))
                    } else {
                        Err(ManifestError::PythonNotInAxis {
                            environment: environment.to_string(),
                            python: d,
                        })
                    }
                } else if axis.len() == 1 {
                    Ok(Some(axis[0].clone()))
                } else {
                    Err(ManifestError::AmbiguousPython {
                        environment: environment.to_string(),
                    })
                }
            }
        }
    }

    /// Return the unique, sorted feature names an environment composes,
    /// including those inherited via `extends`.
    pub fn composed_features(&self, environment: &str) -> Result<Vec<String>, ManifestError> {
        let mut chain = Vec::new();
        let mut feats = self.collect_features(environment, &mut chain)?;
        feats.sort();
        feats.dedup();
        Ok(feats)
    }

    /// Walk the `extends` chain (base first, then the deriving environment)
    /// collecting feature names. `chain` tracks visited environments so a
    /// cycle is reported rather than looping forever.
    fn collect_features(
        &self,
        environment: &str,
        chain: &mut Vec<String>,
    ) -> Result<Vec<String>, ManifestError> {
        if chain.iter().any(|e| e == environment) {
            return Err(ManifestError::EnvironmentCycle(environment.to_string()));
        }
        let spec = self
            .environments
            .get(environment)
            .ok_or_else(|| ManifestError::UnknownEnvironment(environment.to_string()))?;
        chain.push(environment.to_string());

        let mut features = Vec::new();
        if let Some(base) = spec.extends() {
            if !self.environments.contains_key(base) {
                return Err(ManifestError::UnknownBaseEnvironment {
                    environment: environment.to_string(),
                    base: base.to_string(),
                });
            }
            features.extend(self.collect_features(base, chain)?);
        }
        features.extend(spec.features().iter().cloned());
        Ok(features)
    }

    /// Resolve an environment's platforms: its own override, else the nearest
    /// `extends` ancestor that sets platforms, else the project default. Safe
    /// against cycles because [`Manifest::resolve`] validates the chain first.
    fn resolved_platforms(&self, environment: &str) -> Vec<String> {
        let mut current = environment;
        while let Some(spec) = self.environments.get(current) {
            if !spec.platforms().is_empty() {
                return spec.platforms().to_vec();
            }
            match spec.extends() {
                Some(base) => current = base,
                None => break,
            }
        }
        self.project.platforms.clone()
    }

    /// Resolve an environment's variant axis: its own (plural `variants` or
    /// singular `variant`), else the nearest `extends` ancestor that sets one.
    fn resolved_variant_axis(&self, environment: &str) -> Vec<String> {
        let mut current = environment;
        while let Some(spec) = self.environments.get(current) {
            let axis = spec.variant_axis();
            if !axis.is_empty() {
                return axis;
            }
            match spec.extends() {
                Some(base) => current = base,
                None => break,
            }
        }
        Vec::new()
    }

    /// Resolve an environment's default variant from its `extends` chain.
    fn resolved_default_variant(&self, environment: &str) -> Option<String> {
        let mut current = environment;
        while let Some(spec) = self.environments.get(current) {
            if let Some(default) = spec.default_variant() {
                return Some(default.to_string());
            }
            match spec.extends() {
                Some(base) => current = base,
                None => break,
            }
        }
        None
    }

    /// Resolve an environment's Python axis: its own, else the nearest
    /// `extends` ancestor that sets one, else the project default axis.
    fn resolved_python_axis(&self, environment: &str) -> Vec<String> {
        let mut current = environment;
        while let Some(spec) = self.environments.get(current) {
            if !spec.python_axis().is_empty() {
                return spec.python_axis().to_vec();
            }
            match spec.extends() {
                Some(base) => current = base,
                None => break,
            }
        }
        self.project.python.clone()
    }

    /// Resolve an environment's default Python from its `extends` chain, else
    /// the project default.
    fn resolved_default_python(&self, environment: &str) -> Option<String> {
        let mut current = environment;
        while let Some(spec) = self.environments.get(current) {
            if let Some(default) = spec.default_python() {
                return Some(default.to_string());
            }
            match spec.extends() {
                Some(base) => current = base,
                None => break,
            }
        }
        self.project.default_python.clone()
    }

    /// Merge another manifest into this one, with `other` taking precedence:
    /// keyed maps (channels, variants, features, environments) gain `other`'s
    /// entries, overriding on key collisions; list fields concatenate then
    /// dedup; a non-empty `other.project` replaces this one. Used by
    /// [`Manifest::load`] to combine imported fragments.
    pub fn merge(&mut self, other: Manifest) {
        if !other.project.is_empty() {
            self.project = other.project;
        }
        self.channels.extend(other.channels);
        self.variants.extend(other.variants);
        self.features.extend(other.features);
        self.environments.extend(other.environments);
        self.virtual_packages.extend(other.virtual_packages);
        for (python, envs) in other.exclude {
            extend_dedup(self.exclude.entry(python).or_default(), envs);
        }
        for (python, envs) in other.include {
            extend_dedup(self.include.entry(python).or_default(), envs);
        }
        extend_dedup(&mut self.dependencies, other.dependencies);
        extend_dedup(&mut self.pypi_dependencies, other.pypi_dependencies);
        self.activation.merge(&other.activation);
    }

    /// Load a manifest from a YAML file, resolving and merging any `imports`
    /// (relative to each file) before this file's own definitions. Imports are
    /// local paths today; arbitrary spec backends will plug in here later. An
    /// import cycle is reported rather than looping forever.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ManifestError> {
        let mut stack = Vec::new();
        Self::load_inner(path.as_ref(), &mut stack)
    }

    fn load_inner(path: &Path, stack: &mut Vec<std::path::PathBuf>) -> Result<Self, ManifestError> {
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if stack.contains(&canonical) {
            return Err(ManifestError::ImportCycle(canonical.display().to_string()));
        }
        stack.push(canonical);

        let raw = Self::from_yaml_path(path)?;
        let parent = path.parent().unwrap_or_else(|| Path::new("."));

        let mut merged = Manifest::default();
        for import in &raw.imports {
            validate_import_path(import)?;
            let import_path = parent.join(import);
            let fragment = Self::load_inner(&import_path, stack)?;
            merged.merge(fragment);
        }

        let mut own = raw;
        own.imports = Vec::new();
        merged.merge(own);

        stack.pop();
        Ok(merged)
    }

    /// Resolve the default build cell of every defined environment.
    pub fn resolve_all(&self) -> Result<Vec<ResolvedEnvironment>, ManifestError> {
        let names: Vec<String> = self.environment_names().map(String::from).collect();
        names
            .iter()
            .map(|name| self.resolve_default(name))
            .collect()
    }

    /// Run validation lints. Hard pins are reported once per section (base or
    /// feature); the hard-pin ceiling is checked per composed environment.
    /// Only conda `dependencies` are checked for pins, since PyPI `==` pins are
    /// idiomatic.
    pub fn lint(&self) -> Result<Vec<Lint>, ManifestError> {
        let mut lints = Vec::new();

        for spec in &self.dependencies {
            if is_hard_pin(spec) {
                lints.push(Lint::HardPin {
                    location: Location::Base,
                    spec: spec.clone(),
                });
            }
        }
        for (feature_name, feature) in &self.features {
            for spec in &feature.dependencies {
                if is_hard_pin(spec) {
                    lints.push(Lint::HardPin {
                        location: Location::Feature(feature_name.clone()),
                        spec: spec.clone(),
                    });
                }
            }
        }

        let env_names: Vec<String> = self.environment_names().map(String::from).collect();
        for name in &env_names {
            // Count pins on a representative matrix cell (the first), so the
            // budget check never trips on an ambiguous default.
            let selector = self.targets(name)?.into_iter().next().unwrap_or_default();
            let env = self.resolve(name, &selector)?;
            let count = env.dependencies.iter().filter(|s| is_hard_pin(s)).count();
            if count > DEFAULT_MAX_HARD_PINS {
                lints.push(Lint::TooManyHardPins {
                    environment: env.name,
                    count,
                    max: DEFAULT_MAX_HARD_PINS,
                });
            }
        }

        let base_names: BTreeSet<&str> = self
            .dependencies
            .iter()
            .map(|s| spec_package_name(s))
            .collect();
        for (feature_name, feature) in &self.features {
            for spec in &feature.dependencies {
                let name = spec_package_name(spec);
                if base_names.contains(name) {
                    lints.push(Lint::BaseFeatureCollision {
                        feature: feature_name.clone(),
                        package: name.to_string(),
                    });
                }
            }
        }

        Ok(lints)
    }
}

/// Whether a match-spec carries a hard (`==`) pin.
fn is_hard_pin(spec: &str) -> bool {
    spec.contains("==")
}

/// Append `extra` to `target`, preserving order and skipping values already
/// present.
fn extend_dedup(target: &mut Vec<String>, extra: Vec<String>) {
    for value in extra {
        if !target.contains(&value) {
            target.push(value);
        }
    }
}

/// Extract the package name from a conda match-spec string by taking the text
/// up to the first whitespace or version-operator character.
fn spec_package_name(spec: &str) -> &str {
    let spec = spec.trim();
    let end = spec
        .find(|c: char| c.is_whitespace() || matches!(c, '=' | '<' | '>' | '!' | '~'))
        .unwrap_or(spec.len());
    &spec[..end]
}

/// Rewrite each spec in `deps` whose package name is pinned, replacing it with
/// `<name> <pin>` so the pinned version wins. Specs for unpinned packages are
/// left untouched.
fn bake_pins(deps: &mut [String], pins: &BTreeMap<String, String>) {
    for dep in deps.iter_mut() {
        if let Some(pin) = pins.get(spec_package_name(dep)) {
            *dep = format!("{} {}", spec_package_name(dep), pin);
        }
    }
}

/// Reject an `imports` entry that could escape the manifest tree: a rooted or
/// prefixed path, or one containing a `..` component. Relative imports under the
/// manifest's own directory are allowed. Guards against path traversal if a
/// manifest is ever accepted from untrusted input.
fn validate_import_path(import: &str) -> Result<(), ManifestError> {
    let path = Path::new(import);
    if path.has_root()
        || path.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir | std::path::Component::Prefix(_)
            )
        })
    {
        return Err(ManifestError::UnsafeImport(import.to_string()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
project:
  name: sample
  channels: [internal, conda-forge]
  platforms: [linux-64]

channels:
  internal:
    url: https://example.com/internal
    priority: 10
  conda-forge:
    url: https://example.com/conda-forge

dependencies:
  - python >=3.11
  - numpy >=2

features:
  develop:
    dependencies: [pytest, ruff]
  ray:
    dependencies: [ray]
  docs:
    pypi-dependencies: [gendoc]

environments:
  app: [ray]
  dev: [ray, develop, docs]
"#;

    #[test]
    fn parses_minimal_fields() {
        let m = Manifest::from_yaml_str(SAMPLE).expect("parses");
        assert_eq!(m.project.name, "sample");
        assert_eq!(m.project.platforms, ["linux-64"]);
        assert_eq!(m.channels["internal"].priority, Some(10));
        assert_eq!(m.channels["conda-forge"].priority, None);
        assert_eq!(m.features.len(), 3);
    }

    #[test]
    fn resolve_composes_dedups_and_sorts() {
        let m = Manifest::from_yaml_str(SAMPLE).expect("parses");
        let dev = m.resolve("dev", &Selector::default()).expect("resolves");
        // base (python, numpy) + develop (pytest, ruff) + ray (ray); docs adds
        // only a pypi dep, so it does not appear in conda deps.
        assert_eq!(
            dev.dependencies,
            ["numpy >=2", "pytest", "python >=3.11", "ray", "ruff"]
        );
        assert_eq!(dev.pypi_dependencies, ["gendoc"]);
    }

    #[test]
    fn resolve_keeps_conda_and_pypi_separate() {
        let m = Manifest::from_yaml_str(SAMPLE).expect("parses");
        let app = m.resolve("app", &Selector::default()).expect("resolves");
        assert_eq!(app.dependencies, ["numpy >=2", "python >=3.11", "ray"]);
        assert!(app.pypi_dependencies.is_empty());
    }

    #[test]
    fn resolve_unknown_environment_errors() {
        let m = Manifest::from_yaml_str(SAMPLE).expect("parses");
        let err = m.resolve("nope", &Selector::default()).unwrap_err();
        assert!(matches!(err, ManifestError::UnknownEnvironment(name) if name == "nope"));
    }

    #[test]
    fn resolve_unknown_feature_errors() {
        let yaml = r#"
project:
  name: x
dependencies: [python]
environments:
  bad: [missing]
"#;
        let m = Manifest::from_yaml_str(yaml).expect("parses");
        let err = m.resolve("bad", &Selector::default()).unwrap_err();
        assert!(matches!(
            err,
            ManifestError::UnknownFeature { environment, feature }
                if environment == "bad" && feature == "missing"
        ));
    }

    #[test]
    fn lint_flags_hard_pins_with_location() {
        let yaml = r#"
project:
  name: x
dependencies:
  - numpy ==2.0
features:
  foo:
    dependencies: [scipy ==1.0]
environments:
  e: [foo]
"#;
        let m = Manifest::from_yaml_str(yaml).expect("parses");
        let lints = m.lint().expect("lints");
        assert!(lints.contains(&Lint::HardPin {
            location: Location::Base,
            spec: "numpy ==2.0".to_string(),
        }));
        assert!(lints.contains(&Lint::HardPin {
            location: Location::Feature("foo".to_string()),
            spec: "scipy ==1.0".to_string(),
        }));
    }

    #[test]
    fn lint_flags_too_many_hard_pins() {
        let mut deps = String::new();
        for i in 0..(DEFAULT_MAX_HARD_PINS + 1) {
            deps.push_str(&format!("  - pkg{i} =={i}.0\n"));
        }
        let yaml = format!("project:\n  name: x\ndependencies:\n{deps}environments:\n  full: []\n");
        let m = Manifest::from_yaml_str(&yaml).expect("parses");
        let lints = m.lint().expect("lints");
        assert!(lints.contains(&Lint::TooManyHardPins {
            environment: "full".to_string(),
            count: DEFAULT_MAX_HARD_PINS + 1,
            max: DEFAULT_MAX_HARD_PINS,
        }));
    }

    #[test]
    fn lint_flags_base_feature_collision() {
        let yaml = r#"
project:
  name: x
dependencies:
  - numpy >=2
features:
  foo:
    dependencies: [numpy <3]
environments:
  e: [foo]
"#;
        let m = Manifest::from_yaml_str(yaml).expect("parses");
        let lints = m.lint().expect("lints");
        assert!(lints.contains(&Lint::BaseFeatureCollision {
            feature: "foo".to_string(),
            package: "numpy".to_string(),
        }));
    }

    #[test]
    fn yaml_round_trips() {
        let m = Manifest::from_yaml_str(SAMPLE).expect("parses");
        let out = m.to_yaml_string().expect("serializes");
        let reparsed = Manifest::from_yaml_str(&out).expect("reparses");
        assert_eq!(m, reparsed);
    }

    #[test]
    fn spec_package_name_strips_operators() {
        assert_eq!(spec_package_name("numpy"), "numpy");
        assert_eq!(spec_package_name("numpy >=2"), "numpy");
        assert_eq!(spec_package_name("numpy>=2"), "numpy");
        assert_eq!(spec_package_name("python ==3.11"), "python");
        assert_eq!(spec_package_name("pkg!=1.0"), "pkg");
        assert_eq!(spec_package_name("pkg~=1.2"), "pkg");
    }

    #[test]
    fn resolve_selects_variant_with_deps_and_constraints() {
        let yaml = r#"
project:
  name: v
variants:
  gpu:
    dependencies: [cuda]
    constraints: ["pytorch * cuda*"]
features:
  base:
    dependencies: [scipy]
environments:
  app:
    features: [base]
    variant: gpu
"#;
        let m = Manifest::from_yaml_str(yaml).expect("parses");
        let app = m.resolve("app", &Selector::default()).expect("resolves");
        assert_eq!(app.variant.as_deref(), Some("gpu"));
        assert_eq!(app.constraints, ["pytorch * cuda*"]);
        // The variant contributes its own dependency, merged with the feature's.
        assert_eq!(app.dependencies, ["cuda", "scipy"]);
    }

    #[test]
    fn resolve_merges_activation_hooks() {
        let yaml = r#"
project:
  name: a
activation:
  env:
    BASE_VAR: base
  scripts:
    - 'echo base'
variants:
  gpu:
    activation:
      env:
        BASE_VAR: gpu
features:
  tel:
    activation:
      scripts:
        - 'echo tel'
environments:
  app:
    features: [tel]
    variant: gpu
"#;
        let m = Manifest::from_yaml_str(yaml).expect("parses");
        let app = m.resolve("app", &Selector::default()).expect("resolves");
        // env: variant wins over base on a key clash.
        assert_eq!(
            app.activation.env.get("BASE_VAR").map(String::as_str),
            Some("gpu")
        );
        // scripts: base, then feature (variant adds none here), in order.
        assert_eq!(app.activation.scripts, ["echo base", "echo tel"]);
    }

    #[test]
    fn resolve_unknown_variant_errors() {
        let yaml = r#"
project:
  name: v
environments:
  app:
    variant: gpu
"#;
        let m = Manifest::from_yaml_str(yaml).expect("parses");
        let err = m.resolve("app", &Selector::default()).unwrap_err();
        assert!(matches!(
            err,
            ManifestError::UnknownVariant { environment, variant }
                if environment == "app" && variant == "gpu"
        ));
    }

    #[test]
    fn resolve_inherits_variant_via_extends() {
        let yaml = r#"
project:
  name: v
variants:
  cpu:
    constraints: ["pytorch * cpu*"]
environments:
  base:
    variant: cpu
  derived:
    extends: base
"#;
        let m = Manifest::from_yaml_str(yaml).expect("parses");
        let derived = m
            .resolve("derived", &Selector::default())
            .expect("resolves");
        assert_eq!(derived.variant.as_deref(), Some("cpu"));
        assert_eq!(derived.constraints, ["pytorch * cpu*"]);
    }

    #[test]
    fn resolve_inherits_via_extends_and_overrides_platforms() {
        let yaml = r#"
project:
  name: v
  platforms: [linux-64]
dependencies: [python]
features:
  ml:
    dependencies: [numpy]
environments:
  base: [ml]
  win:
    extends: base
    platforms: [win-64]
"#;
        let m = Manifest::from_yaml_str(yaml).expect("parses");
        let win = m.resolve("win", &Selector::default()).expect("resolves");
        // Inherits base's features (ml) plus the global base deps.
        assert_eq!(win.dependencies, ["numpy", "python"]);
        assert_eq!(win.platforms, ["win-64"]);
        assert_eq!(m.composed_features("win").expect("features"), ["ml"]);
        // The base environment falls back to the project platforms.
        assert_eq!(
            m.resolve("base", &Selector::default())
                .expect("resolves")
                .platforms,
            ["linux-64"]
        );
    }

    #[test]
    fn resolve_detects_extends_cycle() {
        let yaml = r#"
project:
  name: v
environments:
  a:
    extends: b
  b:
    extends: a
"#;
        let m = Manifest::from_yaml_str(yaml).expect("parses");
        let err = m.resolve("a", &Selector::default()).unwrap_err();
        assert!(matches!(err, ManifestError::EnvironmentCycle(_)));
    }

    #[test]
    fn resolve_unknown_base_environment_errors() {
        let yaml = r#"
project:
  name: v
environments:
  a:
    extends: ghost
"#;
        let m = Manifest::from_yaml_str(yaml).expect("parses");
        let err = m.resolve("a", &Selector::default()).unwrap_err();
        assert!(matches!(
            err,
            ManifestError::UnknownBaseEnvironment { environment, base }
                if environment == "a" && base == "ghost"
        ));
    }

    #[test]
    fn detailed_and_list_environments_round_trip() {
        let yaml = r#"
project:
  name: v
features:
  ml:
    dependencies: [numpy]
environments:
  base: [ml]
  win:
    extends: base
    platforms: [win-64]
"#;
        let m = Manifest::from_yaml_str(yaml).expect("parses");
        assert!(matches!(
            m.environments["base"],
            EnvironmentSpec::Features(_)
        ));
        assert!(matches!(
            m.environments["win"],
            EnvironmentSpec::Detailed(_)
        ));
        let out = m.to_yaml_string().expect("serializes");
        let reparsed = Manifest::from_yaml_str(&out).expect("reparses");
        assert_eq!(m, reparsed);
    }

    #[test]
    fn merge_unions_maps_and_dedups_lists() {
        let mut base = Manifest::from_yaml_str(
            r#"
project:
  name: root
dependencies: [python, numpy]
features:
  a:
    dependencies: [aa]
"#,
        )
        .expect("parses base");
        let fragment = Manifest::from_yaml_str(
            r#"
dependencies: [numpy, scipy]
features:
  b:
    dependencies: [bb]
"#,
        )
        .expect("parses fragment");

        base.merge(fragment);
        // Project name preserved (fragment has none).
        assert_eq!(base.project.name, "root");
        // Lists concatenate but dedup.
        assert_eq!(base.dependencies, ["python", "numpy", "scipy"]);
        // Feature maps union.
        assert!(base.features.contains_key("a"));
        assert!(base.features.contains_key("b"));
    }

    #[test]
    fn load_resolves_imports_from_separate_files() {
        // A root manifest that imports a feature fragment and a variant
        // fragment, then composes them — proving the model works split across
        // files.
        let dir = std::env::temp_dir().join(format!("nepenthe-import-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");

        std::fs::write(
            dir.join("features.yaml"),
            "features:\n  ml:\n    dependencies: [numpy]\n",
        )
        .expect("write features");
        std::fs::write(
            dir.join("variants.yaml"),
            "variants:\n  gpu:\n    dependencies: [cuda]\n",
        )
        .expect("write variants");
        std::fs::write(
            dir.join("nepenthe.yaml"),
            r#"
project:
  name: split
imports:
  - features.yaml
  - variants.yaml
dependencies: [python]
environments:
  app:
    features: [ml]
    variant: gpu
"#,
        )
        .expect("write root");

        let m = Manifest::load(dir.join("nepenthe.yaml")).expect("loads");
        assert_eq!(m.project.name, "split");
        assert!(m.features.contains_key("ml"));
        assert!(m.variants.contains_key("gpu"));
        // The composed environment pulls deps from base, the imported feature,
        // and the imported variant.
        let app = m.resolve("app", &Selector::default()).expect("resolves");
        assert_eq!(app.dependencies, ["cuda", "numpy", "python"]);
        assert_eq!(app.variant.as_deref(), Some("gpu"));

        std::fs::remove_dir_all(&dir).ok();
    }

    const MATRIX: &str = r#"
project:
  name: matrix
  python: ["3.11", "3.12", "3.13"]
  default-python: "3.11"
variants:
  cpu: {}
  gpu:
    dependencies: [cuda]
features:
  algo:
    dependencies: [scipy]
environments:
  hfalgo:
    features: [algo]
    variants: [cpu, gpu]
    default-variant: cpu
  altenv:
    features: [algo]
    variants: [gpu]
  mwaa:
    features: [algo]
    python: ["3.11"]
"#;

    #[test]
    fn targets_enumerate_variant_times_python_matrix() {
        let m = Manifest::from_yaml_str(MATRIX).expect("parses");
        // hfalgo: {cpu, gpu} x {3.11, 3.12, 3.13} = 6 cells.
        assert_eq!(m.targets("hfalgo").expect("targets").len(), 6);
        // altenv: single variant x 3 pythons = 3 cells.
        assert_eq!(m.targets("altenv").expect("targets").len(), 3);
        // mwaa: no variant axis x its own single python = 1 cell.
        assert_eq!(m.targets("mwaa").expect("targets").len(), 1);
    }

    #[test]
    fn targets_filtered_narrows_to_a_single_cell() {
        let m = Manifest::from_yaml_str(MATRIX).expect("parses");
        // Pin one python: hfalgo keeps cpu@3.11 and gpu@3.11 = 2 cells.
        let by_python = Selector::default().with_python("3.11");
        assert_eq!(
            m.targets_filtered("hfalgo", &by_python)
                .expect("targets")
                .len(),
            2
        );
        // Pin one variant: hfalgo keeps cpu@{3.11,3.12,3.13} = 3 cells.
        let by_variant = Selector::variant("cpu");
        assert_eq!(
            m.targets_filtered("hfalgo", &by_variant)
                .expect("targets")
                .len(),
            3
        );
        // Pin both axes: exactly one cell.
        let one = Selector::variant("gpu").with_python("3.12");
        let cells = m.targets_filtered("hfalgo", &one).expect("targets");
        assert_eq!(cells.len(), 1);
        assert_eq!(cells[0].variant.as_deref(), Some("gpu"));
        assert_eq!(cells[0].python.as_deref(), Some("3.12"));
        // A python outside the axis selects nothing (the caller treats an empty
        // result as a skipped cell).
        let absent = Selector::default().with_python("3.10");
        assert!(m
            .targets_filtered("hfalgo", &absent)
            .expect("targets")
            .is_empty());
        // An empty filter keeps the whole matrix.
        assert_eq!(
            m.targets_filtered("hfalgo", &Selector::default())
                .expect("targets")
                .len(),
            6
        );
    }

    #[test]
    fn selector_matches_pins_only_specified_axes() {
        let cell = Selector::variant("cpu").with_python("3.11");
        // Empty filter matches anything.
        assert!(cell.matches(&Selector::default()));
        // Matching pins.
        assert!(cell.matches(&Selector::default().with_python("3.11")));
        assert!(cell.matches(&Selector::variant("cpu")));
        assert!(cell.matches(&Selector::variant("cpu").with_python("3.11")));
        // Non-matching pins.
        assert!(!cell.matches(&Selector::default().with_python("3.12")));
        assert!(!cell.matches(&Selector::variant("gpu")));
    }

    #[test]
    fn resolve_default_uses_declared_defaults() {
        let m = Manifest::from_yaml_str(MATRIX).expect("parses");
        let d = m.resolve_default("hfalgo").expect("resolves");
        assert_eq!(d.variant.as_deref(), Some("cpu"));
        assert_eq!(d.python.as_deref(), Some("3.11"));
        // cpu variant adds nothing; python is injected as a match-spec.
        assert!(d.dependencies.contains(&"python 3.11.*".to_string()));
        assert!(d.dependencies.contains(&"scipy".to_string()));
        assert!(!d.dependencies.contains(&"cuda".to_string()));
    }

    #[test]
    fn resolve_selects_requested_cell() {
        let m = Manifest::from_yaml_str(MATRIX).expect("parses");
        let sel = Selector::variant("gpu").with_python("3.12");
        let r = m.resolve("hfalgo", &sel).expect("resolves");
        assert_eq!(r.variant.as_deref(), Some("gpu"));
        assert_eq!(r.python.as_deref(), Some("3.12"));
        assert!(r.dependencies.contains(&"cuda".to_string()));
        assert!(r.dependencies.contains(&"python 3.12.*".to_string()));
    }

    #[test]
    fn resolve_rejects_out_of_axis_selection() {
        let m = Manifest::from_yaml_str(MATRIX).expect("parses");
        let bad_variant = m.resolve("hfalgo", &Selector::variant("rocm")).unwrap_err();
        assert!(matches!(
            bad_variant,
            ManifestError::VariantNotInAxis { environment, variant }
                if environment == "hfalgo" && variant == "rocm"
        ));
        let bad_py = m
            .resolve("hfalgo", &Selector::default().with_python("3.9"))
            .unwrap_err();
        assert!(matches!(
            bad_py,
            ManifestError::PythonNotInAxis { environment, python }
                if environment == "hfalgo" && python == "3.9"
        ));
    }

    #[test]
    fn resolve_default_ambiguous_variant_errors() {
        let yaml = r#"
project:
  name: m
variants:
  cpu: {}
  gpu: {}
environments:
  e:
    variants: [cpu, gpu]
"#;
        let m = Manifest::from_yaml_str(yaml).expect("parses");
        let err = m.resolve_default("e").unwrap_err();
        assert!(matches!(
            err,
            ManifestError::AmbiguousVariant { environment } if environment == "e"
        ));
    }

    #[test]
    fn per_env_python_axis_overrides_project() {
        let m = Manifest::from_yaml_str(MATRIX).expect("parses");
        // mwaa narrows python to just 3.11.
        let targets = m.targets("mwaa").expect("targets");
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].python.as_deref(), Some("3.11"));
        let r = m.resolve_default("mwaa").expect("resolves");
        assert_eq!(r.python.as_deref(), Some("3.11"));
    }

    const OVERRIDES: &str = r#"
virtual-packages:
  cuda: "12.9"
  archspec: skylake_avx512
pins:
  grpcio: ">=1.73,<1.74"
  protobuf: ">=6.31.1,<6.31.2"
variants:
  cpu:
    constraints: ["pytorch >=2.8,<2.9 cpu*"]
  gpu:
    constraints: ["pytorch >=2.8,<2.9 cuda129*"]
exclude:
  "3.13": [altenv]
"#;

    #[test]
    fn apply_fills_variant_constraints() {
        let mut m = Manifest::from_yaml_str(MATRIX).expect("parses");
        // Before: cpu variant is empty.
        assert!(m.variants["cpu"].constraints.is_empty());
        let ov = Overrides::from_yaml_str(OVERRIDES).expect("parses overrides");
        m.apply(&ov);
        assert_eq!(m.variants["cpu"].constraints, ["pytorch >=2.8,<2.9 cpu*"]);
        assert_eq!(
            m.variants["gpu"].constraints,
            ["pytorch >=2.8,<2.9 cuda129*"]
        );
        // gpu keeps its pre-existing cuda dependency.
        assert!(m.variants["gpu"].dependencies.contains(&"cuda".to_string()));
    }

    #[test]
    fn apply_bakes_pins_into_dependency_specs() {
        let yaml = r#"
project:
  name: p
features:
  net:
    dependencies: [grpcio, protobuf, requests]
environments:
  app: [net]
"#;
        let mut m = Manifest::from_yaml_str(yaml).expect("parses");
        let ov = Overrides::from_yaml_str(OVERRIDES).expect("overrides");
        m.apply(&ov);
        let net = &m.features["net"].dependencies;
        assert!(net.contains(&"grpcio >=1.73,<1.74".to_string()));
        assert!(net.contains(&"protobuf >=6.31.1,<6.31.2".to_string()));
        // Unpinned package is untouched.
        assert!(net.contains(&"requests".to_string()));
    }

    #[test]
    fn apply_records_virtual_packages_and_exclude_prunes_matrix() {
        let mut m = Manifest::from_yaml_str(MATRIX).expect("parses");
        // Before: altenv has 3 python cells.
        assert_eq!(m.targets("altenv").expect("targets").len(), 3);
        let ov = Overrides::from_yaml_str(OVERRIDES).expect("overrides");
        m.apply(&ov);
        // Global virtual-packages recorded.
        assert_eq!(m.virtual_packages["cuda"], "12.9");
        assert_eq!(m.virtual_packages["archspec"], "skylake_avx512");
        // exclude prunes the (altenv, 3.13) cell → 2 remain.
        let cells = m.targets("altenv").expect("targets");
        assert_eq!(cells.len(), 2);
        assert!(cells.iter().all(|s| s.python.as_deref() != Some("3.13")));
    }

    #[test]
    fn include_allowlist_keeps_only_named_environments() {
        let mut m = Manifest::from_yaml_str(MATRIX).expect("parses");
        // altenv builds for all 3 pythons by default.
        assert_eq!(m.targets("altenv").expect("targets").len(), 3);
        // An include allowlist for 3.13 naming only hfalgo drops altenv@3.13...
        let ov = Overrides::from_yaml_str("include:\n  \"3.13\": [hfalgo]\n").expect("overrides");
        m.apply(&ov);
        let cells = m.targets("altenv").expect("targets");
        assert_eq!(cells.len(), 2);
        assert!(cells.iter().all(|s| s.python.as_deref() != Some("3.13")));
        // ...but hfalgo, on the allowlist, keeps its 3.13 cells.
        assert!(m
            .targets("hfalgo")
            .expect("targets")
            .iter()
            .any(|s| s.python.as_deref() == Some("3.13")));
    }

    #[test]
    fn exclude_wins_over_include_on_overlap() {
        let mut m = Manifest::from_yaml_str(MATRIX).expect("parses");
        let ov = Overrides::from_yaml_str(
            "include:\n  \"3.13\": [altenv]\nexclude:\n  \"3.13\": [altenv]\n",
        )
        .expect("overrides");
        m.apply(&ov);
        // altenv is both included and excluded at 3.13 → excluded.
        let cells = m.targets("altenv").expect("targets");
        assert!(cells.iter().all(|s| s.python.as_deref() != Some("3.13")));
    }

    #[test]
    fn overrides_yaml_round_trips() {
        let ov = Overrides::from_yaml_str(OVERRIDES).expect("parses");
        let out = ov.to_yaml_string().expect("serializes");
        let reparsed = Overrides::from_yaml_str(&out).expect("reparses");
        assert_eq!(ov, reparsed);
    }

    #[test]
    fn targets_on_extends_cycle_errors_instead_of_hanging() {
        let yaml = r#"
project:
  name: c
environments:
  a:
    extends: b
  b:
    extends: a
"#;
        let m = Manifest::from_yaml_str(yaml).expect("parses");
        // Previously the axis/platform resolvers looped forever on this graph.
        assert!(matches!(
            m.targets("a").unwrap_err(),
            ManifestError::EnvironmentCycle(_)
        ));
        assert!(matches!(
            m.lint().unwrap_err(),
            ManifestError::EnvironmentCycle(_)
        ));
    }

    #[test]
    fn resolve_default_variant_not_in_axis_errors() {
        let yaml = r#"
project:
  name: m
variants:
  cpu: {}
  gpu: {}
environments:
  e:
    variants: [cpu, gpu]
    default-variant: rocm
"#;
        let m = Manifest::from_yaml_str(yaml).expect("parses");
        assert!(matches!(
            m.resolve_default("e").unwrap_err(),
            ManifestError::VariantNotInAxis { environment, variant }
                if environment == "e" && variant == "rocm"
        ));
    }

    #[test]
    fn resolve_default_python_not_in_axis_errors() {
        let yaml = r#"
project:
  name: m
  python: ["3.11", "3.12"]
  default-python: "3.13"
environments:
  e: []
"#;
        let m = Manifest::from_yaml_str(yaml).expect("parses");
        assert!(matches!(
            m.resolve_default("e").unwrap_err(),
            ManifestError::PythonNotInAxis { environment, python }
                if environment == "e" && python == "3.13"
        ));
    }

    #[test]
    fn merge_carries_virtual_packages_and_exclude() {
        let mut base = Manifest::from_yaml_str(
            "project:\n  name: base\nvirtual-packages:\n  cuda: \"12.0\"\nexclude:\n  \"3.13\": [a]\nenvironments:\n  a: []\n",
        )
        .expect("parses base");
        let other = Manifest::from_yaml_str(
            "virtual-packages:\n  archspec: skylake\nexclude:\n  \"3.13\": [b]\n  \"3.12\": [a]\n",
        )
        .expect("parses other");
        base.merge(other);
        assert_eq!(base.virtual_packages["cuda"], "12.0");
        assert_eq!(base.virtual_packages["archspec"], "skylake");
        assert_eq!(base.exclude["3.13"], ["a", "b"]);
        assert_eq!(base.exclude["3.12"], ["a"]);
    }

    #[test]
    fn resolve_carries_variant_virtual_packages() {
        let yaml = r#"
project:
  name: v
variants:
  gpu:
    virtual-packages:
      cuda: "12.9"
environments:
  app:
    variant: gpu
"#;
        let m = Manifest::from_yaml_str(yaml).expect("parses");
        let app = m.resolve_default("app").expect("resolves");
        assert_eq!(app.variant.as_deref(), Some("gpu"));
        assert_eq!(
            app.virtual_packages.get("cuda").map(String::as_str),
            Some("12.9")
        );
    }

    #[test]
    fn validate_import_path_rejects_escapes() {
        assert!(validate_import_path("features.yaml").is_ok());
        assert!(validate_import_path("sub/features.yaml").is_ok());
        assert!(matches!(
            validate_import_path("/etc/passwd").unwrap_err(),
            ManifestError::UnsafeImport(_)
        ));
        assert!(matches!(
            validate_import_path("../secrets.yaml").unwrap_err(),
            ManifestError::UnsafeImport(_)
        ));
        assert!(matches!(
            validate_import_path("a/../../b.yaml").unwrap_err(),
            ManifestError::UnsafeImport(_)
        ));
    }
}
