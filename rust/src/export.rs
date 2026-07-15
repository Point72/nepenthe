//! Exports for a solved environment ([`SolveOutcome`](crate::solve::SolveOutcome)):
//! the modern `rattler_lock` lockfile (the primary, round-trippable artifact)
//! plus the compatibility formats `@EXPLICIT` (installable via `conda create
//! --file`) and `environment.yml`.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use rattler_conda_types::{PackageRecord, Platform, RepoDataRecord};
use rattler_lock::{LockFile, LockFileBuilder, ParseCondaLockError, PlatformData, PlatformName};

use crate::solve::SolveOutcome;

/// The default environment name used in a lockfile produced from a single
/// [`SolveOutcome`].
pub const DEFAULT_ENVIRONMENT: &str = "default";

/// Errors raised while exporting a solved environment.
#[derive(Debug)]
pub enum ExportError {
    /// A platform string could not be parsed.
    Platform(String),
    /// Building the lockfile failed.
    Lock(ParseCondaLockError),
    /// Rendering the lockfile to a string failed.
    Render(std::io::Error),
    /// Serialising the environment.yml document failed.
    Serialize(serde_yaml::Error),
    /// A multi-platform export was given no solve outcomes.
    Empty,
    /// A multi-platform export was given two outcomes for the same platform.
    DuplicatePlatform(String),
    /// Composing locks found a package pinned to different versions.
    ComposeConflict {
        /// The package name that conflicts.
        package: String,
        /// The platform on which the conflict was found.
        platform: String,
        /// The first input's pin (`version=build`).
        left: String,
        /// The conflicting input's pin (`version=build`).
        right: String,
    },
}

impl fmt::Display for ExportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExportError::Platform(p) => write!(f, "invalid platform '{p}'"),
            ExportError::Lock(e) => write!(f, "failed to build lockfile: {e}"),
            ExportError::Render(e) => write!(f, "failed to render lockfile: {e}"),
            ExportError::Serialize(e) => write!(f, "failed to serialise environment.yml: {e}"),
            ExportError::Empty => write!(f, "no solve outcomes to export"),
            ExportError::DuplicatePlatform(p) => {
                write!(f, "two solve outcomes for the same platform '{p}'")
            }
            ExportError::ComposeConflict {
                package,
                platform,
                left,
                right,
            } => write!(
                f,
                "cannot compose: '{package}' is pinned to {left} and {right} on {platform}"
            ),
        }
    }
}

impl std::error::Error for ExportError {}

/// Render a set of solved conda records as a `conda list --explicit`
/// (`@EXPLICIT`) spec: a header line followed by each package's download URL.
/// The records are **topologically sorted** so a package's dependencies are
/// listed before it, the order `conda create --file` expects. This is the
/// format that `conda create --file` consume.
pub fn to_explicit_records(records: &[RepoDataRecord]) -> String {
    let sorted = PackageRecord::sort_topologically(records.to_vec());
    let mut out = String::from("@EXPLICIT\n");
    for record in &sorted {
        out.push_str(record.url.as_str());
        out.push('\n');
    }
    out
}

/// Render a solved environment as an `@EXPLICIT` spec. Convenience wrapper over
/// [`to_explicit_records`] for a [`SolveOutcome`].
pub fn to_explicit(outcome: &SolveOutcome) -> String {
    to_explicit_records(&outcome.records)
}

/// Render a solved environment as an `environment.yml`: the environment name,
/// its channels, and a `name=version=build` pin for every package. Built via
/// serde so unusual names, channels, and pins are correctly quoted/escaped.
pub fn to_environment_yml(outcome: &SolveOutcome, name: &str) -> Result<String, ExportError> {
    #[derive(serde::Serialize)]
    struct EnvYml<'a> {
        name: &'a str,
        channels: &'a [String],
        dependencies: Vec<String>,
    }
    let dependencies = outcome
        .records
        .iter()
        .map(|record| {
            let pr = &record.package_record;
            format!(
                "{}={}={}",
                pr.name.as_normalized(),
                pr.version.as_str(),
                pr.build
            )
        })
        .collect();
    let doc = EnvYml {
        name,
        channels: &outcome.channels,
        dependencies,
    };
    serde_yaml::to_string(&doc).map_err(ExportError::Serialize)
}

/// Build a [`rattler_lock::LockFile`] for a solved environment under the
/// `environment` name. The lock records the environment's channels, target
/// platform, and every solved conda package. It round-trips through
/// [`LockFile::render_to_string`] / [`LockFile::from_str_with_base_directory`].
pub fn to_lockfile(outcome: &SolveOutcome, environment: &str) -> Result<LockFile, ExportError> {
    to_multi_platform_lockfile(std::slice::from_ref(outcome), environment)
}

/// Build a single multi-platform [`rattler_lock::LockFile`] for one environment
/// from one [`SolveOutcome`] per platform. Every outcome must target a distinct
/// platform; all are registered under the same `environment`, so one lock can
/// be installed on any of them (`linux-64`, `osx-arm64`, `win-64`, …). This is
/// what makes a published lock multi-platform by design.
pub fn to_multi_platform_lockfile(
    outcomes: &[SolveOutcome],
    environment: &str,
) -> Result<LockFile, ExportError> {
    if outcomes.is_empty() {
        return Err(ExportError::Empty);
    }

    let mut platforms = Vec::with_capacity(outcomes.len());
    let mut seen = std::collections::BTreeSet::new();
    for outcome in outcomes {
        if !seen.insert(outcome.platform.clone()) {
            return Err(ExportError::DuplicatePlatform(outcome.platform.clone()));
        }
        let subdir = Platform::from_str(&outcome.platform)
            .map_err(|_| ExportError::Platform(outcome.platform.clone()))?;
        let name = PlatformName::try_from(outcome.platform.as_str())
            .map_err(|_| ExportError::Platform(outcome.platform.clone()))?;
        platforms.push(PlatformData {
            name,
            subdir,
            virtual_packages: outcome.virtual_packages.clone(),
        });
    }

    // Channels are per-environment in the lock; they are consistent across an
    // environment's platforms, so take them from the first outcome.
    let mut builder = LockFileBuilder::new()
        .with_platforms(platforms)
        .map_err(ExportError::Lock)?
        .with_channels(environment, outcomes[0].channels.clone());

    for outcome in outcomes {
        for record in &outcome.records {
            builder = builder
                .with_conda_package(
                    environment,
                    &outcome.platform,
                    RepoDataRecord::clone(record).into(),
                )
                .map_err(ExportError::Lock)?;
        }
    }

    Ok(builder.finish())
}

/// Build a lockfile and render it to its YAML string representation.
pub fn to_lockfile_string(
    outcome: &SolveOutcome,
    environment: &str,
) -> Result<String, ExportError> {
    to_lockfile(outcome, environment)?
        .render_to_string()
        .map_err(ExportError::Render)
}

/// Combine the matrix output of
/// [`solve_environment`](crate::solve::solve_environment) into one
/// multi-platform lock **per build cell**. The `(Selector, platform, outcome)`
/// rows are grouped by their `(variant, python)` selector, and each cell's
/// per-platform outcomes are merged into a single multi-platform lock — so a
/// `cpu` and a `gpu` variant each get their own lock spanning every platform.
/// Cell order is preserved from the input.
pub fn matrix_to_lockfiles(
    matrix: &[(crate::manifest::Selector, String, SolveOutcome)],
    environment: &str,
) -> Result<Vec<(crate::manifest::Selector, LockFile)>, ExportError> {
    // Group outcomes by selector, preserving first-seen order (Selector is not
    // Hash/Ord, so use a Vec of buckets).
    let mut cells: Vec<(crate::manifest::Selector, Vec<SolveOutcome>)> = Vec::new();
    for (selector, _platform, outcome) in matrix {
        match cells.iter_mut().find(|(s, _)| s == selector) {
            Some((_, outcomes)) => outcomes.push(outcome.clone()),
            None => cells.push((selector.clone(), vec![outcome.clone()])),
        }
    }

    cells
        .into_iter()
        .map(|(selector, outcomes)| {
            to_multi_platform_lockfile(&outcomes, environment).map(|lock| (selector, lock))
        })
        .collect()
}

/// Compose several published locks into one multi-platform lock under `out_env`.
///
/// Each input is a `(lock, environment-name)` pair (the environment whose
/// packages to take from that lock). Conda packages are **unioned by name**
/// across all inputs, for every platform **common to all of them**; the first
/// input to provide a package wins, and a package pinned to a *different*
/// version/build in a later input is a [`ExportError::ComposeConflict`].
/// Channels are unioned in input order. This lifts manifest-level feature
/// composition to the published-artifact level (e.g. `base ∪ ml-extras`).
pub fn compose_lockfiles(
    inputs: &[(LockFile, String)],
    out_env: &str,
) -> Result<LockFile, ExportError> {
    if inputs.is_empty() {
        return Err(ExportError::Empty);
    }

    let mut envs = Vec::with_capacity(inputs.len());
    for (lock, name) in inputs {
        let env = lock
            .environment(name)
            .ok_or_else(|| ExportError::Platform(format!("lock has no environment '{name}'")))?;
        envs.push(env);
    }

    // Common platforms = the intersection of every input's platforms.
    let mut common: Vec<Platform> = envs[0].platforms().map(|p| p.subdir()).collect();
    for env in &envs[1..] {
        let here: std::collections::BTreeSet<Platform> =
            env.platforms().map(|p| p.subdir()).collect();
        common.retain(|p| here.contains(p));
    }
    common.sort();
    common.dedup();
    if common.is_empty() {
        return Err(ExportError::Empty);
    }

    // Channels: union in input order, deduplicated by their rendered form.
    let mut channels = Vec::new();
    let mut seen_channels = std::collections::BTreeSet::new();
    for env in &envs {
        for channel in env.channels() {
            if seen_channels.insert(format!("{channel:?}")) {
                channels.push(channel.clone());
            }
        }
    }

    // Platform metadata (virtual packages are empty: provenance lives in the
    // records, which carry their own URLs and hashes).
    let mut platform_data = Vec::with_capacity(common.len());
    for subdir in &common {
        let name = PlatformName::try_from(subdir.as_str())
            .map_err(|_| ExportError::Platform(subdir.to_string()))?;
        platform_data.push(PlatformData {
            name,
            subdir: *subdir,
            virtual_packages: Vec::new(),
        });
    }

    let mut builder = LockFileBuilder::new()
        .with_platforms(platform_data)
        .map_err(ExportError::Lock)?
        .with_channels(out_env, channels);

    for subdir in &common {
        let platform_str = subdir.to_string();
        // Union records by name across inputs, in input order; first wins.
        let mut chosen: BTreeMap<String, RepoDataRecord> = BTreeMap::new();
        for env in &envs {
            let Some(handle) = env.platforms().find(|p| p.subdir() == *subdir) else {
                continue;
            };
            let records = env
                .conda_repodata_records(handle)
                .map_err(|e| ExportError::Platform(format!("converting lock records: {e}")))?
                .unwrap_or_default();
            for record in records {
                let name = record.package_record.name.as_normalized().to_string();
                match chosen.get(&name) {
                    Some(existing) => {
                        let ex = &existing.package_record;
                        let new = &record.package_record;
                        if ex.version != new.version || ex.build != new.build {
                            return Err(ExportError::ComposeConflict {
                                package: name,
                                platform: platform_str.clone(),
                                left: format!("{}={}", ex.version, ex.build),
                                right: format!("{}={}", new.version, new.build),
                            });
                        }
                    }
                    None => {
                        chosen.insert(name, record);
                    }
                }
            }
        }
        for record in chosen.into_values() {
            builder = builder
                .with_conda_package(out_env, &platform_str, record.into())
                .map_err(ExportError::Lock)?;
        }
    }

    Ok(builder.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::solve::ChannelPriorityMode;
    use rattler_conda_types::package::DistArchiveIdentifier;
    use rattler_conda_types::{PackageName, PackageRecord, RepoDataRecord, VersionWithSource};
    use std::str::FromStr;
    use url::Url;

    fn record(name: &str, version: &str, build: &str, url: &str) -> RepoDataRecord {
        let mut pr = PackageRecord::new(
            PackageName::from_str(name).unwrap(),
            VersionWithSource::from_str(version).unwrap(),
            build.to_string(),
        );
        pr.subdir = "linux-64".to_string();
        let file_name = format!("{name}-{version}-{build}.conda");
        RepoDataRecord {
            package_record: pr,
            identifier: file_name.parse::<DistArchiveIdentifier>().unwrap(),
            url: Url::parse(url).unwrap(),
            channel: Some("https://example.com/conda-forge".to_string()),
        }
    }

    fn outcome() -> SolveOutcome {
        let python = record(
            "python",
            "3.11.14",
            "h0_cpython",
            "https://example.com/conda-forge/linux-64/python-3.11.14-h0_cpython.conda",
        );
        let mut numpy = record(
            "numpy",
            "2.1.0",
            "py311h0",
            "https://example.com/conda-forge/linux-64/numpy-2.1.0-py311h0.conda",
        );
        // numpy depends on python, so a topological @EXPLICIT lists python first.
        numpy.package_record.depends = vec!["python".to_string()];
        SolveOutcome {
            records: vec![python, numpy],
            channels: vec!["https://example.com/conda-forge".to_string()],
            platform: "linux-64".to_string(),
            virtual_packages: vec!["__cuda=12.9=0".to_string()],
            channel_priority: ChannelPriorityMode::Disabled,
            exclude_newer: None,
        }
    }

    #[test]
    fn explicit_lists_urls_after_header() {
        let text = to_explicit(&outcome());
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "@EXPLICIT");
        assert!(lines[1].ends_with("python-3.11.14-h0_cpython.conda"));
        assert!(lines[2].ends_with("numpy-2.1.0-py311h0.conda"));
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn explicit_records_are_topologically_sorted() {
        // Records given dependents-first must come out dependencies-first.
        let python = record(
            "python",
            "3.11.14",
            "h0_cpython",
            "https://example.com/conda-forge/linux-64/python-3.11.14-h0_cpython.conda",
        );
        let mut numpy = record(
            "numpy",
            "2.1.0",
            "py311h0",
            "https://example.com/conda-forge/linux-64/numpy-2.1.0-py311h0.conda",
        );
        numpy.package_record.depends = vec!["python".to_string()];

        let text = to_explicit_records(&[numpy, python]);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "@EXPLICIT");
        // python is a dependency of numpy, so it must be listed first.
        assert!(lines[1].ends_with("python-3.11.14-h0_cpython.conda"));
        assert!(lines[2].ends_with("numpy-2.1.0-py311h0.conda"));
    }

    #[test]
    fn environment_yml_has_name_channels_and_pins() {
        let yml = to_environment_yml(&outcome(), "myenv").expect("serializes");
        // Parse back and assert structurally so formatting/escaping is robust.
        let doc: serde_yaml::Value = serde_yaml::from_str(&yml).expect("valid yaml");
        assert_eq!(doc["name"], serde_yaml::Value::from("myenv"));
        let channels = doc["channels"].as_sequence().expect("channels seq");
        assert!(channels
            .iter()
            .any(|c| c.as_str() == Some("https://example.com/conda-forge")));
        let deps = doc["dependencies"].as_sequence().expect("deps seq");
        assert!(deps
            .iter()
            .any(|d| d.as_str() == Some("python=3.11.14=h0_cpython")));
        assert!(deps
            .iter()
            .any(|d| d.as_str() == Some("numpy=2.1.0=py311h0")));
    }

    #[test]
    fn lockfile_round_trips() {
        let lock = to_lockfile(&outcome(), "default").expect("builds lock");
        let yaml = lock.render_to_string().expect("renders");
        let reparsed = LockFile::from_str_with_base_directory(&yaml, None).expect("reparses lock");
        // LockFile has no PartialEq; round-trip by comparing the rendered YAML.
        assert_eq!(yaml, reparsed.render_to_string().expect("re-renders"));

        // the lock carries both solved packages on the default environment
        assert!(reparsed.environment("default").is_some());
        assert!(yaml.contains("python"));
        assert!(yaml.contains("numpy"));
    }

    /// A `SolveOutcome` for `platform` carrying one package built for it.
    fn outcome_for(platform: &str) -> SolveOutcome {
        let mut pr = PackageRecord::new(
            PackageName::from_str("python").unwrap(),
            VersionWithSource::from_str("3.11.14").unwrap(),
            "h0".to_string(),
        );
        pr.subdir = platform.to_string();
        let rec = RepoDataRecord {
            package_record: pr,
            identifier: "python-3.11.14-h0.conda"
                .parse::<DistArchiveIdentifier>()
                .unwrap(),
            url: Url::parse(&format!(
                "https://example.com/conda-forge/{platform}/python-3.11.14-h0.conda"
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

    #[test]
    fn multi_platform_lock_registers_every_platform() {
        let outcomes = [outcome_for("linux-64"), outcome_for("osx-arm64")];
        let lock = to_multi_platform_lockfile(&outcomes, "app").expect("builds lock");
        let yaml = lock.render_to_string().expect("renders");
        let reparsed = LockFile::from_str_with_base_directory(&yaml, None).expect("reparses");

        let env = reparsed.environment("app").expect("env present");
        let platforms: std::collections::BTreeSet<String> =
            env.platforms().map(|p| p.subdir().to_string()).collect();
        assert!(platforms.contains("linux-64"), "got {platforms:?}");
        assert!(platforms.contains("osx-arm64"), "got {platforms:?}");

        // each platform carries its own python record
        for platform in ["linux-64", "osx-arm64"] {
            let subdir = Platform::from_str(platform).unwrap();
            let handle = env.platforms().find(|p| p.subdir() == subdir).unwrap();
            let records = env.conda_repodata_records(handle).unwrap().unwrap();
            assert!(records
                .iter()
                .any(|r| r.package_record.name.as_normalized() == "python"));
        }
    }

    #[test]
    fn multi_platform_lock_rejects_empty_and_duplicate() {
        assert!(matches!(
            to_multi_platform_lockfile(&[], "app"),
            Err(ExportError::Empty)
        ));
        let dup = [outcome_for("linux-64"), outcome_for("linux-64")];
        assert!(matches!(
            to_multi_platform_lockfile(&dup, "app"),
            Err(ExportError::DuplicatePlatform(p)) if p == "linux-64"
        ));
    }

    /// A single-package lock for `env` on linux-64 carrying `pkg`.
    fn lock_with(env: &str, pkg: RepoDataRecord) -> LockFile {
        let outcome = SolveOutcome {
            records: vec![pkg],
            channels: vec!["https://example.com/conda-forge".to_string()],
            platform: "linux-64".to_string(),
            virtual_packages: vec![],
            channel_priority: ChannelPriorityMode::Disabled,
            exclude_newer: None,
        };
        to_lockfile(&outcome, env).expect("builds lock")
    }

    #[test]
    fn compose_unions_packages_across_locks() {
        let base = lock_with(
            "base",
            record(
                "numpy",
                "2.1.0",
                "py311h0",
                "https://example.com/conda-forge/linux-64/numpy-2.1.0-py311h0.conda",
            ),
        );
        let extras = lock_with(
            "extras",
            record(
                "click",
                "8.1.7",
                "py311h0",
                "https://example.com/conda-forge/linux-64/click-8.1.7-py311h0.conda",
            ),
        );
        let composed = compose_lockfiles(
            &[(base, "base".to_string()), (extras, "extras".to_string())],
            "combined",
        )
        .expect("composes");

        let env = composed.environment("combined").expect("env present");
        let subdir = Platform::from_str("linux-64").unwrap();
        let handle = env.platforms().find(|p| p.subdir() == subdir).unwrap();
        let names: std::collections::BTreeSet<String> = env
            .conda_repodata_records(handle)
            .unwrap()
            .unwrap()
            .iter()
            .map(|r| r.package_record.name.as_normalized().to_string())
            .collect();
        assert!(names.contains("numpy"), "got {names:?}");
        assert!(names.contains("click"), "got {names:?}");
    }

    #[test]
    fn compose_detects_conflicting_pins() {
        let a = lock_with(
            "a",
            record(
                "numpy",
                "2.1.0",
                "py311h0",
                "https://example.com/conda-forge/linux-64/numpy-2.1.0-py311h0.conda",
            ),
        );
        let b = lock_with(
            "b",
            record(
                "numpy",
                "2.2.0",
                "py311h0",
                "https://example.com/conda-forge/linux-64/numpy-2.2.0-py311h0.conda",
            ),
        );
        assert!(matches!(
            compose_lockfiles(&[(a, "a".to_string()), (b, "b".to_string())], "x"),
            Err(ExportError::ComposeConflict { package, .. }) if package == "numpy"
        ));
    }

    #[test]
    fn matrix_to_lockfiles_groups_by_cell() {
        use crate::manifest::Selector;
        // two cells (cpu, gpu), each solved for two platforms.
        let cpu = Selector::variant("cpu");
        let gpu = Selector::variant("gpu");
        let matrix = vec![
            (cpu.clone(), "linux-64".to_string(), outcome_for("linux-64")),
            (
                cpu.clone(),
                "osx-arm64".to_string(),
                outcome_for("osx-arm64"),
            ),
            (gpu.clone(), "linux-64".to_string(), outcome_for("linux-64")),
            (
                gpu.clone(),
                "osx-arm64".to_string(),
                outcome_for("osx-arm64"),
            ),
        ];
        let locks = matrix_to_lockfiles(&matrix, "app").expect("builds locks");
        // one lock per cell, in input order
        assert_eq!(locks.len(), 2);
        assert_eq!(locks[0].0, cpu);
        assert_eq!(locks[1].0, gpu);
        // each cell's lock spans both platforms
        for (_selector, lock) in &locks {
            let env = lock.environment("app").expect("env");
            let platforms: std::collections::BTreeSet<String> =
                env.platforms().map(|p| p.subdir().to_string()).collect();
            assert_eq!(platforms.len(), 2, "got {platforms:?}");
        }
    }

    /// Solve python live and exercise every exporter end to end. Ignored by
    /// default so CI stays offline; run with `cargo test -- --ignored`.
    #[ignore = "requires network access to conda-forge"]
    #[tokio::test]
    async fn real_export_python_all_formats() {
        use crate::solve::{solve, ChannelSettings, SolveRequest};

        let outcome = solve(
            &SolveRequest {
                channels: vec!["conda-forge".to_string()],
                platform: "linux-64".to_string(),
                specs: vec!["python 3.11.*".to_string()],
                ..Default::default()
            },
            &ChannelSettings::default(),
        )
        .await
        .expect("live solve should succeed");

        // @EXPLICIT: header then one fully-qualified URL per package.
        let explicit = to_explicit(&outcome);
        let lines: Vec<&str> = explicit.lines().collect();
        assert_eq!(lines[0], "@EXPLICIT");
        assert!(lines.len() > 1, "expected solved packages");
        for url in &lines[1..] {
            assert!(url.starts_with("https://"), "explicit url: {url}");
            assert!(
                url.ends_with(".conda") || url.ends_with(".tar.bz2"),
                "explicit url: {url}"
            );
        }
        assert!(
            lines[1..].iter().any(|u| u.contains("/python-3.11")),
            "explicit should pin python 3.11"
        );

        // environment.yml: name, channels, and exact name=version=build pins.
        let yml = to_environment_yml(&outcome, "live").expect("serializes");
        let doc: serde_yaml::Value = serde_yaml::from_str(&yml).expect("valid yaml");
        assert_eq!(doc["name"], serde_yaml::Value::from("live"));
        assert!(doc["dependencies"]
            .as_sequence()
            .expect("deps seq")
            .iter()
            .any(|d| d.as_str().is_some_and(|s| s.starts_with("python=3.11"))));

        // lockfile: builds and round-trips.
        let lock_yaml = to_lockfile_string(&outcome, "default").expect("renders lock");
        let reparsed =
            LockFile::from_str_with_base_directory(&lock_yaml, None).expect("reparses lock");
        assert_eq!(
            lock_yaml,
            reparsed.render_to_string().expect("re-renders lock")
        );
        assert!(reparsed.environment("default").is_some());
    }
}
