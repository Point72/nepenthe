//! `nepenthe` command-line interface.
//!
//! Covers the full producer → consumer lifecycle:
//!
//! - **Producer**: `build` solves a manifest into lock(s) and (optionally)
//!   publishes them to a registry.
//! - **Consumer**: `create` / `pull` / `publish` / `show` / `diff` / `status` /
//!   `remove` / `activate` / `cache` operate on published locks and prefixes.
//!
//! A single multicall binary is installed as `nepenthe`, with `np` and `npb`
//! symlinks pointing at it. [`run_multicall`] reads the invoked name (argv[0]):
//! `npb` runs `nepenthe build`, any other name runs the full CLI. See also
//! [`run`] and [`run_build`].

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand, ValueEnum};
use rattler_conda_types::Platform;

use crate::backend::SpecStore;
use crate::install;
use crate::registry::{Coordinates, Label, Registry};

/// nepenthe — forget your environment sorrows.
#[derive(Parser)]
#[command(name = "nepenthe", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

/// Argument parser for the `npb` personality, equivalent to `nepenthe build`.
///
/// The auto `--version` flag is disabled so `--version` carries the publish
/// version (as on `nepenthe build`); use `nepenthe --version` for the binary
/// version.
#[derive(Parser)]
#[command(name = "npb", about = "Shortcut for `nepenthe build`.", long_about = None)]
struct NpbCli {
    #[command(flatten)]
    args: BuildArgs,
}

#[derive(Subcommand)]
enum Command {
    /// Solve a manifest into lock(s) and optionally publish them.
    Build(BuildArgs),
    /// Create an environment from a local lock file or a published registry release (no conda required).
    Create(CreateArgs),
    /// Download a lock from a registry without installing it.
    Pull(PullArgs),
    /// Render a lock as a `conda create --file` (`@EXPLICIT`) spec.
    Export(ExportArgs),
    /// Recover the manifest embedded in a lock or published alongside it.
    Manifest(ManifestArgs),
    /// Publish a lock file to a registry under a version.
    Publish(PublishArgs),
    /// Show the release a label resolves to.
    Show(ShowArgs),
    /// Compare an installed prefix against a lock file.
    Diff(DiffArgs),
    /// Report what is installed in a prefix.
    Status(StatusArgs),
    /// Remove an environment prefix.
    Remove(RemoveArgs),
    /// Print an activation script for a prefix.
    Activate(ActivateArgs),
    /// Pack a lock's packages into a self-contained offline bundle.
    Pack(PackArgs),
    /// Install an environment from a packed bundle (offline, no conda).
    Unpack(UnpackArgs),
    /// Install the environment a `pyproject.toml` references (`[tool.nepenthe]`).
    Sync(SyncArgs),
    /// Check a project's dependencies against its referenced environment.
    Check(CheckArgs),
    /// Run a command in a base environment plus an optional conda/PyPI overlay.
    Run(RunArgs),
    /// Trial-solve an environment with extra requirements, before publishing.
    Try(TryArgs),
    /// Open an activated subshell in a published environment.
    Shell(ShellArgs),
    /// List the environments and releases in a registry.
    List(ListArgs),
    /// Compare the package sets of two published releases of an environment.
    DiffVersions(DiffVersionsArgs),
    /// Compose several published environments into one lock.
    Compose(ComposeArgs),
    /// Report the licenses of a lock's packages and flag denied ones.
    License(LicenseArgs),
    /// Build a container image (SIF or OCI) from a published environment.
    #[command(subcommand)]
    Image(ImageCommand),
    /// Manage the shared package cache.
    #[command(subcommand)]
    Cache(CacheCommand),
}

/// Registry coordinates shared by registry-backed commands.
#[derive(Args)]
struct Coords {
    /// Environment name.
    env: String,
    /// Registry root URL (e.g. `file:///srv/nepenthe`, `s3://bucket/envs`).
    #[arg(long)]
    registry: String,
    /// Target platform (defaults to the current platform).
    #[arg(long)]
    platform: Option<String>,
    /// Python axis value (e.g. `3.11`), if the environment fans out over python.
    #[arg(long)]
    python: Option<String>,
    /// Variant axis value (e.g. `cpu`/`gpu`), if any.
    #[arg(long)]
    variant: Option<String>,
}

impl Coords {
    fn platform(&self) -> String {
        self.platform
            .clone()
            .unwrap_or_else(|| Platform::current().to_string())
    }

    fn coordinates(&self) -> Coordinates {
        let mut c = Coordinates::new(self.env.clone(), self.platform());
        if let Some(py) = &self.python {
            c = c.with_python(py.clone());
        }
        if let Some(v) = &self.variant {
            c = c.with_variant(v.clone());
        }
        c
    }

    fn build_registry(&self) -> Registry {
        Registry::new(SpecStore::new(), self.registry.clone())
    }
}

#[derive(Args)]
struct BuildArgs {
    /// Environment manifest to solve: a local YAML path, or a spec-backend URL
    /// (`file://`, `s3://`, `https://`). A remote manifest must be
    /// self-contained (no `imports`).
    #[arg(long)]
    manifest: String,
    /// Optional override layer applied before solving: a local YAML path or a
    /// spec-backend URL, same as `--manifest`.
    #[arg(long)]
    overrides: Option<String>,
    /// Environment name within the manifest to build.
    #[arg(long)]
    env: String,
    /// Directory to write one lock file per build cell.
    #[arg(long)]
    output_dir: Option<PathBuf>,
    /// Registry root URL to publish to (requires `--version`).
    #[arg(long, requires = "version")]
    registry: Option<String>,
    /// Semver version to publish the solved locks under (requires `--registry`).
    #[arg(long, requires = "registry")]
    version: Option<String>,
    /// Channel-priority policy: `strict` (default) or `disabled`.
    #[arg(long, default_value = "strict")]
    channel_priority: String,
    /// Repodata cutoff (RFC3339) pinning the solve for reproducibility.
    #[arg(long)]
    exclude_newer: Option<String>,
    /// Build only this Python axis value (e.g. `3.11`). Omit to build every
    /// Python in the environment's axis.
    #[arg(long)]
    python: Option<String>,
    /// Build only this variant axis value (e.g. `cpu`/`gpu`). Omit to build
    /// every variant in the environment's axis.
    #[arg(long)]
    variant: Option<String>,
}

#[derive(Args)]
struct CreateArgs {
    /// Environment name. With `--registry` it selects the release to resolve;
    /// with `--lock` it defaults to the lock's only environment (required when
    /// the lock declares several).
    env: Option<String>,
    /// Install directly from a local lock file — no registry, no solve. The
    /// lock's own package URLs drive the fetch.
    #[arg(long, conflicts_with = "registry")]
    lock: Option<PathBuf>,
    /// Registry root URL to resolve and pull the lock from.
    #[arg(long, conflicts_with = "lock")]
    registry: Option<String>,
    /// Target platform (defaults to the current platform).
    #[arg(long)]
    platform: Option<String>,
    /// Python axis value (e.g. `3.11`), when resolving from a registry.
    #[arg(long)]
    python: Option<String>,
    /// Variant axis value (e.g. `cpu`/`gpu`), when resolving from a registry.
    #[arg(long)]
    variant: Option<String>,
    /// Version label to resolve (`latest`, `latest-but-one`, an exact version, or a semver range),
    /// when resolving from a registry.
    #[arg(long, default_value = "latest")]
    label: String,
    /// Directory to install the environment into.
    #[arg(long)]
    prefix: PathBuf,
}

#[derive(Args)]
struct PullArgs {
    #[command(flatten)]
    coords: Coords,
    /// Version label to resolve.
    #[arg(long, default_value = "latest")]
    label: String,
    /// File to write the lock to.
    #[arg(short, long)]
    output: PathBuf,
}

#[derive(Args)]
struct ExportArgs {
    /// Environment name (within the lock, or to resolve from a registry).
    #[arg(long)]
    env: String,
    /// Read packages from a local lock file (no registry needed).
    #[arg(long, conflicts_with = "registry")]
    lock: Option<PathBuf>,
    /// Registry root URL to resolve and pull the lock from.
    #[arg(long, conflicts_with = "lock")]
    registry: Option<String>,
    /// Target platform (defaults to the current platform).
    #[arg(long)]
    platform: Option<String>,
    /// Python axis value, when resolving from a registry.
    #[arg(long)]
    python: Option<String>,
    /// Variant axis value, when resolving from a registry.
    #[arg(long)]
    variant: Option<String>,
    /// Version label to resolve from a registry.
    #[arg(long, default_value = "latest")]
    label: String,
    /// File to write the spec to (defaults to stdout).
    #[arg(short, long)]
    output: Option<PathBuf>,
}

#[derive(Args)]
struct ManifestArgs {
    /// Recover from a lock file's embedded comment band (no registry needed).
    #[arg(long, conflicts_with_all = ["env", "registry"])]
    lock: Option<PathBuf>,
    /// Environment name to resolve from a registry (with `--registry`).
    #[arg(long, requires = "registry")]
    env: Option<String>,
    /// Registry root URL to resolve from (with `--env`).
    #[arg(long, requires = "env")]
    registry: Option<String>,
    /// Target platform (defaults to the current platform).
    #[arg(long)]
    platform: Option<String>,
    /// Python axis value, if the environment fans out over python.
    #[arg(long)]
    python: Option<String>,
    /// Variant axis value (e.g. `cpu`/`gpu`), if any.
    #[arg(long)]
    variant: Option<String>,
    /// Version label to resolve.
    #[arg(long, default_value = "latest")]
    label: String,
    /// File to write the manifest to (defaults to stdout).
    #[arg(short, long)]
    output: Option<PathBuf>,
}

#[derive(Args)]
struct PublishArgs {
    #[command(flatten)]
    coords: Coords,
    /// Semver version to publish under.
    #[arg(long)]
    version: String,
    /// Lock file to publish.
    #[arg(long)]
    lock: PathBuf,
}

#[derive(Args)]
struct LicenseArgs {
    /// Report from a local lock file (no registry needed).
    #[arg(long, conflicts_with_all = ["env", "registry"])]
    lock: Option<PathBuf>,
    /// Environment name to resolve from a registry (with `--registry`).
    #[arg(long, requires = "registry")]
    env: Option<String>,
    /// Registry root URL to resolve from (with `--env`).
    #[arg(long, requires = "env")]
    registry: Option<String>,
    /// Target platform (defaults to the current platform).
    #[arg(long)]
    platform: Option<String>,
    /// Python axis value, if the environment fans out over python.
    #[arg(long)]
    python: Option<String>,
    /// Variant axis value (e.g. `cpu`/`gpu`), if any.
    #[arg(long)]
    variant: Option<String>,
    /// Version label to resolve.
    #[arg(long, default_value = "latest")]
    label: String,
    /// License to flag (repeatable); any matching package fails the report.
    /// Matched case-insensitively and exactly against the conda license string.
    #[arg(long)]
    deny: Vec<String>,
}

#[derive(Args)]
struct ShowArgs {
    #[command(flatten)]
    coords: Coords,
    /// Version label to resolve.
    #[arg(long, default_value = "latest")]
    label: String,
}

#[derive(Args)]
struct DiffArgs {
    /// Lock file to compare against.
    #[arg(long)]
    lock: PathBuf,
    /// Environment name within the lock.
    #[arg(long)]
    env: String,
    /// Platform within the lock (defaults to the current platform).
    #[arg(long)]
    platform: Option<String>,
    /// Installed prefix to compare.
    #[arg(long)]
    prefix: PathBuf,
}

#[derive(Args)]
struct StatusArgs {
    /// Prefix to inspect.
    #[arg(long)]
    prefix: PathBuf,
}

#[derive(Args)]
struct RemoveArgs {
    /// Prefix to remove.
    #[arg(long)]
    prefix: PathBuf,
    /// Remove even if the prefix has no `conda-meta` marker. Protected paths
    /// (filesystem root, home, current directory) are still refused.
    #[arg(long)]
    force: bool,
}

#[derive(Args)]
struct ActivateArgs {
    /// Prefix to activate.
    #[arg(long)]
    prefix: PathBuf,
    /// Target platform (defaults to the current platform).
    #[arg(long)]
    platform: Option<String>,
    /// Shell to target (`bash`, `zsh`, `fish`, `xonsh`, `cmd`, `powershell`, `nu`). Defaults to the current shell.
    #[arg(long)]
    shell: Option<String>,
}

#[derive(Args)]
struct PackArgs {
    /// Lock file whose packages to bundle.
    #[arg(long)]
    lock: PathBuf,
    /// Environment name within the lock.
    #[arg(long)]
    env: String,
    /// Platform(s) to include (repeat for several; defaults to all in the lock).
    #[arg(long)]
    platform: Vec<String>,
    /// Bundle file to write (a `.tar` archive).
    #[arg(long)]
    output: PathBuf,
}

#[derive(Args)]
struct UnpackArgs {
    /// Bundle file to install from.
    #[arg(long)]
    pack: PathBuf,
    /// Directory to install the environment into.
    #[arg(long)]
    prefix: PathBuf,
    /// Environment within the bundle (defaults to the bundle's environment).
    #[arg(long)]
    env: Option<String>,
    /// Platform to install (defaults to the current platform).
    #[arg(long)]
    platform: Option<String>,
    /// Directory to extract the bundle into (defaults to a temporary directory).
    #[arg(long)]
    stage_dir: Option<PathBuf>,
}

#[derive(Args)]
struct SyncArgs {
    /// Path to the `pyproject.toml` to read (defaults to `./pyproject.toml`).
    #[arg(long, default_value = "pyproject.toml")]
    project: PathBuf,
}

#[derive(Args)]
struct CheckArgs {
    /// Path to the `pyproject.toml` to read (defaults to `./pyproject.toml`).
    #[arg(long, default_value = "pyproject.toml")]
    project: PathBuf,
    /// Platform to check against (defaults to the project's, then the current).
    #[arg(long)]
    platform: Option<String>,
}

#[derive(Args)]
struct RunArgs {
    /// A script with an inline `# /// nepenthe` block, or a `pyproject.toml`.
    /// Defaults to `./pyproject.toml`.
    #[arg(long)]
    config: Option<PathBuf>,
    /// Extra conda match-specs to overlay (repeat; adds to the config's overlay).
    #[arg(long = "with")]
    with: Vec<String>,
    /// Extra PyPI requirements to overlay with uv (repeat; adds to the config's
    /// `overlay.pip`).
    #[arg(long = "with-pip")]
    with_pip: Vec<String>,
    /// Run inside an Apptainer/SIF image of the environment instead of a prefix.
    #[arg(long)]
    image: bool,
    /// Base OS image for `--image` (must provide glibc + `/bin/sh`).
    #[arg(long)]
    base: Option<String>,
    /// Lazy `--image`: bind the env at run time instead of baking it into the SIF.
    #[arg(long)]
    lazy: bool,
    /// Give the `--image` container an ephemeral in-memory writable layer.
    #[arg(long)]
    writable: bool,
    /// Persistent EXT3 overlay image for `--image` writes over a read-only base.
    #[arg(long)]
    overlay_image: Option<PathBuf>,
    /// Write the run's standalone overlay lock (conda + PyPI) to this path.
    #[arg(long)]
    emit_overlay_lock: Option<PathBuf>,
    /// Materialize via a copy-on-write clone of the base prefix (reflink FS).
    #[arg(long)]
    clone: bool,
    /// The command (and arguments) to run, after `--`. Overrides the config's
    /// command when given.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<String>,
}

#[derive(Args)]
struct TryArgs {
    /// Recover the environment manifest from a lock file's embedded band.
    #[arg(long, conflicts_with_all = ["env", "registry"])]
    lock: Option<PathBuf>,
    /// Environment to trial-solve, resolved from a registry (with `--registry`).
    #[arg(long, requires = "registry")]
    env: Option<String>,
    /// Registry root URL to resolve the manifest from (with `--env`).
    #[arg(long, requires = "env")]
    registry: Option<String>,
    /// Version label to resolve the manifest from.
    #[arg(long, default_value = "latest")]
    label: String,
    /// Environment name within the manifest (defaults to `--env`).
    #[arg(long = "environment")]
    environment: Option<String>,
    /// Target platform (defaults to the environment's first platform).
    #[arg(long)]
    platform: Option<String>,
    /// Python axis value to select.
    #[arg(long)]
    python: Option<String>,
    /// Variant axis value to select.
    #[arg(long)]
    variant: Option<String>,
    /// Extra conda match-specs to inject (repeat).
    #[arg(long = "with")]
    with: Vec<String>,
    /// Inject a `pyproject.toml`'s `[project.dependencies]` (PyPI → conda).
    #[arg(long)]
    project: Option<PathBuf>,
    /// Channel-priority policy: `strict` (default) or `disabled`.
    #[arg(long, default_value = "strict")]
    channel_priority: String,
    /// Repodata cutoff (RFC3339) pinning the solve for reproducibility.
    #[arg(long)]
    exclude_newer: Option<String>,
}

#[derive(Args)]
struct ShellArgs {
    #[command(flatten)]
    coords: Coords,
    /// Version label to resolve.
    #[arg(long, default_value = "latest")]
    label: String,
    /// Directory to materialize the environment into (defaults to a cache dir).
    #[arg(long)]
    prefix: Option<PathBuf>,
    /// Shell to launch (defaults to `$SHELL`, then `bash`).
    #[arg(long)]
    shell: Option<String>,
}

#[derive(Args)]
struct ListArgs {
    /// Registry root URL to list.
    #[arg(long)]
    registry: String,
    /// Limit to one environment's releases.
    #[arg(long)]
    env: Option<String>,
}

#[derive(Args)]
struct DiffVersionsArgs {
    #[command(flatten)]
    coords: Coords,
    /// The older version label (e.g. `1.2.0`).
    #[arg(long)]
    from: String,
    /// The newer version label (e.g. `1.3.0`).
    #[arg(long)]
    to: String,
}

#[derive(Args)]
struct ComposeArgs {
    /// Registry root URL the environments are published to.
    #[arg(long)]
    registry: String,
    /// An environment to include, as `name` or `name@version` (repeat, ≥2).
    #[arg(long = "env", required = true)]
    envs: Vec<String>,
    /// Target platform shared by all inputs (defaults to the current platform).
    #[arg(long)]
    platform: Option<String>,
    /// Python axis value shared by all inputs.
    #[arg(long)]
    python: Option<String>,
    /// Variant axis value shared by all inputs.
    #[arg(long)]
    variant: Option<String>,
    /// Environment name for the composed lock.
    #[arg(long, default_value = "composed")]
    name: String,
    /// Output path for the composed lock file.
    #[arg(long)]
    output: PathBuf,
}

#[derive(Subcommand)]
enum ImageCommand {
    /// Build a self-contained Apptainer/SIF or OCI image from a published environment.
    Build(ImageBuildArgs),
}

/// The image format `image build` produces.
#[derive(Clone, Copy, ValueEnum)]
enum ImageFormat {
    /// Apptainer/SIF image file (default).
    Sif,
    /// OCI image loaded into the local podman/docker store.
    Oci,
}

#[derive(Args)]
struct ImageBuildArgs {
    #[command(flatten)]
    coords: Coords,
    /// Version label to resolve.
    #[arg(long, default_value = "latest")]
    label: String,
    /// Image format to build.
    #[arg(long, value_enum, default_value_t = ImageFormat::Sif)]
    format: ImageFormat,
    /// Output SIF path (required for `--format sif`), e.g. `app.sif`.
    #[arg(long)]
    output: Option<PathBuf>,
    /// Image tag (required for `--format oci`), e.g. `app:1.0.0`.
    #[arg(long)]
    tag: Option<String>,
    /// Base to bootstrap from: a Docker/OCI ref (default), or
    /// `localimage:<path.sif>` to layer on an existing SIF (SIF format only).
    /// Must provide glibc + `/bin/sh`.
    #[arg(long, default_value_t = crate::image::DEFAULT_BASE_IMAGE.to_string())]
    base: String,
    /// Lazy image (SIF only): don't bake the env in; bind it at run time.
    #[arg(long)]
    lazy: bool,
    /// Staging prefix to materialize into (defaults to a cache dir). Also the
    /// environment's path inside the image.
    #[arg(long)]
    prefix: Option<PathBuf>,
}

#[derive(Subcommand)]
enum CacheCommand {
    /// Show the package cache location, or remove it with `--all`.
    Clean {
        /// Delete the entire package cache.
        #[arg(long)]
        all: bool,
    },
}

type CliResult = Result<(), Box<dyn std::error::Error>>;

/// Run the full `nepenthe` CLI (also used for the `np` alias). Parses arguments
/// from the process, dispatches the chosen command, and returns a process exit
/// code.
pub fn run() -> ExitCode {
    dispatch(Cli::parse())
}

/// Run the `npb` shortcut binary, equivalent to `nepenthe build`.
pub fn run_build() -> ExitCode {
    let runtime = match build_runtime() {
        Ok(rt) => rt,
        Err(code) => return code,
    };
    finish(runtime.block_on(build(NpbCli::parse().args)))
}

/// Entry point for the single multicall binary. Chooses behaviour from the
/// invoked program name (argv[0]): a name of `npb` runs [`run_build`], every
/// other name (`nepenthe`, `np`, or anything else) runs the full [`run`] CLI.
pub fn run_multicall() -> ExitCode {
    let arg0 = std::env::args().next().unwrap_or_default();
    if selects_build(&arg0) {
        run_build()
    } else {
        run()
    }
}

/// Whether an invoked program name (argv[0], possibly a full path) selects the
/// `nepenthe build` shortcut — i.e. its file stem is `npb`.
fn selects_build(arg0: &str) -> bool {
    std::path::Path::new(arg0)
        .file_stem()
        .and_then(|s| s.to_str())
        .is_some_and(|stem| stem == "npb")
}

fn dispatch(cli: Cli) -> ExitCode {
    let runtime = match build_runtime() {
        Ok(rt) => rt,
        Err(code) => return code,
    };
    runtime.block_on(async {
        let result = match cli.command {
            None => {
                println!(
                    "nepenthe {} (core {})",
                    env!("CARGO_PKG_VERSION"),
                    crate::version()
                );
                Ok(())
            }
            Some(command) => run_command(command).await,
        };
        finish(result)
    })
}

fn build_runtime() -> Result<tokio::runtime::Runtime, ExitCode> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| {
            eprintln!("error: failed to start async runtime: {e}");
            ExitCode::FAILURE
        })
}

fn finish(result: CliResult) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run_command(command: Command) -> CliResult {
    match command {
        Command::Build(args) => build(args).await,
        Command::Create(args) => create(args).await,
        Command::Pull(args) => pull(args),
        Command::Export(args) => export(args),
        Command::Manifest(args) => manifest(args),
        Command::Publish(args) => publish(args),
        Command::Show(args) => show(args),
        Command::Diff(args) => diff(args),
        Command::Status(args) => status(args),
        Command::Remove(args) => remove(args),
        Command::Activate(args) => activate(args),
        Command::Pack(args) => pack(args).await,
        Command::Unpack(args) => unpack(args).await,
        Command::Sync(args) => sync(args).await,
        Command::Check(args) => check(args).await,
        Command::Run(args) => run_env(args).await,
        Command::Try(args) => try_solve(args).await,
        Command::Shell(args) => shell(args).await,
        Command::List(args) => list(args),
        Command::DiffVersions(args) => diff_versions(args),
        Command::Compose(args) => compose(args).await,
        Command::License(args) => license(args),
        Command::Image(ImageCommand::Build(args)) => image_build(args).await,
        Command::Cache(CacheCommand::Clean { all }) => cache_clean(all),
    }
}

async fn build(args: BuildArgs) -> CliResult {
    let channel_priority: crate::solve::ChannelPriorityMode = args.channel_priority.parse()?;
    let request = crate::producer::BuildRequest {
        manifest: args.manifest,
        overrides: args.overrides,
        environment: args.env,
        output_dir: args.output_dir,
        registry: args.registry,
        version: args.version,
        channel_priority,
        exclude_newer: args.exclude_newer,
        python: args.python,
        variant: args.variant,
    };

    let cells = crate::producer::build(&request).await?;
    for cell in &cells {
        if let Some(path) = &cell.lock_path {
            println!(
                "wrote {} ({} platforms)",
                path.display(),
                cell.platforms.len()
            );
        }
        for release in &cell.releases {
            println!(
                "published {} {} on {} → {}",
                release.environment, release.version, release.platform, release.lock
            );
        }
    }

    Ok(())
}

async fn create(args: CreateArgs) -> CliResult {
    let platform = args
        .platform
        .clone()
        .unwrap_or_else(|| Platform::current().to_string());

    let summary = if let Some(lock_path) = &args.lock {
        // No registry, no solve: install exactly the packages the lock pins,
        // fetched from their own channel URLs.
        let bytes = std::fs::read(lock_path)?;
        let lock = install::parse_lock(&bytes)?;
        let environment = match &args.env {
            Some(env) => env.clone(),
            None => install::sole_environment(&lock)?,
        };
        let summary = install::install_lock(&lock, &environment, &platform, &args.prefix).await?;
        install::write_hooks_from_lock(
            &bytes,
            &environment,
            &platform,
            args.python.as_deref(),
            args.variant.as_deref(),
            None,
            &args.prefix,
        )?;
        summary
    } else {
        let registry_url = args
            .registry
            .clone()
            .ok_or("pass --lock <file>, or --registry <url> with an environment name")?;
        let env = args
            .env
            .clone()
            .ok_or("pass an environment name to resolve from --registry")?;
        let registry = Registry::new(SpecStore::new(), registry_url);
        let mut coords = Coordinates::new(env, platform.clone());
        if let Some(py) = &args.python {
            coords = coords.with_python(py.clone());
        }
        if let Some(v) = &args.variant {
            coords = coords.with_variant(v.clone());
        }
        let label = Label::parse(&args.label);
        install::create(&registry, &coords, &label, &args.prefix).await?
    };

    println!(
        "created {} ({}) at {} — {} packages",
        summary.environment,
        summary.platform,
        summary.prefix.display(),
        summary.packages.len()
    );
    Ok(())
}

fn pull(args: PullArgs) -> CliResult {
    let registry = args.coords.build_registry();
    let coords = args.coords.coordinates();
    let label = Label::parse(&args.label);
    let bytes = registry.pull(&coords, &label)?;
    std::fs::write(&args.output, &bytes)?;
    println!("pulled {} bytes to {}", bytes.len(), args.output.display());
    Ok(())
}

fn export(args: ExportArgs) -> CliResult {
    let platform = args
        .platform
        .clone()
        .unwrap_or_else(|| Platform::current().to_string());

    // Source the lock from a local file or a registry release.
    let bytes = if let Some(lock_path) = &args.lock {
        std::fs::read(lock_path)?
    } else if let Some(registry_url) = &args.registry {
        let registry = Registry::new(SpecStore::new(), registry_url.clone());
        let mut coords = Coordinates::new(args.env.clone(), platform.clone());
        if let Some(py) = &args.python {
            coords = coords.with_python(py.clone());
        }
        if let Some(v) = &args.variant {
            coords = coords.with_variant(v.clone());
        }
        let label = Label::parse(&args.label);
        registry.pull(&coords, &label)?
    } else {
        return Err("pass --lock <file>, or --registry <url>".into());
    };

    let lock = install::parse_lock(&bytes)?;
    let records = install::lock_records(&lock, &args.env, &platform)?;
    let spec = crate::export::to_explicit_records(&records);

    match &args.output {
        Some(path) => {
            std::fs::write(path, spec.as_bytes())?;
            eprintln!(
                "wrote @EXPLICIT spec ({} packages) → {}",
                records.len(),
                path.display()
            );
        }
        None => print!("{spec}"),
    }
    Ok(())
}

fn manifest(args: ManifestArgs) -> CliResult {
    // Recover the manifest from either source: a lock file's embedded band
    // (portable, no registry) or a registry release's sidecar.
    let (yaml, source) = if let Some(lock_path) = &args.lock {
        let bytes = std::fs::read(lock_path)?;
        let yaml =
            crate::embed::extract_manifest(&bytes)?.ok_or("lock has no embedded manifest band")?;
        (yaml, format!("embedded band in {}", lock_path.display()))
    } else if let (Some(env), Some(registry_url)) = (&args.env, &args.registry) {
        let registry = Registry::new(SpecStore::new(), registry_url.clone());
        let platform = args
            .platform
            .clone()
            .unwrap_or_else(|| Platform::current().to_string());
        let mut coords = Coordinates::new(env.clone(), platform);
        if let Some(py) = &args.python {
            coords = coords.with_python(py.clone());
        }
        if let Some(v) = &args.variant {
            coords = coords.with_variant(v.clone());
        }
        let label = Label::parse(&args.label);
        // Prefer the lock's embedded band, fall back to the registry sidecar.
        let lock_bytes = registry.pull(&coords, &label)?;
        match crate::embed::extract_manifest(&lock_bytes)? {
            Some(yaml) => (yaml, "embedded band in the published lock".to_string()),
            None => {
                let bytes = registry
                    .pull_manifest(&coords, &label)?
                    .ok_or("release has no manifest (no embedded band and no sidecar)")?;
                let yaml = String::from_utf8(bytes)
                    .map_err(|e| format!("manifest is not valid UTF-8: {e}"))?;
                (yaml, "registry sidecar".to_string())
            }
        }
    } else {
        return Err("pass --lock <file>, or --env <name> --registry <url>".into());
    };

    match &args.output {
        Some(path) => {
            std::fs::write(path, yaml.as_bytes())?;
            eprintln!("recovered manifest ({source}) → {}", path.display());
        }
        None => print!("{yaml}"),
    }
    Ok(())
}

fn license(args: LicenseArgs) -> CliResult {
    // Load the lock bytes from either source: a local file, or a registry
    // release resolved by coordinates + label.
    let lock_bytes = if let Some(lock_path) = &args.lock {
        std::fs::read(lock_path)?
    } else if let (Some(env), Some(registry_url)) = (&args.env, &args.registry) {
        let registry = Registry::new(SpecStore::new(), registry_url.clone());
        let platform = args
            .platform
            .clone()
            .unwrap_or_else(|| Platform::current().to_string());
        let mut coords = Coordinates::new(env.clone(), platform);
        if let Some(py) = &args.python {
            coords = coords.with_python(py.clone());
        }
        if let Some(v) = &args.variant {
            coords = coords.with_variant(v.clone());
        }
        let label = Label::parse(&args.label);
        registry.pull(&coords, &label)?
    } else {
        return Err("pass --lock <file>, or --env <name> --registry <url>".into());
    };

    let lock = install::parse_lock(&lock_bytes)?;
    let by_license = crate::license::collect(&lock)?;
    print!("{}", crate::license::render_text(&by_license));

    let flagged = crate::license::flagged(&by_license, &args.deny);
    if !flagged.is_empty() {
        eprintln!("\ndenied licenses found:");
        for (license, pkg) in &flagged {
            eprintln!("  {pkg} ({license})");
        }
        return Err(format!(
            "{} package(s) violate the license deny policy",
            flagged.len()
        )
        .into());
    }
    Ok(())
}

fn publish(args: PublishArgs) -> CliResult {
    let registry = args.coords.build_registry();
    let coords = args.coords.coordinates();
    let bytes = std::fs::read(&args.lock)?;
    // Validate the lock parses and actually contains the target
    // environment/platform before creating an immutable release.
    let lock = install::parse_lock(&bytes)?;
    install::lock_records(&lock, &coords.environment, &coords.platform)?;
    let release = registry.publish(&coords, &args.version, &bytes)?;
    println!(
        "published {} {} on {} → {}",
        release.environment, release.version, release.platform, release.lock
    );
    Ok(())
}

fn show(args: ShowArgs) -> CliResult {
    let registry = args.coords.build_registry();
    let coords = args.coords.coordinates();
    let label = Label::parse(&args.label);
    let release = registry.resolve(&coords, &label)?;
    println!("environment: {}", release.environment);
    println!("platform:    {}", release.platform);
    if let Some(py) = &release.python {
        println!("python:      {py}");
    }
    if let Some(v) = &release.variant {
        println!("variant:     {v}");
    }
    println!("version:     {}", release.version);
    println!("lock:        {}", release.lock);
    println!("created:     {}", release.created);
    Ok(())
}

fn diff(args: DiffArgs) -> CliResult {
    let platform = args
        .platform
        .unwrap_or_else(|| Platform::current().to_string());
    let bytes = std::fs::read(&args.lock)?;
    let lock = install::parse_lock(&bytes)?;
    let d = install::diff(&lock, &args.env, &platform, &args.prefix)?;
    if d.is_empty() {
        println!("up to date — prefix matches the lock");
        return Ok(());
    }
    for p in &d.added {
        println!("+ {p}");
    }
    for p in &d.removed {
        println!("- {p}");
    }
    for (have, want) in &d.changed {
        println!("~ {have} -> {want}");
    }
    Ok(())
}

fn status(args: StatusArgs) -> CliResult {
    let st = install::status(&args.prefix)?;
    if !st.exists {
        println!("{} — not an environment", st.prefix.display());
        return Ok(());
    }
    println!("{} — {} packages", st.prefix.display(), st.packages.len());
    for p in &st.packages {
        println!("  {p}");
    }
    Ok(())
}

fn remove(args: RemoveArgs) -> CliResult {
    install::remove_prefix(&args.prefix, args.force)?;
    println!("removed {}", args.prefix.display());
    Ok(())
}

fn activate(args: ActivateArgs) -> CliResult {
    let platform = args
        .platform
        .unwrap_or_else(|| Platform::current().to_string());
    let script = install::activation_script_for(&args.prefix, args.shell.as_deref(), &platform)?;
    print!("{script}");
    Ok(())
}

async fn pack(args: PackArgs) -> CliResult {
    let lock_bytes = std::fs::read(&args.lock)?;
    let summary = crate::pack::pack(&lock_bytes, &args.env, &args.platform, &args.output).await?;
    println!(
        "packed {} ({}) — {} packages, {:.1} MiB → {}",
        summary.environment,
        summary.platforms.join(", "),
        summary.packages,
        summary.bytes as f64 / (1024.0 * 1024.0),
        summary.output.display()
    );
    Ok(())
}

async fn unpack(args: UnpackArgs) -> CliResult {
    let summary = crate::pack::install_pack(
        &args.pack,
        args.env.as_deref(),
        args.platform.as_deref(),
        &args.prefix,
        args.stage_dir.as_deref(),
    )
    .await?;
    println!(
        "installed {} ({}) at {} — {} packages",
        summary.environment,
        summary.platform,
        summary.prefix.display(),
        summary.packages.len()
    );
    Ok(())
}

async fn sync(args: SyncArgs) -> CliResult {
    let project = crate::project::read(&args.project)?;
    let summary = crate::project::sync(&project).await?;
    println!(
        "synced {} ({}) at {} — {} packages",
        summary.environment,
        summary.platform,
        summary.prefix.display(),
        summary.packages.len()
    );
    Ok(())
}

async fn check(args: CheckArgs) -> CliResult {
    use crate::project::DependencyStatus;

    let project = crate::project::read(&args.project)?;
    let report = crate::project::check(&project, args.platform.as_deref()).await?;

    if report.dependencies.is_empty() {
        println!("no [project.dependencies] to check");
        return Ok(());
    }

    for dep in &report.dependencies {
        match &dep.status {
            DependencyStatus::Satisfied { found, .. } => {
                println!("  [ok]       {} (env pins {found})", dep.requirement)
            }
            DependencyStatus::Conflict {
                specifier, found, ..
            } => println!(
                "  [conflict] {} — env pins {found} (needs {specifier})",
                dep.requirement
            ),
            DependencyStatus::Missing { .. } => {
                println!("  [missing]  {} — not in env", dep.requirement)
            }
            DependencyStatus::Skipped { reason } => {
                println!("  [skip]     {} — {reason}", dep.requirement)
            }
        }
    }

    println!(
        "{} ok, {} conflict, {} missing, {} skipped",
        report.satisfied(),
        report.conflicts(),
        report.missing(),
        report.skipped()
    );

    if report.has_conflicts() {
        return Err(format!(
            "{} dependency conflict(s) with the environment",
            report.conflicts()
        )
        .into());
    }
    Ok(())
}

async fn run_env(args: RunArgs) -> CliResult {
    use crate::run::RunConfig;

    let config_path = args
        .config
        .unwrap_or_else(|| PathBuf::from("pyproject.toml"));
    let mut config = if config_path.extension().and_then(|e| e.to_str()) == Some("toml") {
        RunConfig::from_pyproject(&config_path)?
    } else {
        RunConfig::from_inline_script(&config_path)?
            .ok_or("script has no inline `# /// nepenthe` block")?
    };

    config.overlay_conda.extend(args.with);
    config.overlay_pip.extend(args.with_pip);
    if args.image {
        config.use_image = true;
    }
    if let Some(base) = args.base {
        config.image_base = Some(base);
    }
    if args.lazy {
        config.image_lazy = true;
    }
    if args.writable {
        config.image_writable = true;
    }
    if let Some(overlay) = args.overlay_image {
        config.image_overlay = Some(overlay);
    }
    if args.clone {
        config.use_clone = true;
    }
    // A trailing command (after `--`) fully overrides the config's command.
    if !args.command.is_empty() {
        config.command = args.command;
    }

    let summary = crate::run::run(&config, &[]).await?;
    if let Some(dest) = args.emit_overlay_lock {
        match &summary.overlay_lock {
            Some(src) => {
                std::fs::copy(src, &dest)?;
                eprintln!("wrote overlay lock to {}", dest.display());
            }
            None => {
                return Err(
                    "no overlay to lock (add --with / --with-pip or a config overlay)".into(),
                )
            }
        }
    }
    let location = summary
        .image
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| summary.prefix.display().to_string());
    eprintln!(
        "ran {} ({}, {} conda + {} pip overlay) in {}",
        config.environment,
        summary.platform,
        summary.overlay_packages,
        summary.pip_overlay,
        location
    );
    if !summary.status.success() {
        std::process::exit(summary.status.code().unwrap_or(1));
    }
    Ok(())
}

async fn try_solve(args: TryArgs) -> CliResult {
    use crate::manifest::{Manifest, Selector};
    use crate::solve::ChannelSettings;

    let yaml = recover_manifest_yaml(
        args.lock.as_deref(),
        args.env.as_deref(),
        args.registry.as_deref(),
        args.platform.as_deref(),
        args.python.as_deref(),
        args.variant.as_deref(),
        &args.label,
    )?;
    let manifest = Manifest::from_yaml_str(&yaml)?;
    let environment = args
        .environment
        .clone()
        .or_else(|| args.env.clone())
        .ok_or("pass --environment (the env name within the manifest)")?;

    let mut specs = args.with.clone();
    if let Some(project) = &args.project {
        let deps = crate::project::read_dependencies(project)?;
        specs.extend(crate::project::requirements_to_conda_specs(&deps));
    }
    if specs.is_empty() {
        return Err("nothing to try: pass --with <spec> and/or --project <pyproject.toml>".into());
    }

    let selector = Selector {
        variant: args.variant.clone(),
        python: args.python.clone(),
    };
    let settings = ChannelSettings::from_manifest(&manifest);
    let priority: crate::solve::ChannelPriorityMode = args.channel_priority.parse()?;

    println!("trial-solving '{environment}' with: {}", specs.join(", "));
    match crate::producer::trial_solve(
        &manifest,
        &environment,
        &selector,
        &specs,
        &settings,
        priority,
        args.platform.as_deref(),
        args.exclude_newer.clone(),
    )
    .await
    {
        Ok(outcome) => {
            println!(
                "ok — '{environment}' solves on {} with the added requirements",
                outcome.platform
            );
            let wanted: std::collections::BTreeSet<&str> = specs
                .iter()
                .map(|s| s.split_whitespace().next().unwrap_or(s))
                .collect();
            for package in &outcome.packages {
                if wanted.contains(package.name.as_str()) {
                    println!("  {} {}", package.name, package.version);
                }
            }
            Ok(())
        }
        Err(e) => Err(format!("unsatisfiable: {e}").into()),
    }
}

async fn shell(args: ShellArgs) -> CliResult {
    let registry = args.coords.build_registry();
    let coords = args.coords.coordinates();
    let label = Label::parse(&args.label);

    let prefix = match args.prefix {
        Some(p) => p,
        None => cache_env_prefix(&coords)?,
    };
    if !prefix.join("conda-meta").is_dir() {
        install::create(&registry, &coords, &label, &prefix).await?;
    }

    let shell_program = args
        .shell
        .or_else(|| std::env::var("SHELL").ok())
        .unwrap_or_else(|| "bash".to_string());
    eprintln!(
        "entering {} ({}) at {} — exit the shell to leave",
        coords.environment,
        coords.platform,
        prefix.display()
    );
    let status = install::exec_in_prefix(
        &prefix,
        &coords.platform,
        std::ffi::OsStr::new(&shell_program),
        &[],
        &[],
    )?;
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}

async fn image_build(args: ImageBuildArgs) -> CliResult {
    let registry = args.coords.build_registry();
    let coords = args.coords.coordinates();
    let label = Label::parse(&args.label);
    let prefix = match args.prefix {
        Some(p) => p,
        None => cache_env_prefix(&coords)?,
    };
    let target = match args.format {
        ImageFormat::Sif => {
            let output = args
                .output
                .ok_or("`--output <file>` is required for --format sif")?;
            crate::image::ImageTarget::Sif {
                output,
                lazy: args.lazy,
            }
        }
        ImageFormat::Oci => {
            if args.lazy {
                return Err("--lazy is only supported for --format sif".into());
            }
            if args.base.starts_with("localimage:") {
                return Err(
                    "`localimage:` bases are only supported for --format sif; for OCI, pass an image tag as --base".into(),
                );
            }
            let tag = args
                .tag
                .ok_or("`--tag <name:tag>` is required for --format oci")?;
            crate::image::ImageTarget::Oci { tag }
        }
    };
    let summary = crate::image::build(
        &registry,
        &coords,
        &label,
        &args.base,
        &prefix,
        &target,
        &args.label,
    )
    .await?;
    eprintln!(
        "built image for {} ({}) → {}",
        coords.environment, summary.platform, summary.artifact
    );
    Ok(())
}

fn list(args: ListArgs) -> CliResult {
    let registry = Registry::new(SpecStore::new(), args.registry);
    let index = registry.load_index()?;
    let environments = match &args.env {
        Some(env) => vec![env.clone()],
        None => index.environments(),
    };
    if environments.is_empty() {
        println!("registry is empty");
        return Ok(());
    }
    for environment in environments {
        let releases = index.releases_of(&environment);
        if releases.is_empty() {
            continue;
        }
        println!("{environment}");
        for release in releases {
            let mut axes = vec![release.platform.clone()];
            if let Some(py) = &release.python {
                axes.push(format!("py{py}"));
            }
            if let Some(variant) = &release.variant {
                axes.push(variant.clone());
            }
            println!("  {} [{}]", release.version, axes.join(", "));
        }
    }
    Ok(())
}

fn diff_versions(args: DiffVersionsArgs) -> CliResult {
    let registry = args.coords.build_registry();
    let coords = args.coords.coordinates();
    let env = &coords.environment;
    let platform = &coords.platform;

    let from_bytes = registry.pull(&coords, &Label::parse(&args.from))?;
    let to_bytes = registry.pull(&coords, &Label::parse(&args.to))?;
    let from_lock = install::parse_lock(&from_bytes)?;
    let to_lock = install::parse_lock(&to_bytes)?;
    let from_pkgs = install::lock_packages(&from_lock, env, platform)?;
    let to_pkgs = install::lock_packages(&to_lock, env, platform)?;

    // `diff_packages(desired, installed)`: desired = the newer (`to`) set,
    // installed = the older (`from`) set — so `added` are packages new in `to`.
    let d = install::diff_packages(&to_pkgs, &from_pkgs);
    if d.is_empty() {
        println!(
            "{env}: {} and {} are identical on {platform}",
            args.from, args.to
        );
        return Ok(());
    }
    println!("{env}: {} → {} ({platform})", args.from, args.to);
    for p in &d.added {
        println!("+ {} {}", p.name, p.version);
    }
    for p in &d.removed {
        println!("- {} {}", p.name, p.version);
    }
    for (from, to) in &d.changed {
        println!("~ {} {} -> {}", from.name, from.version, to.version);
    }
    Ok(())
}

async fn compose(args: ComposeArgs) -> CliResult {
    if args.envs.len() < 2 {
        return Err("compose needs at least two --env inputs".into());
    }
    let registry = Registry::new(SpecStore::new(), args.registry);
    let platform = args
        .platform
        .clone()
        .unwrap_or_else(|| Platform::current().to_string());

    // Pull and parse each input's lock, paired with its environment name.
    let mut inputs: Vec<(rattler_lock::LockFile, String)> = Vec::with_capacity(args.envs.len());
    for spec in &args.envs {
        let (env, version) = match spec.split_once('@') {
            Some((env, version)) => (env.to_string(), version.to_string()),
            None => (spec.clone(), "latest".to_string()),
        };
        let mut coords = Coordinates::new(env.clone(), platform.clone());
        if let Some(py) = &args.python {
            coords = coords.with_python(py.clone());
        }
        if let Some(variant) = &args.variant {
            coords = coords.with_variant(variant.clone());
        }
        let bytes = registry.pull(&coords, &Label::parse(&version))?;
        let lock = install::parse_lock(&bytes)?;
        inputs.push((lock, env));
    }

    let composed = crate::export::compose_lockfiles(&inputs, &args.name)?;
    let rendered = composed.render_to_string()?;
    std::fs::write(&args.output, rendered)?;
    eprintln!(
        "composed {} into '{}' ({}) → {}",
        args.envs.join(" + "),
        args.name,
        platform,
        args.output.display()
    );
    Ok(())
}

/// Recover an environment's manifest YAML from a lock file's embedded band, or
/// from a registry release (band then sidecar). Shared by `manifest` and `try`.
fn recover_manifest_yaml(
    lock: Option<&std::path::Path>,
    env: Option<&str>,
    registry_url: Option<&str>,
    platform: Option<&str>,
    python: Option<&str>,
    variant: Option<&str>,
    label: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    if let Some(lock_path) = lock {
        let bytes = std::fs::read(lock_path)?;
        return crate::embed::extract_manifest(&bytes)?
            .ok_or_else(|| "lock has no embedded manifest band".into());
    }
    let (Some(env), Some(registry_url)) = (env, registry_url) else {
        return Err("pass --lock <file>, or --env <name> --registry <url>".into());
    };
    let registry = Registry::new(SpecStore::new(), registry_url.to_string());
    let platform = platform
        .map(str::to_string)
        .unwrap_or_else(|| Platform::current().to_string());
    let mut coords = Coordinates::new(env.to_string(), platform);
    if let Some(py) = python {
        coords = coords.with_python(py.to_string());
    }
    if let Some(v) = variant {
        coords = coords.with_variant(v.to_string());
    }
    let label = Label::parse(label);
    let lock_bytes = registry.pull(&coords, &label)?;
    if let Some(yaml) = crate::embed::extract_manifest(&lock_bytes)? {
        return Ok(yaml);
    }
    let bytes = registry
        .pull_manifest(&coords, &label)?
        .ok_or("release has no manifest (no embedded band and no sidecar)")?;
    String::from_utf8(bytes).map_err(|e| format!("manifest is not valid UTF-8: {e}").into())
}

/// A cache prefix for a published environment, keyed by its coordinates.
fn cache_env_prefix(coords: &Coordinates) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let cache = rattler_cache::default_cache_dir().map_err(|e| e.to_string())?;
    let mut name = format!("{}-{}", coords.environment, coords.platform);
    if let Some(py) = &coords.python {
        name.push_str(&format!("-py{py}"));
    }
    if let Some(variant) = &coords.variant {
        name.push('-');
        name.push_str(variant);
    }
    Ok(cache.join("nepenthe-env").join(name))
}

fn cache_clean(all: bool) -> CliResult {
    let cache_dir = rattler_cache::default_cache_dir().map_err(|e| e.to_string())?;
    let package_cache = cache_dir.join(rattler_cache::PACKAGE_CACHE_DIR);
    if all {
        if package_cache.exists() {
            std::fs::remove_dir_all(&package_cache)?;
            println!("removed package cache at {}", package_cache.display());
        } else {
            println!(
                "package cache is already empty ({})",
                package_cache.display()
            );
        }
    } else {
        println!("package cache: {}", package_cache.display());
        println!("run `nepenthe cache clean --all` to remove it");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_parses_build_and_aliases() {
        use clap::Parser;

        // `nepenthe build` parses into the Build command.
        let cli = Cli::parse_from([
            "nepenthe",
            "build",
            "--manifest",
            "environment.yaml",
            "--env",
            "app",
            "--output-dir",
            "locks",
        ]);
        assert!(matches!(cli.command, Some(Command::Build(_))));

        // `npb` parses the same BuildArgs directly.
        let npb = NpbCli::parse_from([
            "npb",
            "--manifest",
            "environment.yaml",
            "--env",
            "app",
            "--output-dir",
            "locks",
        ]);
        assert_eq!(npb.args.env, "app");
        assert_eq!(npb.args.manifest, "environment.yaml");
    }

    #[test]
    fn build_publish_requires_version() {
        use clap::Parser;
        // --registry without --version is rejected (and vice versa).
        let result = NpbCli::try_parse_from([
            "npb",
            "--manifest",
            "m.yaml",
            "--env",
            "app",
            "--registry",
            "file:///srv/nepenthe",
        ]);
        assert!(result.is_err());
    }

    #[test]
    fn selects_build_only_for_npb_stem() {
        // Only the `npb` name (by file stem, path- and extension-insensitive)
        // picks the build shortcut.
        assert!(selects_build("npb"));
        assert!(selects_build("/usr/local/bin/npb"));
        assert!(selects_build("./target/release/npb"));
        assert!(selects_build("npb.exe"));
        assert!(!selects_build("nepenthe"));
        assert!(!selects_build("np"));
        assert!(!selects_build("/opt/tools/np"));
        assert!(!selects_build(""));
    }
}
