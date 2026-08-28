use std::path::PathBuf;
use std::str::FromStr;

use pyo3::exceptions::{PyIOError, PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyList};

use fsspec_rs_bridge::PyFsspecFs;
use nepenthe_core::backend::fsspec_rs::{FileSystem, FsError};
use nepenthe_core::backend::SpecStore;
use nepenthe_core::install::{self, InstallDiff, InstallSummary, PrefixStatus};
use nepenthe_core::manifest::{Manifest, Selector};
use nepenthe_core::pack::PackSummary;
use nepenthe_core::producer::{self, BuildError, BuildRequest, BuiltCell};
use nepenthe_core::project::{self, CheckReport, DependencyStatus};
use nepenthe_core::registry::{Coordinates, Index, Label, Registry, Release};
use nepenthe_core::solve::{ChannelPriorityMode, ChannelSettings, SolveError};

/// Version of the underlying `nepenthe-core` crate.
#[pyfunction]
fn version() -> &'static str {
    nepenthe_core::version()
}

fn fs_to_py(e: FsError) -> PyErr {
    PyIOError::new_err(e.to_string())
}

/// Pull the bytes of a spec from `path` through a user-supplied Python fsspec
/// object, by way of the [`fsspec_rs_bridge`] [`FileSystem`] adapter.
#[pyfunction]
fn fsspec_pull<'py>(py: Python<'py>, fs: Py<PyAny>, path: &str) -> PyResult<Bound<'py, PyBytes>> {
    let adapter = PyFsspecFs::from_py_fs(fs).map_err(fs_to_py)?;
    let bytes = adapter.cat_file(path, None, None).map_err(fs_to_py)?;
    Ok(PyBytes::new(py, &bytes))
}

/// Publish the bytes of a spec to `path` through a user-supplied Python fsspec
/// object, by way of the [`fsspec_rs_bridge`] [`FileSystem`] adapter.
#[pyfunction]
fn fsspec_publish(fs: Py<PyAny>, path: &str, data: &[u8]) -> PyResult<()> {
    let adapter = PyFsspecFs::from_py_fs(fs).map_err(fs_to_py)?;
    adapter.pipe_file(path, data).map_err(fs_to_py)
}

/// Map any core error into a Python `RuntimeError`.
fn err<E: std::fmt::Display>(e: E) -> PyErr {
    PyRuntimeError::new_err(e.to_string())
}

/// Build a short-lived multi-threaded tokio runtime and block on `fut`,
/// releasing the GIL so other Python threads can run during the (network) wait.
fn block_on<F>(py: Python<'_>, fut: F) -> PyResult<F::Output>
where
    F: std::future::Future + Send,
    F::Output: Send,
{
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| PyRuntimeError::new_err(format!("failed to start async runtime: {e}")))?;
    Ok(py.detach(|| runtime.block_on(fut)))
}

/// Assemble registry [`Coordinates`], defaulting the platform to the current one.
fn coordinates(
    environment: String,
    platform: Option<String>,
    python: Option<String>,
    variant: Option<String>,
) -> Coordinates {
    let platform = platform.unwrap_or_else(nepenthe_core::current_platform);
    let mut coords = Coordinates::new(environment, platform);
    if let Some(python) = python {
        coords = coords.with_python(python);
    }
    if let Some(variant) = variant {
        coords = coords.with_variant(variant);
    }
    coords
}

/// Convert a [`Release`] into a Python dict.
fn release_dict<'py>(py: Python<'py>, release: &Release) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    dict.set_item("environment", &release.environment)?;
    dict.set_item("platform", &release.platform)?;
    dict.set_item("python", release.python.clone())?;
    dict.set_item("variant", release.variant.clone())?;
    dict.set_item("version", &release.version)?;
    dict.set_item("lock", &release.lock)?;
    dict.set_item("manifest", release.manifest.clone())?;
    dict.set_item("created", &release.created)?;
    Ok(dict)
}

/// Convert an [`InstallSummary`] into a Python dict.
fn summary_dict<'py>(py: Python<'py>, summary: &InstallSummary) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    dict.set_item("prefix", summary.prefix.to_string_lossy().as_ref())?;
    dict.set_item("environment", &summary.environment)?;
    dict.set_item("platform", &summary.platform)?;
    let packages: Vec<String> = summary.packages.iter().map(|p| p.to_string()).collect();
    dict.set_item("packages", packages)?;
    Ok(dict)
}

/// Convert a [`PackSummary`] into a Python dict.
fn pack_summary_dict<'py>(py: Python<'py>, summary: &PackSummary) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    dict.set_item("output", summary.output.to_string_lossy().as_ref())?;
    dict.set_item("environment", &summary.environment)?;
    dict.set_item("platforms", summary.platforms.clone())?;
    dict.set_item("packages", summary.packages)?;
    dict.set_item("bytes", summary.bytes)?;
    Ok(dict)
}

/// Convert a project dependency-check report into a Python dict.
fn check_report_dict<'py>(py: Python<'py>, report: &CheckReport) -> PyResult<Bound<'py, PyDict>> {
    let dependencies = PyList::empty(py);
    for dep in &report.dependencies {
        let entry = PyDict::new(py);
        entry.set_item("requirement", &dep.requirement)?;
        match &dep.status {
            DependencyStatus::Satisfied { name, found } => {
                entry.set_item("status", "satisfied")?;
                entry.set_item("name", name)?;
                entry.set_item("found", found)?;
            }
            DependencyStatus::Conflict {
                name,
                specifier,
                found,
            } => {
                entry.set_item("status", "conflict")?;
                entry.set_item("name", name)?;
                entry.set_item("specifier", specifier)?;
                entry.set_item("found", found)?;
            }
            DependencyStatus::Missing { name } => {
                entry.set_item("status", "missing")?;
                entry.set_item("name", name)?;
            }
            DependencyStatus::Skipped { reason } => {
                entry.set_item("status", "skipped")?;
                entry.set_item("reason", reason)?;
            }
        }
        dependencies.append(entry)?;
    }

    let dict = PyDict::new(py);
    dict.set_item("dependencies", dependencies)?;
    dict.set_item("satisfied", report.satisfied())?;
    dict.set_item("conflicts", report.conflicts())?;
    dict.set_item("missing", report.missing())?;
    dict.set_item("skipped", report.skipped())?;
    dict.set_item("has_conflicts", report.has_conflicts())?;
    Ok(dict)
}

/// Convert a [`PrefixStatus`] into a Python dict.
fn status_dict<'py>(py: Python<'py>, status: &PrefixStatus) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    dict.set_item("prefix", status.prefix.to_string_lossy().as_ref())?;
    dict.set_item("exists", status.exists)?;
    let packages: Vec<String> = status.packages.iter().map(|p| p.to_string()).collect();
    dict.set_item("packages", packages)?;
    Ok(dict)
}

/// Convert an [`InstallDiff`] into a Python dict.
fn diff_dict<'py>(py: Python<'py>, diff: &InstallDiff) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    let added: Vec<String> = diff.added.iter().map(|p| p.to_string()).collect();
    let removed: Vec<String> = diff.removed.iter().map(|p| p.to_string()).collect();
    let changed: Vec<(String, String)> = diff
        .changed
        .iter()
        .map(|(have, want)| (have.to_string(), want.to_string()))
        .collect();
    dict.set_item("up_to_date", diff.is_empty())?;
    dict.set_item("added", added)?;
    dict.set_item("removed", removed)?;
    dict.set_item("changed", changed)?;
    Ok(dict)
}

/// Convert a [`BuiltCell`] into a Python dict.
fn cell_dict<'py>(py: Python<'py>, cell: &BuiltCell) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    dict.set_item("cell", &cell.stem)?;
    dict.set_item("variant", cell.selector.variant.clone())?;
    dict.set_item("python", cell.selector.python.clone())?;
    dict.set_item("platforms", cell.platforms.clone())?;
    dict.set_item(
        "lock_path",
        cell.lock_path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned()),
    )?;
    let releases = PyList::empty(py);
    for release in &cell.releases {
        releases.append(release_dict(py, release)?)?;
    }
    dict.set_item("releases", releases)?;
    Ok(dict)
}

/// Returns the current platform as a conda subdir string (e.g. `linux-64`).
#[pyfunction]
fn current_platform() -> String {
    nepenthe_core::current_platform()
}

/// Solve a manifest environment into one lock per build cell, optionally writing
/// the locks to `output_dir` and/or publishing them to `registry` under
/// Solve a manifest environment into one lock per build cell, optionally writing
/// the locks to `output_dir` and/or publishing them to `registry` under
/// `version`. `manifest` and `overrides` are each a local YAML path or a
/// spec-backend URL (`file://`, `s3://`, `https://`); a remote manifest must be
/// self-contained (no `imports`). Pass `python` and/or `variant` to build only
/// that matrix cell (e.g. `python="3.11"`) instead of the whole environment.
/// Returns one dict per built cell.
#[pyfunction]
#[pyo3(signature = (
    manifest,
    env,
    *,
    overrides = None,
    output_dir = None,
    registry = None,
    version = None,
    channel_priority = "strict",
    exclude_newer = None,
    python = None,
    variant = None,
))]
#[allow(clippy::too_many_arguments)]
fn build<'py>(
    py: Python<'py>,
    manifest: String,
    env: String,
    overrides: Option<String>,
    output_dir: Option<PathBuf>,
    registry: Option<String>,
    version: Option<String>,
    channel_priority: &str,
    exclude_newer: Option<String>,
    python: Option<String>,
    variant: Option<String>,
) -> PyResult<Bound<'py, PyList>> {
    let channel_priority = ChannelPriorityMode::from_str(channel_priority)
        .map_err(|e| PyValueError::new_err(e.to_string()))?;
    let request = BuildRequest {
        manifest,
        overrides,
        environment: env,
        output_dir,
        registry,
        version,
        channel_priority,
        exclude_newer,
        python,
        variant,
    };

    let cells = block_on(py, producer::build(&request))?.map_err(err)?;
    let list = PyList::empty(py);
    for cell in &cells {
        list.append(cell_dict(py, cell)?)?;
    }
    Ok(list)
}

/// Resolve a published lock and install it into `prefix` (no conda required).
/// Set `link_scripts` to run each package's `post-link` script (off by default).
/// Returns an install summary dict.
#[pyfunction]
#[pyo3(signature = (env, registry, prefix, *, platform = None, python = None, variant = None, label = "latest", link_scripts = false))]
#[allow(clippy::too_many_arguments)]
fn create<'py>(
    py: Python<'py>,
    env: String,
    registry: String,
    prefix: PathBuf,
    platform: Option<String>,
    python: Option<String>,
    variant: Option<String>,
    label: &str,
    link_scripts: bool,
) -> PyResult<Bound<'py, PyDict>> {
    let registry = Registry::new(SpecStore::new(), registry);
    let coords = coordinates(env, platform, python, variant);
    let label = Label::parse(label);
    let summary = block_on(
        py,
        install::create(
            &registry,
            &coords,
            &label,
            &prefix,
            install::LinkScripts::from(link_scripts),
        ),
    )?
    .map_err(err)?;
    summary_dict(py, &summary)
}

/// Download a published lock to `output` without installing it. Returns the
/// number of bytes written.
#[pyfunction]
#[pyo3(signature = (env, registry, output, *, platform = None, python = None, variant = None, label = "latest"))]
fn pull(
    env: String,
    registry: String,
    output: PathBuf,
    platform: Option<String>,
    python: Option<String>,
    variant: Option<String>,
    label: &str,
) -> PyResult<usize> {
    let registry = Registry::new(SpecStore::new(), registry);
    let coords = coordinates(env, platform, python, variant);
    let bytes = registry.pull(&coords, &Label::parse(label)).map_err(err)?;
    std::fs::write(&output, &bytes).map_err(err)?;
    Ok(bytes.len())
}

/// Recover the manifest a lock was solved from, as a YAML string.
///
/// Provide either `lock` (a lock file with an embedded comment band — works
/// offline) or `env` + `registry` (resolve a release and read its embedded band,
/// then its registry manifest sidecar). Raises if no manifest can be recovered.
#[pyfunction]
#[pyo3(signature = (*, lock = None, env = None, registry = None, platform = None, python = None, variant = None, label = "latest"))]
#[allow(clippy::too_many_arguments)]
fn manifest(
    lock: Option<PathBuf>,
    env: Option<String>,
    registry: Option<String>,
    platform: Option<String>,
    python: Option<String>,
    variant: Option<String>,
    label: &str,
) -> PyResult<String> {
    if let Some(lock_path) = lock {
        let bytes = std::fs::read(&lock_path).map_err(err)?;
        return nepenthe_core::embed::extract_manifest(&bytes)
            .map_err(err)?
            .ok_or_else(|| PyRuntimeError::new_err("lock has no embedded manifest band"));
    }
    let (Some(env), Some(registry_url)) = (env, registry) else {
        return Err(PyValueError::new_err(
            "pass lock=<file>, or env=<name> and registry=<url>",
        ));
    };
    let registry = Registry::new(SpecStore::new(), registry_url);
    let coords = coordinates(env, platform, python, variant);
    let label = Label::parse(label);
    let lock_bytes = registry.pull(&coords, &label).map_err(err)?;
    if let Some(yaml) = nepenthe_core::embed::extract_manifest(&lock_bytes).map_err(err)? {
        return Ok(yaml);
    }
    let bytes = registry
        .pull_manifest(&coords, &label)
        .map_err(err)?
        .ok_or_else(|| {
            PyRuntimeError::new_err("release has no manifest (no embedded band and no sidecar)")
        })?;
    String::from_utf8(bytes).map_err(err)
}

/// Publish a lock file to `registry` under `version`. Returns the release dict.
#[pyfunction]
#[pyo3(signature = (env, registry, version, lock, *, platform = None, python = None, variant = None))]
#[allow(clippy::too_many_arguments)]
fn publish<'py>(
    py: Python<'py>,
    env: String,
    registry: String,
    version: String,
    lock: PathBuf,
    platform: Option<String>,
    python: Option<String>,
    variant: Option<String>,
) -> PyResult<Bound<'py, PyDict>> {
    let registry = Registry::new(SpecStore::new(), registry);
    let coords = coordinates(env, platform, python, variant);
    let bytes = std::fs::read(&lock).map_err(err)?;
    // Validate the lock parses and contains the target env/platform before
    // creating an immutable release.
    let lockfile = install::parse_lock(&bytes).map_err(err)?;
    install::lock_records(&lockfile, &coords.environment, &coords.platform).map_err(err)?;
    let release = registry.publish(&coords, &version, &bytes).map_err(err)?;
    release_dict(py, &release)
}

/// Show the release a label resolves to. Returns the release dict.
#[pyfunction]
#[pyo3(signature = (env, registry, *, platform = None, python = None, variant = None, label = "latest"))]
fn show<'py>(
    py: Python<'py>,
    env: String,
    registry: String,
    platform: Option<String>,
    python: Option<String>,
    variant: Option<String>,
    label: &str,
) -> PyResult<Bound<'py, PyDict>> {
    let registry = Registry::new(SpecStore::new(), registry);
    let coords = coordinates(env, platform, python, variant);
    let release = registry
        .resolve(&coords, &Label::parse(label))
        .map_err(err)?;
    release_dict(py, &release)
}

/// Compare an installed `prefix` against a lock file. Returns a diff dict.
#[pyfunction]
#[pyo3(signature = (lock, env, prefix, *, platform = None))]
fn diff<'py>(
    py: Python<'py>,
    lock: PathBuf,
    env: String,
    prefix: PathBuf,
    platform: Option<String>,
) -> PyResult<Bound<'py, PyDict>> {
    let platform = platform.unwrap_or_else(nepenthe_core::current_platform);
    let bytes = std::fs::read(&lock).map_err(err)?;
    let lockfile = install::parse_lock(&bytes).map_err(err)?;
    let diff = install::diff(&lockfile, &env, &platform, &prefix).map_err(err)?;
    diff_dict(py, &diff)
}

/// Report what is installed in `prefix`. Returns a status dict.
#[pyfunction]
fn status<'py>(py: Python<'py>, prefix: PathBuf) -> PyResult<Bound<'py, PyDict>> {
    let status = install::status(&prefix).map_err(err)?;
    status_dict(py, &status)
}

/// Remove an environment prefix. A non-existent prefix is a no-op. Protected
/// paths (filesystem root, home, current directory) are always refused; set
/// `force` to remove a prefix that has no `conda-meta` marker.
#[pyfunction]
#[pyo3(signature = (prefix, *, force = false))]
fn remove(prefix: PathBuf, force: bool) -> PyResult<()> {
    install::remove_prefix(&prefix, force).map_err(err)
}

/// Render an activation script for `prefix`, targeting `shell` (or the current
/// shell when omitted) on `platform` (or the current platform when omitted).
#[pyfunction]
#[pyo3(signature = (prefix, *, platform = None, shell = None))]
fn activate(prefix: PathBuf, platform: Option<String>, shell: Option<String>) -> PyResult<String> {
    let platform = platform.unwrap_or_else(nepenthe_core::current_platform);
    install::activation_script_for(&prefix, shell.as_deref(), &platform).map_err(err)
}

/// Pack the packages a lock pins for `env` into a self-contained offline bundle
/// at `output`. When `platforms` is omitted, every platform the lock covers for
/// the environment is included. Returns a pack summary dict.
#[pyfunction]
#[pyo3(signature = (lock, env, output, *, platforms = None))]
fn pack<'py>(
    py: Python<'py>,
    lock: PathBuf,
    env: String,
    output: PathBuf,
    platforms: Option<Vec<String>>,
) -> PyResult<Bound<'py, PyDict>> {
    let lock_bytes = std::fs::read(&lock).map_err(err)?;
    let platforms = platforms.unwrap_or_default();
    let summary = block_on(
        py,
        nepenthe_core::pack::pack(&lock_bytes, &env, &platforms, &output),
    )?
    .map_err(err)?;
    pack_summary_dict(py, &summary)
}

/// Install an environment from a packed bundle into `prefix`, fully offline.
/// `env` defaults to the bundle's environment and `platform` to the current
/// platform. Set `link_scripts` to run each package's `post-link` script (off by
/// default). Returns an install summary dict.
#[pyfunction]
#[pyo3(signature = (pack, prefix, *, env = None, platform = None, stage_dir = None, link_scripts = false))]
fn unpack<'py>(
    py: Python<'py>,
    pack: PathBuf,
    prefix: PathBuf,
    env: Option<String>,
    platform: Option<String>,
    stage_dir: Option<PathBuf>,
    link_scripts: bool,
) -> PyResult<Bound<'py, PyDict>> {
    let summary = block_on(
        py,
        nepenthe_core::pack::install_pack(
            &pack,
            env.as_deref(),
            platform.as_deref(),
            &prefix,
            stage_dir.as_deref(),
            install::LinkScripts::from(link_scripts),
        ),
    )?
    .map_err(err)?;
    summary_dict(py, &summary)
}

/// Install the environment a project's `pyproject.toml` references in its
/// `[tool.nepenthe]` stanza. `project` defaults to `./pyproject.toml`. Set
/// `link_scripts` to run each package's `post-link` script (off by default).
/// Returns an install summary dict.
#[pyfunction]
#[pyo3(signature = (project = None, *, link_scripts = false))]
fn sync<'py>(
    py: Python<'py>,
    project: Option<PathBuf>,
    link_scripts: bool,
) -> PyResult<Bound<'py, PyDict>> {
    let path = project.unwrap_or_else(|| PathBuf::from("pyproject.toml"));
    let file = project::read(&path).map_err(err)?;
    let summary = block_on(
        py,
        project::sync(&file, install::LinkScripts::from(link_scripts)),
    )?
    .map_err(err)?;
    summary_dict(py, &summary)
}

/// Check a project's `[project.dependencies]` against the environment it
/// references, returning a report dict (`dependencies`, `satisfied`,
/// `conflicts`, `missing`, `skipped`, `has_conflicts`). `project` defaults to
/// `./pyproject.toml`; `platform` overrides the checked platform.
#[pyfunction]
#[pyo3(signature = (project = None, *, platform = None))]
fn check<'py>(
    py: Python<'py>,
    project: Option<PathBuf>,
    platform: Option<String>,
) -> PyResult<Bound<'py, PyDict>> {
    let path = project.unwrap_or_else(|| PathBuf::from("pyproject.toml"));
    let file = project::read(&path).map_err(err)?;
    let report = block_on(py, project::check(&file, platform.as_deref()))?.map_err(err)?;
    check_report_dict(py, &report)
}

/// Trial-solve an environment with extra requirements to check whether it would
/// still solve on its next build. Recovers the environment's manifest from the
/// registry (embedded band, then sidecar), injects `with_` conda specs and/or a
/// `project` pyproject's `[project.dependencies]` (PyPI → conda), and re-solves.
/// Returns `{satisfiable, platform, packages, conflict}` — `satisfiable=False`
/// with a `conflict` message on a solver conflict.
#[pyfunction]
#[pyo3(signature = (env, registry, *, with_ = None, project = None, platform = None, python = None, variant = None, label = "latest", channel_priority = "strict", exclude_newer = None))]
#[allow(clippy::too_many_arguments)]
fn try_solve<'py>(
    py: Python<'py>,
    env: String,
    registry: String,
    with_: Option<Vec<String>>,
    project: Option<PathBuf>,
    platform: Option<String>,
    python: Option<String>,
    variant: Option<String>,
    label: &str,
    channel_priority: &str,
    exclude_newer: Option<String>,
) -> PyResult<Bound<'py, PyDict>> {
    let priority = ChannelPriorityMode::from_str(channel_priority)
        .map_err(|e| PyValueError::new_err(e.to_string()))?;
    let registry = Registry::new(SpecStore::new(), registry);
    let coords = coordinates(
        env.clone(),
        platform.clone(),
        python.clone(),
        variant.clone(),
    );
    let resolved_label = Label::parse(label);

    // Recover the manifest the environment was solved from: the lock's embedded
    // band, falling back to the registry sidecar.
    let lock_bytes = registry.pull(&coords, &resolved_label).map_err(err)?;
    let yaml = match nepenthe_core::embed::extract_manifest(&lock_bytes).map_err(err)? {
        Some(yaml) => yaml,
        None => {
            let bytes = registry
                .pull_manifest(&coords, &resolved_label)
                .map_err(err)?
                .ok_or_else(|| {
                    PyRuntimeError::new_err(
                        "release has no manifest (no embedded band and no sidecar)",
                    )
                })?;
            String::from_utf8(bytes).map_err(err)?
        }
    };
    let manifest = Manifest::from_yaml_str(&yaml).map_err(err)?;

    let mut specs = with_.unwrap_or_default();
    if let Some(path) = &project {
        let deps = project::read_dependencies(path).map_err(err)?;
        specs.extend(project::requirements_to_conda_specs(&deps));
    }
    if specs.is_empty() {
        return Err(PyValueError::new_err(
            "nothing to try: pass with_=[...] and/or project=<pyproject.toml>",
        ));
    }

    let selector = Selector {
        variant: variant.clone(),
        python: python.clone(),
    };
    let settings = ChannelSettings::from_manifest(&manifest);
    let result = block_on(
        py,
        producer::trial_solve(
            &manifest,
            &env,
            &selector,
            &specs,
            &settings,
            priority,
            platform.as_deref(),
            exclude_newer,
        ),
    )?;

    let dict = PyDict::new(py);
    match result {
        Ok(outcome) => {
            dict.set_item("satisfiable", true)?;
            dict.set_item("platform", outcome.platform)?;
            let packages = PyList::empty(py);
            for package in &outcome.packages {
                let entry = PyDict::new(py);
                entry.set_item("name", &package.name)?;
                entry.set_item("version", &package.version)?;
                packages.append(entry)?;
            }
            dict.set_item("packages", packages)?;
            dict.set_item("conflict", py.None())?;
        }
        Err(e) if matches!(&e, BuildError::Solve(SolveError::Solve(_))) => {
            dict.set_item("satisfiable", false)?;
            dict.set_item("platform", py.None())?;
            dict.set_item("packages", PyList::empty(py))?;
            dict.set_item("conflict", e.to_string())?;
        }
        Err(e) => return Err(err(e)),
    }
    Ok(dict)
}

/// List a registry's releases (newest first), optionally limited to one `env`.
/// Returns a list of release dicts.
#[pyfunction]
#[pyo3(signature = (registry, *, env = None))]
fn list_releases<'py>(
    py: Python<'py>,
    registry: String,
    env: Option<String>,
) -> PyResult<Bound<'py, PyList>> {
    let registry = Registry::new(SpecStore::new(), registry);
    let index: Index = registry.load_index().map_err(err)?;
    let environments = match &env {
        Some(name) => vec![name.clone()],
        None => index.environments(),
    };
    let out = PyList::empty(py);
    for environment in environments {
        for release in index.releases_of(&environment) {
            out.append(release_dict(py, release)?)?;
        }
    }
    Ok(out)
}

#[pymodule]
fn nepenthe(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    m.add_function(wrap_pyfunction!(version, m)?)?;
    m.add_function(wrap_pyfunction!(current_platform, m)?)?;
    m.add_function(wrap_pyfunction!(build, m)?)?;
    m.add_function(wrap_pyfunction!(create, m)?)?;
    m.add_function(wrap_pyfunction!(pull, m)?)?;
    m.add_function(wrap_pyfunction!(manifest, m)?)?;
    m.add_function(wrap_pyfunction!(publish, m)?)?;
    m.add_function(wrap_pyfunction!(show, m)?)?;
    m.add_function(wrap_pyfunction!(diff, m)?)?;
    m.add_function(wrap_pyfunction!(status, m)?)?;
    m.add_function(wrap_pyfunction!(remove, m)?)?;
    m.add_function(wrap_pyfunction!(activate, m)?)?;
    m.add_function(wrap_pyfunction!(pack, m)?)?;
    m.add_function(wrap_pyfunction!(unpack, m)?)?;
    m.add_function(wrap_pyfunction!(sync, m)?)?;
    m.add_function(wrap_pyfunction!(check, m)?)?;
    m.add_function(wrap_pyfunction!(try_solve, m)?)?;
    m.add_function(wrap_pyfunction!(list_releases, m)?)?;
    m.add_function(wrap_pyfunction!(fsspec_pull, m)?)?;
    m.add_function(wrap_pyfunction!(fsspec_publish, m)?)?;
    Ok(())
}
