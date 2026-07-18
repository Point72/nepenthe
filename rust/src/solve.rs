//! The solver core, built on rattler.
//!
//! Turns a [`SolveRequest`] (channels, platform, specs, constraints, virtual
//! packages) into a concrete, solved package set by fetching repodata through
//! [`rattler_repodata_gateway::Gateway`] and solving with `rattler_solve`'s
//! resolvo solver — the same pipeline py-rattler and pixi use.
//!
//! A [`SolveRequest`] is typically built from a manifest's
//! [`ResolvedEnvironment`](crate::manifest::ResolvedEnvironment) via
//! [`SolveRequest::from_resolved`].

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use rattler_conda_types::{
    Channel, ChannelConfig, GenericVirtualPackage, MatchSpec, PackageName, ParseStrictness,
    Platform, Version,
};
use rattler_repodata_gateway::Gateway;
use rattler_solve::{resolvo::Solver, ChannelPriority, ExcludeNewer, SolverImpl, SolverTask};

use crate::manifest::ResolvedEnvironment;

/// Channel-priority policy for a solve. Mirrors conda's setting: `Strict` only
/// considers the first channel a package appears in (priority order), while
/// `Disabled` lets the highest version win across all channels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ChannelPriorityMode {
    /// First channel a package is found in wins (channel order matters).
    #[default]
    Strict,
    /// Highest version wins regardless of channel.
    Disabled,
}

impl From<ChannelPriorityMode> for ChannelPriority {
    fn from(mode: ChannelPriorityMode) -> Self {
        match mode {
            ChannelPriorityMode::Strict => ChannelPriority::Strict,
            ChannelPriorityMode::Disabled => ChannelPriority::Disabled,
        }
    }
}

/// Error returned when a channel-priority string is not `strict` or `disabled`.
#[derive(Debug, PartialEq, Eq)]
pub struct ParseChannelPriorityError(String);

impl fmt::Display for ParseChannelPriorityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unknown channel-priority '{}' (expected strict or disabled)",
            self.0
        )
    }
}

impl std::error::Error for ParseChannelPriorityError {}

impl FromStr for ChannelPriorityMode {
    type Err = ParseChannelPriorityError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "strict" => Ok(ChannelPriorityMode::Strict),
            "disabled" => Ok(ChannelPriorityMode::Disabled),
            other => Err(ParseChannelPriorityError(other.to_string())),
        }
    }
}

/// A request to solve one environment cell for one platform.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SolveRequest {
    /// Channel names or URLs, in priority order (e.g. `["conda-forge"]`).
    pub channels: Vec<String>,
    /// The target platform subdir (e.g. `linux-64`); `noarch` is always added.
    pub platform: String,
    /// Conda match-spec strings to install.
    pub specs: Vec<String>,
    /// Match-spec constraints that bound the solve without adding a dependency.
    pub constraints: Vec<String>,
    /// Virtual-package assumptions, keyed by name → value (e.g. `cuda` →
    /// `12.9`). A bare name is prefixed with `__` to form the conda virtual
    /// package (`cuda` → `__cuda`). These override host-detected packages.
    pub virtual_packages: BTreeMap<String, String>,
    /// Channel-priority policy for this solve.
    pub channel_priority: ChannelPriorityMode,
    /// Optional repodata pin: an RFC3339 timestamp; packages published after it
    /// are ignored. Pinning the cutoff makes a re-solve reproducible against an
    /// evolving channel.
    pub exclude_newer: Option<String>,
}

impl SolveRequest {
    /// Build a request from a resolved environment cell plus the channels to
    /// solve against. Uses the environment's conda dependencies as specs, its
    /// variant constraints, and any global virtual-package overrides. The
    /// platform is the environment's first resolved platform, or `linux-64`.
    pub fn from_resolved(
        resolved: &ResolvedEnvironment,
        channels: Vec<String>,
        virtual_packages: BTreeMap<String, String>,
    ) -> Self {
        let platform = resolved
            .platforms
            .first()
            .cloned()
            .unwrap_or_else(|| "linux-64".to_string());
        // Base (project) channels first, then any channels the environment adds
        // (e.g. a private channel only `myenv-private` needs), deduped.
        let mut all_channels = channels;
        for channel in &resolved.channels {
            if !all_channels.contains(channel) {
                all_channels.push(channel.clone());
            }
        }
        SolveRequest {
            channels: all_channels,
            platform,
            specs: resolved.dependencies.clone(),
            constraints: resolved.constraints.clone(),
            virtual_packages,
            ..Default::default()
        }
    }

    /// Set the channel-priority policy (builder style).
    pub fn with_channel_priority(mut self, priority: ChannelPriorityMode) -> Self {
        self.channel_priority = priority;
        self
    }

    /// Pin the repodata cutoff to an RFC3339 timestamp (builder style).
    pub fn with_exclude_newer(mut self, cutoff: impl Into<String>) -> Self {
        self.exclude_newer = Some(cutoff.into());
        self
    }
}

/// How channel names in a [`SolveRequest`] are resolved to URLs. This is how
/// nepenthe points at an internal Artifactory mirror instead of public conda
/// channels: a bare channel name resolves against the
/// `channel_alias`, and any name listed in `mirrors` is rewritten to its mirror
/// URL. Both default to empty, in which case bare names resolve against the
/// conda default (`conda.anaconda.org`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChannelSettings {
    /// Base URL that bare channel names resolve against, overriding the conda
    /// default (e.g. `https://artifacts.example.com/artifactory/api/conda`).
    pub channel_alias: Option<String>,
    /// Per-channel mirror overrides: a channel entry exactly matching a key is
    /// replaced by its value before resolution. Lets a manifest keep saying
    /// `conda-forge` while solving against an internal Artifactory mirror.
    pub mirrors: BTreeMap<String, String>,
}

impl ChannelSettings {
    /// Settings that resolve bare channel names against `alias`.
    pub fn with_alias(alias: impl Into<String>) -> Self {
        ChannelSettings {
            channel_alias: Some(alias.into()),
            mirrors: BTreeMap::new(),
        }
    }

    /// Add a mirror override: a channel named `from` is solved against `to`.
    pub fn mirror(mut self, from: impl Into<String>, to: impl Into<String>) -> Self {
        self.mirrors.insert(from.into(), to.into());
        self
    }

    /// Derive channel settings from a manifest's `project.channel-alias` and
    /// per-channel `mirror`/`url` overrides. A channel with
    /// an explicit `url` is treated as a self-mirror to that URL; a channel
    /// with a `mirror` redirects to it. This is the YAML → solver bridge.
    pub fn from_manifest(manifest: &crate::manifest::Manifest) -> Self {
        let mut mirrors = BTreeMap::new();
        for (name, channel) in &manifest.channels {
            if let Some(mirror) = &channel.mirror {
                mirrors.insert(name.clone(), mirror.clone());
            } else if let Some(url) = &channel.url {
                mirrors.insert(name.clone(), url.clone());
            }
        }
        ChannelSettings {
            channel_alias: manifest.project.channel_alias.clone(),
            mirrors,
        }
    }

    /// Resolve one channel name to its effective URL under these settings:
    /// apply a `mirrors` override first, then prefix the `channel_alias` if the
    /// result is still a bare name. A value that is already a URL is returned
    /// unchanged (unless mirrored). Use this to preview where a channel will be
    /// fetched from — e.g. to confirm a manifest's channels point at an
    /// internal Artifactory instead of the public conda servers.
    pub fn resolve(&self, channel: &str) -> String {
        resolve_channel_url(channel, self)
    }
}

/// Resolve a channel string to its effective URL/name: apply a mirror override
/// first, then prefix the alias if the result is still a bare name. A channel
/// that is already a URL (`scheme://…`) is returned unchanged unless mirrored.
fn resolve_channel_url(channel: &str, settings: &ChannelSettings) -> String {
    let effective = settings
        .mirrors
        .get(channel)
        .map(String::as_str)
        .unwrap_or(channel);
    match &settings.channel_alias {
        Some(alias) if !effective.contains("://") => {
            format!("{}/{}", alias.trim_end_matches('/'), effective)
        }
        _ => effective.to_string(),
    }
}

/// One package in a solved environment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SolvedPackage {
    /// Package name.
    pub name: String,
    /// Package version.
    pub version: String,
    /// Build string.
    pub build: String,
    /// Download URL.
    pub url: String,
}

/// The result of a solve plus the **provenance** needed to reproduce it: the
/// resolved channel URLs, target platform, virtual packages, priority policy,
/// and repodata cutoff that produced this exact package set. Recording these
/// is what lets a later re-solve be deterministic.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SolveOutcome {
    /// The solved conda package records, in install (topological) order. This
    /// is the full data, from which lockfiles and `@EXPLICIT`/`environment.yml`
    /// exports are produced.
    pub records: Vec<rattler_conda_types::RepoDataRecord>,
    /// The fully-resolved channel URLs used, in priority order.
    pub channels: Vec<String>,
    /// The platform solved for.
    pub platform: String,
    /// The effective virtual packages (`name=version=build`), sorted.
    pub virtual_packages: Vec<String>,
    /// The channel-priority policy used.
    pub channel_priority: ChannelPriorityMode,
    /// The repodata cutoff applied, if any.
    pub exclude_newer: Option<String>,
}

impl SolveOutcome {
    /// A lightweight view of the solved set as [`SolvedPackage`]s (name,
    /// version, build, url), in install order.
    pub fn packages(&self) -> Vec<SolvedPackage> {
        self.records
            .iter()
            .map(|r| SolvedPackage {
                name: r.package_record.name.as_normalized().to_string(),
                version: r.package_record.version.as_str().to_string(),
                build: r.package_record.build.clone(),
                url: r.url.to_string(),
            })
            .collect()
    }
}

/// Errors raised while building or running a solve.
#[derive(Debug)]
pub enum SolveError {
    /// A channel, platform, spec, or virtual-package string failed to parse.
    Parse(String),
    /// The request uses a feature that is modeled but not yet implemented
    /// (e.g. PyPI dependencies), so solving it would silently drop data.
    Unsupported(String),
    /// Building the authenticated HTTP client failed (bad credential store).
    Auth(String),
    /// Fetching repodata from a channel failed.
    Gateway(rattler_repodata_gateway::GatewayError),
    /// The solver could not satisfy the request.
    Solve(rattler_solve::SolveError),
}

impl fmt::Display for SolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SolveError::Parse(msg) => write!(f, "failed to parse solve input: {msg}"),
            SolveError::Unsupported(msg) => write!(f, "unsupported solve request: {msg}"),
            SolveError::Auth(msg) => write!(f, "failed to build authenticated client: {msg}"),
            SolveError::Gateway(e) => write!(f, "failed to fetch repodata: {e}"),
            SolveError::Solve(e) => write!(f, "solve failed: {e}"),
        }
    }
}

impl std::error::Error for SolveError {}

impl From<rattler_repodata_gateway::GatewayError> for SolveError {
    fn from(e: rattler_repodata_gateway::GatewayError) -> Self {
        SolveError::Gateway(e)
    }
}

impl From<rattler_solve::SolveError> for SolveError {
    fn from(e: rattler_solve::SolveError) -> Self {
        SolveError::Solve(e)
    }
}

/// Build the conda virtual packages for a solve targeting `platform`: the
/// platform's baseline set with the request's overrides applied on top.
///
/// The baseline comes from [`VirtualPackages::detect_for_platform`]: for the
/// host platform it is detected; for a **cross-platform** solve rattler supplies
/// deterministic cross-compile defaults (`__win` for `win-*`,
/// `__unix`/`__osx`/`__archspec` for `osx-*`, `__unix`/`__linux`/`__glibc` for
/// `linux-*`), so a Linux host can solve macOS/Windows environments
/// reproducibly.
///
/// Overrides for the **typed** conda virtual packages (`cuda`, `archspec`,
/// `glibc`/`libc`, `osx`, `linux`, `win`, `cuda_arch`) are routed through
/// rattler's [`VirtualPackageOverrides`] so each is encoded the conda way. This
/// matters because the encoding differs per package: `__cuda` carries the value
/// in its **version** (`__cuda=12.9=0`), but `__archspec` carries the
/// microarchitecture in its **build string** with version `1`
/// (`__archspec=1=skylake_avx512`). A microarchitecture name like
/// `skylake_avx512` also happens to parse as a conda `Version`, so a naive
/// "is it a version?" heuristic would mis-encode it — rattler's typed parser
/// does the right thing. Any other (custom) virtual-package name falls back to
/// that heuristic.
fn virtual_packages(
    platform: Platform,
    overrides: &BTreeMap<String, String>,
) -> Result<Vec<GenericVirtualPackage>, SolveError> {
    use rattler_virtual_packages::{Override, VirtualPackageOverrides, VirtualPackages};

    // Route the typed conda virtual packages through rattler so each gets its
    // correct encoding; collect anything else for the fallback below.
    let mut typed = VirtualPackageOverrides::default();
    let mut custom: Vec<(&String, &String)> = Vec::new();
    for (name, value) in overrides {
        let slot = match name.strip_prefix("__").unwrap_or(name) {
            "win" => &mut typed.win,
            "osx" => &mut typed.osx,
            "linux" => &mut typed.linux,
            "glibc" | "libc" => &mut typed.libc,
            "cuda" => &mut typed.cuda,
            "cuda_arch" | "cuda-arch" => &mut typed.cuda_arch,
            "archspec" => &mut typed.archspec,
            _ => {
                custom.push((name, value));
                continue;
            }
        };
        *slot = Some(Override::String(value.clone()));
    }

    let mut by_name: BTreeMap<String, GenericVirtualPackage> =
        VirtualPackages::detect_for_platform(platform, &typed)
            .map_err(|e| SolveError::Parse(format!("virtual package detection failed: {e}")))?
            .into_generic_virtual_packages()
            .map(|g| (g.name.as_normalized().to_string(), g))
            .collect();

    // Custom/unknown virtual packages: a value that parses as a version is used
    // as the version (build `0`); otherwise it is treated as a build string with
    // version `1`.
    for (name, value) in custom {
        let full = if name.starts_with("__") {
            name.clone()
        } else {
            format!("__{name}")
        };
        let package_name = PackageName::try_from(full.as_str())
            .map_err(|e| SolveError::Parse(format!("bad virtual package '{name}': {e}")))?;
        let (version, build) = match Version::from_str(value.as_str()) {
            Ok(v) => (v, "0".to_string()),
            Err(_) => (
                Version::from_str("1").expect("'1' is a valid version"),
                value.clone(),
            ),
        };
        by_name.insert(
            package_name.as_normalized().to_string(),
            GenericVirtualPackage {
                name: package_name,
                version,
                build_string: build,
            },
        );
    }

    Ok(by_name.into_values().collect())
}

/// Parse match-spec strings leniently into [`MatchSpec`]s.
fn parse_specs(specs: &[String]) -> Result<Vec<MatchSpec>, SolveError> {
    specs
        .iter()
        .map(|s| {
            MatchSpec::from_str(s, ParseStrictness::Lenient)
                .map_err(|e| SolveError::Parse(format!("bad match-spec '{s}': {e}")))
        })
        .collect()
}

/// Solve a [`SolveRequest`] into a [`SolveOutcome`] (package set + provenance).
///
/// Fetches repodata for the request's channels (for the target platform plus
/// `noarch`) and runs the resolvo solver, applying the request's channel
/// priority and repodata cutoff. `settings` controls how channel names resolve
/// to URLs — e.g. pointing at an internal Artifactory mirror. This performs
/// network I/O via the gateway, so it must be awaited inside a tokio runtime.
pub async fn solve(
    request: &SolveRequest,
    settings: &ChannelSettings,
) -> Result<SolveOutcome, SolveError> {
    let channel_config = ChannelConfig::default_with_root_dir(
        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
    );
    let resolved_urls: Vec<String> = request
        .channels
        .iter()
        .map(|c| resolve_channel_url(c, settings))
        .collect();
    let channels = resolved_urls
        .iter()
        .map(|url| {
            Channel::from_str(url, &channel_config)
                .map_err(|e| SolveError::Parse(format!("bad channel '{url}': {e}")))
        })
        .collect::<Result<Vec<_>, _>>()?;

    let platform = Platform::from_str(&request.platform)
        .map_err(|e| SolveError::Parse(format!("bad platform '{}': {e}", request.platform)))?;

    let specs = parse_specs(&request.specs)?;
    let constraints = parse_specs(&request.constraints)?;
    let vpkgs = virtual_packages(platform, &request.virtual_packages)?;

    let exclude_newer = request
        .exclude_newer
        .as_deref()
        .map(|ts| {
            jiff::Timestamp::from_str(ts)
                .map(ExcludeNewer::from_datetime)
                .map_err(|e| SolveError::Parse(format!("bad exclude-newer timestamp '{ts}': {e}")))
        })
        .transpose()?;

    let gateway = Gateway::builder()
        .with_client(crate::net::authenticated_client().map_err(SolveError::Auth)?)
        .finish();
    let repodata = gateway
        .query(channels, [platform, Platform::NoArch], specs.clone())
        .recursive(true)
        .execute()
        .await?;

    let mut task = SolverTask::from_iter(repodata.iter().map(|rd| rd.iter()));
    task.specs = specs;
    task.constraints = constraints;
    task.virtual_packages = vpkgs.clone();
    task.channel_priority = request.channel_priority.into();
    task.exclude_newer = exclude_newer;

    let solved = Solver.solve(task)?;

    let mut virtual_packages: Vec<String> = vpkgs
        .iter()
        .map(|v| {
            format!(
                "{}={}={}",
                v.name.as_normalized(),
                v.version,
                v.build_string
            )
        })
        .collect();
    virtual_packages.sort();

    Ok(SolveOutcome {
        records: solved.records,
        channels: resolved_urls,
        platform: request.platform.clone(),
        virtual_packages,
        channel_priority: request.channel_priority,
        exclude_newer: request.exclude_newer.clone(),
    })
}

/// Solve every build cell of a manifest environment and return one
/// [`SolveOutcome`] per cell. This drives the `(env × platform × variant ×
/// python)` matrix: each of the environment's
/// [`targets`](crate::manifest::Manifest::targets) is crossed with the
/// environment's resolved platforms, and each combination is solved.
///
/// `select` narrows the matrix to a single cell (or a slice of it): any axis it
/// pins (e.g. one `--python`) keeps only the matching targets, so a caller can
/// build just `python 3.11` without solving the rest. An all-`None` selector
/// builds the full matrix. Returns the `(Selector, platform, outcome)` for
/// every solved cell.
pub async fn solve_environment(
    manifest: &crate::manifest::Manifest,
    environment: &str,
    settings: &ChannelSettings,
    channel_priority: ChannelPriorityMode,
    exclude_newer: Option<String>,
    select: &crate::manifest::Selector,
) -> Result<Vec<(crate::manifest::Selector, String, SolveOutcome)>, SolveError> {
    let targets = manifest
        .targets_filtered(environment, select)
        .map_err(|e| SolveError::Parse(e.to_string()))?;

    let mut results = Vec::new();
    for selector in targets {
        let resolved = manifest
            .resolve(environment, &selector)
            .map_err(|e| SolveError::Parse(e.to_string()))?;
        if !resolved.pypi_dependencies.is_empty() {
            return Err(SolveError::Unsupported(format!(
                "environment '{environment}' has pypi-dependencies {:?}; PyPI resolution is not \
                 implemented, so the lock would silently omit them",
                resolved.pypi_dependencies
            )));
        }
        let platforms = if resolved.platforms.is_empty() {
            vec!["linux-64".to_string()]
        } else {
            resolved.platforms.clone()
        };
        // Variant virtual packages override the manifest's global ones.
        let mut vpkgs = manifest.virtual_packages.clone();
        vpkgs.extend(resolved.virtual_packages.clone());
        for platform in platforms {
            let mut request = SolveRequest::from_resolved(
                &resolved,
                manifest.project.channels.clone(),
                vpkgs.clone(),
            )
            .with_channel_priority(channel_priority);
            request.platform = platform.clone();
            if let Some(cutoff) = &exclude_newer {
                request = request.with_exclude_newer(cutoff.clone());
            }
            let outcome = solve(&request, settings).await?;
            results.push((selector.clone(), platform, outcome));
        }
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rattler_conda_types::RepoDataRecord;

    /// Trivial smoke test: an empty solve (no available packages, no specs)
    /// returns an empty solution. Proves the rattler solver links and runs
    /// fully offline, with no repodata or network access.
    #[test]
    fn empty_solve_returns_no_records() {
        let available: Vec<Vec<RepoDataRecord>> = Vec::new();
        let task = SolverTask::from_iter(available.iter().map(Vec::as_slice));
        let result = Solver.solve(task).expect("empty solve should succeed");
        assert!(result.records.is_empty());
    }

    #[test]
    fn from_resolved_carries_specs_constraints_platform() {
        let resolved = ResolvedEnvironment {
            name: "app".to_string(),
            dependencies: vec!["python 3.11.*".to_string(), "numpy".to_string()],
            pypi_dependencies: vec![],
            constraints: vec!["pytorch * cpu*".to_string()],
            virtual_packages: BTreeMap::new(),
            variant: Some("cpu".to_string()),
            python: Some("3.11".to_string()),
            platforms: vec!["osx-arm64".to_string()],
            channels: vec![],
            activation: Default::default(),
        };
        let req = SolveRequest::from_resolved(
            &resolved,
            vec!["conda-forge".to_string()],
            BTreeMap::new(),
        );
        assert_eq!(req.platform, "osx-arm64");
        assert_eq!(req.specs, ["python 3.11.*", "numpy"]);
        assert_eq!(req.constraints, ["pytorch * cpu*"]);
        assert_eq!(req.channels, ["conda-forge"]);
        // defaults for the new fields
        assert_eq!(req.channel_priority, ChannelPriorityMode::Strict);
        assert_eq!(req.exclude_newer, None);
    }

    #[test]
    fn from_resolved_appends_environment_channels() {
        let resolved = ResolvedEnvironment {
            name: "private".to_string(),
            dependencies: vec!["myenv-config".to_string()],
            platforms: vec!["linux-64".to_string()],
            channels: vec!["private-channel".to_string(), "conda-forge".to_string()],
            ..Default::default()
        };
        let req = SolveRequest::from_resolved(
            &resolved,
            vec!["conda-forge".to_string()],
            BTreeMap::new(),
        );
        // Project channel first, env channel appended, no duplicate conda-forge.
        assert_eq!(req.channels, ["conda-forge", "private-channel"]);
    }

    #[test]
    fn builders_set_priority_and_cutoff() {
        let req = SolveRequest::default()
            .with_channel_priority(ChannelPriorityMode::Disabled)
            .with_exclude_newer("2026-01-01T00:00:00Z");
        assert_eq!(req.channel_priority, ChannelPriorityMode::Disabled);
        assert_eq!(req.exclude_newer.as_deref(), Some("2026-01-01T00:00:00Z"));
    }

    #[test]
    fn channel_priority_mode_maps_to_rattler() {
        assert_eq!(
            ChannelPriority::from(ChannelPriorityMode::Strict),
            ChannelPriority::Strict
        );
        assert_eq!(
            ChannelPriority::from(ChannelPriorityMode::Disabled),
            ChannelPriority::Disabled
        );
    }

    #[test]
    fn channel_priority_mode_parses_from_str() {
        assert_eq!(
            "strict".parse::<ChannelPriorityMode>().unwrap(),
            ChannelPriorityMode::Strict
        );
        assert_eq!(
            "Disabled".parse::<ChannelPriorityMode>().unwrap(),
            ChannelPriorityMode::Disabled
        );
        assert!("nonsense".parse::<ChannelPriorityMode>().is_err());
    }

    #[test]
    fn exclude_newer_timestamp_parses() {
        // A valid RFC3339 cutoff parses; a bad one is rejected at solve time.
        assert!(jiff::Timestamp::from_str("2026-01-01T00:00:00Z").is_ok());
        assert!(jiff::Timestamp::from_str("not-a-date").is_err());
    }

    #[test]
    fn virtual_packages_apply_cuda_override() {
        let mut overrides = BTreeMap::new();
        overrides.insert("cuda".to_string(), "12.9".to_string());
        let vps = virtual_packages(Platform::current(), &overrides).expect("builds vpkgs");
        let cuda = vps
            .iter()
            .find(|v| v.name.as_normalized() == "__cuda")
            .expect("__cuda present");
        assert_eq!(cuda.version.to_string(), "12.9");
    }

    fn vpkg_names(platform: &str) -> Vec<String> {
        let p = Platform::from_str(platform).expect("valid platform");
        virtual_packages(p, &BTreeMap::new())
            .expect("builds vpkgs")
            .iter()
            .map(|v| v.name.as_normalized().to_string())
            .collect()
    }

    #[test]
    fn cross_platform_windows_virtual_packages() {
        // Solving win-64 from a non-Windows host still yields the Windows
        // baseline (and no unix/osx markers).
        let names = vpkg_names("win-64");
        assert!(names.contains(&"__win".to_string()), "got {names:?}");
        assert!(names.contains(&"__archspec".to_string()), "got {names:?}");
        assert!(!names.contains(&"__unix".to_string()), "got {names:?}");
        assert!(!names.contains(&"__osx".to_string()), "got {names:?}");
    }

    #[test]
    fn cross_platform_macos_virtual_packages() {
        // osx-arm64 from a Linux host yields the macOS baseline.
        let names = vpkg_names("osx-arm64");
        assert!(names.contains(&"__unix".to_string()), "got {names:?}");
        assert!(names.contains(&"__osx".to_string()), "got {names:?}");
        assert!(names.contains(&"__archspec".to_string()), "got {names:?}");
        assert!(!names.contains(&"__win".to_string()), "got {names:?}");
        assert!(!names.contains(&"__linux".to_string()), "got {names:?}");
    }

    #[test]
    fn cross_platform_linux_aarch64_virtual_packages() {
        // A different Linux arch yields the Linux baseline (unix + linux + glibc).
        let names = vpkg_names("linux-aarch64");
        assert!(names.contains(&"__unix".to_string()), "got {names:?}");
        assert!(names.contains(&"__linux".to_string()), "got {names:?}");
        assert!(names.contains(&"__glibc".to_string()), "got {names:?}");
        assert!(!names.contains(&"__win".to_string()), "got {names:?}");
        assert!(!names.contains(&"__osx".to_string()), "got {names:?}");
    }

    #[test]
    fn override_wins_over_cross_platform_baseline() {
        // A `cuda` override layers on top of a cross-platform baseline: the
        // linux markers stay and `__cuda` is added with the requested version.
        let mut overrides = BTreeMap::new();
        overrides.insert("cuda".to_string(), "12.0".to_string());
        let p = Platform::from_str("linux-aarch64").expect("valid platform");
        let vps = virtual_packages(p, &overrides).expect("builds vpkgs");
        let cuda = vps
            .iter()
            .find(|v| v.name.as_normalized() == "__cuda")
            .expect("__cuda present");
        assert_eq!(cuda.version.to_string(), "12.0");
        // the linux baseline is still present alongside the override
        assert!(vps.iter().any(|v| v.name.as_normalized() == "__linux"));
        assert!(vps.iter().any(|v| v.name.as_normalized() == "__glibc"));
    }

    #[test]
    fn archspec_override_encodes_name_in_build_not_version() {
        // `__archspec` carries the microarchitecture in its BUILD string with
        // version "1" (conda convention), e.g. `__archspec=1=skylake_avx512`.
        // A naive "is it a version?" heuristic mis-encodes this because
        // `skylake_avx512` also parses as a conda Version — so guard it.
        let mut overrides = BTreeMap::new();
        overrides.insert("archspec".to_string(), "skylake_avx512".to_string());
        let vps = virtual_packages(Platform::current(), &overrides).expect("builds vpkgs");
        let archspec = vps
            .iter()
            .find(|v| v.name.as_normalized() == "__archspec")
            .expect("__archspec present");
        assert_eq!(archspec.version.to_string(), "1");
        assert_eq!(archspec.build_string, "skylake_avx512");
    }

    #[test]
    fn archspec_override_applies_cross_platform() {
        // The archspec override is respected even when cross-compiling.
        let mut overrides = BTreeMap::new();
        overrides.insert("archspec".to_string(), "zen3".to_string());
        let p = Platform::from_str("win-64").expect("valid platform");
        let vps = virtual_packages(p, &overrides).expect("builds vpkgs");
        let archspec = vps
            .iter()
            .find(|v| v.name.as_normalized() == "__archspec")
            .expect("__archspec present");
        assert_eq!(archspec.version.to_string(), "1");
        assert_eq!(archspec.build_string, "zen3");
        // the windows baseline is still present alongside the override
        assert!(vps.iter().any(|v| v.name.as_normalized() == "__win"));
    }

    #[test]
    fn custom_virtual_package_uses_version_or_build_heuristic() {
        // A name rattler doesn't model falls back to the heuristic: a
        // version-like value → version (build "0"); a value that is not a valid
        // conda version → build string with version "1".
        let mut overrides = BTreeMap::new();
        overrides.insert("__foo".to_string(), "2.5".to_string());
        overrides.insert("bar".to_string(), "needs space".to_string());
        let vps = virtual_packages(Platform::current(), &overrides).expect("builds vpkgs");

        let foo = vps
            .iter()
            .find(|v| v.name.as_normalized() == "__foo")
            .expect("__foo present");
        assert_eq!(foo.version.to_string(), "2.5");
        assert_eq!(foo.build_string, "0");

        let bar = vps
            .iter()
            .find(|v| v.name.as_normalized() == "__bar")
            .expect("__bar present");
        assert_eq!(bar.version.to_string(), "1");
        assert_eq!(bar.build_string, "needs space");
    }

    #[test]
    fn channel_alias_resolves_bare_names() {
        let settings = ChannelSettings::with_alias("https://artifacts.example.com/api/conda");
        // bare name gets the alias prefixed
        assert_eq!(
            resolve_channel_url("remote-repos-conda-forge", &settings),
            "https://artifacts.example.com/api/conda/remote-repos-conda-forge"
        );
        // an explicit URL is left alone
        assert_eq!(
            resolve_channel_url("https://conda.anaconda.org/conda-forge", &settings),
            "https://conda.anaconda.org/conda-forge"
        );
    }

    #[test]
    fn mirror_overrides_public_conda_forge() {
        let settings = ChannelSettings::default().mirror(
            "conda-forge",
            "https://artifacts.example.com/api/conda/remote-repos-conda-forge",
        );
        // the public name is rewritten to the mirror url
        assert_eq!(
            resolve_channel_url("conda-forge", &settings),
            "https://artifacts.example.com/api/conda/remote-repos-conda-forge"
        );
        // an unmirrored channel is unchanged
        assert_eq!(resolve_channel_url("bioconda", &settings), "bioconda");
    }

    #[test]
    fn mirror_then_alias_compose() {
        // mirror maps the public name to a bare internal name, alias makes it a url
        let settings = ChannelSettings::with_alias("https://artifacts.example.com/api/conda")
            .mirror("conda-forge", "remote-repos-conda-forge");
        assert_eq!(
            resolve_channel_url("conda-forge", &settings),
            "https://artifacts.example.com/api/conda/remote-repos-conda-forge"
        );
    }

    #[test]
    fn resolve_method_points_channels_at_artifactory() {
        // Override every channel to an internal Artifactory: bare names resolve
        // against the alias, and `conda-forge` keeps its identity while being
        // mirrored to the internal `conda-forge-mirror`.
        let settings =
            ChannelSettings::with_alias("https://artifactory.mycompany.net/artifactory/api/conda")
                .mirror("conda-forge", "conda-forge-mirror");
        assert_eq!(
            settings.resolve("conda-forge"),
            "https://artifactory.mycompany.net/artifactory/api/conda/conda-forge-mirror"
        );
        assert_eq!(
            settings.resolve("my-custom-channel"),
            "https://artifactory.mycompany.net/artifactory/api/conda/my-custom-channel"
        );
        // an already-resolved URL is left untouched
        assert_eq!(
            settings.resolve("https://conda.anaconda.org/conda-forge"),
            "https://conda.anaconda.org/conda-forge"
        );
    }

    #[test]
    fn channel_settings_derived_from_manifest_yaml() {
        use crate::manifest::Manifest;
        // A manifest that declares its channels entirely in YAML: an alias for
        // bare internal names, plus a mirror redirecting public conda-forge.
        let yaml = r#"
project:
  name: myenv
  channel-alias: https://artifacts.example.com/api/conda
  channels: [dept-myenv-conda-published-local, conda-forge]
channels:
  conda-forge:
    mirror: remote-repos-conda-forge
    priority: 10
environments:
  app: []
"#;
        let m = Manifest::from_yaml_str(yaml).expect("parses");
        let settings = ChannelSettings::from_manifest(&m);

        // alias carried over
        assert_eq!(
            settings.channel_alias.as_deref(),
            Some("https://artifacts.example.com/api/conda")
        );
        // the conda-forge mirror is recorded
        assert_eq!(
            settings.mirrors.get("conda-forge").map(String::as_str),
            Some("remote-repos-conda-forge")
        );

        // end-to-end: the priority-ordered channel list resolves to real URLs
        let urls: Vec<String> = m
            .project
            .channels
            .iter()
            .map(|c| resolve_channel_url(c, &settings))
            .collect();
        assert_eq!(
            urls,
            [
                "https://artifacts.example.com/api/conda/dept-myenv-conda-published-local",
                "https://artifacts.example.com/api/conda/remote-repos-conda-forge",
            ]
        );
    }

    #[test]
    fn channel_url_in_yaml_is_used_directly() {
        use crate::manifest::Manifest;
        let yaml = r#"
project:
  name: p
  channels: [custom]
channels:
  custom:
    url: https://example.com/private-channel
environments:
  app: []
"#;
        let m = Manifest::from_yaml_str(yaml).expect("parses");
        let settings = ChannelSettings::from_manifest(&m);
        assert_eq!(
            resolve_channel_url("custom", &settings),
            "https://example.com/private-channel"
        );
    }

    /// Real network solve against public conda-forge. Ignored by default so CI
    /// stays offline; run manually with `cargo test -- --ignored`.
    #[ignore = "requires network access to conda-forge"]
    #[tokio::test]
    async fn real_solve_python_from_conda_forge() {
        let request = SolveRequest {
            channels: vec!["conda-forge".to_string()],
            platform: "linux-64".to_string(),
            specs: vec!["python 3.11.*".to_string()],
            ..Default::default()
        };
        let outcome = solve(&request, &ChannelSettings::default())
            .await
            .expect("solve should succeed");
        assert!(
            outcome.packages().iter().any(|p| p.name == "python"),
            "solved set should contain python"
        );
        assert!(
            outcome
                .packages()
                .iter()
                .any(|p| p.name == "libzlib" || p.name == "openssl"),
            "solved set should pull transitive deps"
        );
        // provenance is recorded
        assert_eq!(outcome.platform, "linux-64");
        assert_eq!(outcome.channels, ["conda-forge"]);
    }

    /// Real network solve against a conda mirror, reached two ways: (1) a bare
    /// channel name resolved via `channel_alias`, and (2) the public
    /// `conda-forge` name redirected through a `mirror`. Ignored by default;
    /// point it at a mirror and run manually with
    /// `NEPENTHE_TEST_CONDA_ALIAS=https://my-mirror/api/conda cargo test -- --ignored`.
    /// `NEPENTHE_TEST_CONDA_REPO` names the conda-forge repo under that alias
    /// (default `conda-forge`).
    #[ignore = "requires network access to a conda mirror (set NEPENTHE_TEST_CONDA_ALIAS)"]
    #[tokio::test]
    async fn real_solve_python_from_mirror() {
        let Ok(alias) = std::env::var("NEPENTHE_TEST_CONDA_ALIAS") else {
            eprintln!("skipping: set NEPENTHE_TEST_CONDA_ALIAS to a conda mirror base URL to run");
            return;
        };
        let repo =
            std::env::var("NEPENTHE_TEST_CONDA_REPO").unwrap_or_else(|_| "conda-forge".to_string());

        // (1) bare name + alias
        let via_alias = solve(
            &SolveRequest {
                channels: vec![repo.clone()],
                platform: "linux-64".to_string(),
                specs: vec!["python 3.11.*".to_string()],
                ..Default::default()
            },
            &ChannelSettings::with_alias(&alias),
        )
        .await
        .expect("alias solve should succeed");
        assert!(via_alias.packages().iter().any(|p| p.name == "python"));

        // (2) public conda-forge name redirected to the mirror
        let via_mirror = solve(
            &SolveRequest {
                channels: vec!["conda-forge".to_string()],
                platform: "linux-64".to_string(),
                specs: vec!["python 3.11.*".to_string()],
                ..Default::default()
            },
            &ChannelSettings::default().mirror("conda-forge", format!("{alias}/{repo}")),
        )
        .await
        .expect("mirror solve should succeed");
        assert!(via_mirror.packages().iter().any(|p| p.name == "python"));
    }
}
