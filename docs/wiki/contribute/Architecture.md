# nepenthe Architecture

> **Audience: contributors and maintainers.** For end-user documentation, start
> at the [wiki home](Home).

This document describes how nepenthe is built: the crate layout, the module
seams, what is implemented today, and the key design decisions.

## Crate layout

nepenthe follows a **single-library, multi-module pattern**:

```
nepenthe-core (rust/)
├── manifest   — environment manifests, composition, feature/environment resolution, lints
├── backend    — SpecStore: read/write specs over fsspec_rs (file/S3/HTTP) + auth
├── solve      — solver core: repodata gateway, virtual packages, rattler solve, channel resolution
├── export     — locks + compatibility exports (@EXPLICIT, environment.yml)
├── registry   — versioned index, label resolution, immutable content-addressed publishing
├── install    — install a lock into a prefix (no conda), diff/status/remove/activate
├── embed      — embed/extract the manifest in a lock's leading comment band
├── pack       — air-gapped bundles: download a lock's packages, install offline
├── producer   — build orchestration: manifest → solve → locks → optional publish
├── project    — consumer pyproject.toml: [tool.nepenthe] sync + dependency check
├── run        — run a command in a base environment plus a conda overlay
├── name_map   — PyPI↔conda name mapping (grayskull-derived, vendored + regenerable)
├── cli        — clap CLI for the single multicall binary (build/create/pull/publish/show/diff/status/remove/activate/cache)
└── main       — nepenthe binary (thin: calls cli::run_multicall; argv[0] selects np/npb behaviour)

nepenthe (pyo3 cdylib, rust/python/lib.rs)
├── build / create / pull / publish / show / diff / status / remove / activate — the CLI surface, callable from Python
└── fsspec_pull / fsspec_publish — adapt a Python fsspec object to the Rust FileSystem trait
```

Each module is a **seam** that pins a key dependency and owns one concern:

| Module | Responsibility | Key deps |
|--------|-----------------|----------|
| `manifest` | YAML model, feature/environment composition, override layering, lints | `serde`, `serde_yaml` |
| `backend` | `SpecStore` dispatch by URL scheme, `AuthStore`, secret masking | `fsspec_rs`, `reqwest`, `url` |
| `solve` | repodata + solve, virtual packages, constraints, channel resolution | `rattler_repodata_gateway`, `rattler_solve`, `rattler_virtual_packages` |
| `export` | lock building + format exports + composing published locks | `rattler_lock` |
| `registry` | versioned index, label resolution, immutability | `semver`, `sha2`, `hex` |
| `install` | install a lock into a prefix, diff/status/remove, activation, activation-hook materialization, CoW clone | `rattler`, `rattler_shell`, `rattler_cache` |
| `embed` | manifest comment-band codec for lockfiles (gzip+base64) | `flate2`, `base64` |
| `pack` | bundle a lock's packages into one archive; install offline | `tar`, `reqwest`, `sha2` |
| `producer` | build orchestration: manifest → solve → locks → optional publish | (composes `manifest`/`solve`/`export`/`registry`) |
| `project` | read `[tool.nepenthe]` from `pyproject.toml`; sync + dependency check | `toml` |
| `run` | run a command in a base environment + conda/PyPI overlay, on host or in a SIF | `toml`, `sha2`, `uv` / `apptainer` (subprocess) |
| `image` | package a published environment into a SIF or OCI image | `apptainer` / `podman`/`docker` (subprocess) |
| `name_map` | PyPI↔conda name mapping; vendored divergent pairs + grayskull reducer | `serde_yaml` |
| `cli` | argument parsing + command dispatch for the multicall `nepenthe` binary (`np`/`npb` via argv[0]) | `clap`, `tokio` |

We **start consolidated** (one crate, modules as seams) and promote a module to
its own crate only if compile-time isolation or an external API boundary later
demands it — not preemptively.

## Modules

Each module below owns one concern; the descriptions reflect what is implemented
today.

### Manifest & composition
`manifest.rs`: serde YAML model (`Manifest`, `Project`, `Channel`, `Feature`,
`Variant`, `EnvironmentSpec`, `Overrides`); composition (`resolve`,
`resolve_default`, `targets`); `extends` inheritance; build-matrix axes
(variant × python); `Manifest::apply` override layering (variant merge, pin
baking, virtual-package + exclude/include recording, matrix pruning); validation lints
(`HardPin`, `TooManyHardPins`, `BaseFeatureCollision`). Imports are sandboxed
(absolute / `..` paths rejected).

### Solve core
`solve.rs`: `solve(&SolveRequest, &ChannelSettings) -> SolveOutcome` fetches
repodata via `rattler_repodata_gateway::Gateway` and solves with `rattler_solve`
(resolvo). Channel names resolve to URLs via `ChannelSettings` (alias + mirrors)
— pure-data rewriting, no Artifactory URL hardcoded in the library. Virtual
packages are host-detected **only when solving for the current platform** (a
cross-platform solve relies on explicit overrides). `solve_environment` drives
the full `(variant × python × platform)` matrix and fails fast
(`SolveError::Unsupported`) on non-empty PyPI deps (no PyPI resolver yet).

### Lock & exports
`export.rs`: `to_explicit_records` (`@EXPLICIT`, topologically sorted) and its
`to_explicit` (`SolveOutcome`) wrapper, `to_environment_yml` (serde-built,
returns `Result`), `to_lockfile` / `to_lockfile_string` (`rattler_lock` via
`LockFileBuilder`), and `to_multi_platform_lockfile` / `matrix_to_lockfiles`
(one lock spanning many platforms; one lock per build cell). The
lockfile round-trips (render → reparse → identical render).

### Spec backends
`backend.rs`: `SpecStore` over the `fsspec_rs::FileSystem` trait, dispatching by
URL scheme:
- `file://` → `LocalFs` (auto-mkdir)
- `s3://` → `S3Fs` (region/endpoint/keys from `AuthStore` or the ambient AWS chain; `AWS_ENDPOINT_URL` for S3-compatible stores)
- `https://` / `http://` → `ArtifactoryFs` (GET/PUT/DELETE; basic/bearer auth; 404→`NotFound`, 401/403→`PermissionDenied`; query strings preserved)

Credentials live in `AuthStore` (never in artifacts). `mask_url` redacts userinfo
on every error path, with a string-surgery fallback for URLs `Url::parse`
rejects. `Credential` has a **redacted `Debug`** (no secret ever printed).
Cleartext `http://` with configured credentials is refused
(`BackendError::InsecureScheme`). The pyo3 bridge (`rust/python/lib.rs`) adapts a
user-supplied Python `fsspec.AbstractFileSystem` to the same trait.

### Registry & versioning
`registry.rs`: a backend-hosted `index.yaml` is the source of truth.
`content_address(bytes)` is `sha256-<hex>`; a lock is written once under
`<root>/locks/<addr>.lock`. `Coordinates` `(environment, platform, python?,
variant?)` identify one version sequence; `Label` is `latest` /
`latest-but-one` / exact / semver range. `Registry::publish` is immutable
(identical resubmit = idempotent; different content = rejected).
`Registry::pull` validates the lock address (`sha256-<64 lowercase hex>`) and
**recomputes the content address**, rejecting tampered or corrupt bytes
(`IntegrityMismatch`) before they reach an installer.

### Manifest embedding (`embed` + registry sidecar)
A lock records what was solved but not the manifest it was solved *from*, which a
re-solve needs. The producer keeps the composed (post-override) manifest with
every build, recoverable two ways: **(A)** `embed.rs` bands it onto a written
lock file as a gzip+base64 leading comment (`# nepenthe:manifest+gzip+b64://…`) —
portable with a bare file, and ignored by pixi/rattler since leading comments are
valid YAML; **(B)** `publish_with_manifest` stores it as a content-addressed
sidecar under `<root>/manifests/<addr>.yaml` with `Release.manifest` pointing at
it — deduped across every cell/version that shares the manifest. `producer::build`
does A for `--output-dir` files and B for the registry. The `manifest` command /
`nepenthe.manifest` binding recover from either: try the lock's band, fall back
to the sidecar. The band's one caveat is that a foreign *re-render* of the lock
drops it (it isn't part of the lock structure); the sidecar is unaffected.

### Install / download side
`install.rs`: turns a published lock into a usable prefix **without conda**.
`install_lock` extracts a lock's `(environment × platform)` records and links
them into a prefix with rattler's `Installer` (shared package cache, parallel
fetch); `create` ties the registry to the installer (resolve label → pull →
install). `diff` (pure, offline) compares a lock's desired set against an
installed prefix; `status` reads the prefix's `conda-meta`; `remove` deletes a
prefix; `activation_script` renders a cross-platform activation script via
`rattler_shell`. On `create`, `write_activation_hooks` materializes the
environment's [`activation`](crate::manifest::Activation) block (recovered from
the lock's embedded manifest band, else the registry sidecar) into
`etc/conda/activate.d/nepenthe-activate.{sh,bat}`, injecting the env identity
(`NEPENTHE_ENVIRONMENT`/`PLATFORM`/`VERSION`) so a full activation runs the
hooks. The CLI lives in `cli.rs` as a single **multicall binary**:
`main.rs` is a thin shim calling `cli::run_multicall`, which reads argv[0] —
`npb` runs `nepenthe build`, any other name (`nepenthe`/`np`) runs the full CLI.
It exposes the consumer lifecycle — `create` / `pull` / `publish` / `show` /
`diff` / `status` / `remove` / `activate` / `cache clean` — plus the producer
`build` command. The build orchestration lives in `producer.rs` (`BuildRequest`
→ load manifest, apply any override layer, `solve_environment`,
`matrix_to_lockfiles`, then write one lock per cell (`--output-dir`) and/or
`registry.publish` each cell per platform coordinate (`--registry --version`)),
so the CLI and the Python binding share one code path. `np` and `npb` are
symlinks to the `nepenthe` binary (created post-build via the `aliases` make
target).

The **Python binding** (`rust/python/lib.rs`) wraps the same `producer` /
`install` / `registry` functions as `nepenthe.build` / `create` / `pull` /
`publish` / `show` / `diff` / `status` / `remove` / `activate` (plus the
`fsspec_pull` / `fsspec_publish` backend bridge). Each call builds a short-lived
tokio runtime and releases the GIL (`Python::detach`) while solving/installing;
results are returned as plain dicts/lists. See the [Python API](Python-API)
wiki page.

### Cross-platform & variants
The solve and lock layers are cross-platform by construction. `solve` derives
the virtual-package baseline from the **target** platform
(`VirtualPackages::detect_for_platform`), so a Linux host solving `win-64` /
`osx-arm64` / `linux-aarch64` gets that platform's `__win` / `__osx` / `__glibc`
etc. (overrides still win) — a reproducible cross-platform solve. Overrides for
the typed conda virtual packages (`cuda`, `archspec`, `glibc`, `osx`, `linux`,
`win`, `cuda_arch`) are routed through rattler's `VirtualPackageOverrides` so
each is encoded the conda way — e.g. `__cuda` carries the value in its version
(`__cuda=12.9=0`) while `__archspec` carries the microarchitecture in its build
string with version `1` (`__archspec=1=skylake_avx512`); a custom name falls
back to a version/build heuristic.
`export::to_multi_platform_lockfile` combines one `SolveOutcome` per platform
into a single lock under one environment; `matrix_to_lockfiles` groups the
`(variant × python × platform)` matrix into one multi-platform lock per build
cell. The install side extracts a chosen platform's records from that shared
lock, so `create --platform <p>` installs the right cell.

### Air-gapped bundles (`pack`/`unpack`)
`pack.rs` bundles everything an environment needs into one `.tar` for offline
install. `pack` resolves a lock, downloads each package, **verifies it against
the lock's sha256**, and writes `pkgs/<filename>` + the lock + a
`nepenthe-pack.yml` manifest (the outer tar is uncompressed since `.conda`
archives already are). `install_pack` extracts the bundle, rewrites each lock
record's `url` to the bundle's local `file://` copy, and calls the shared
`install::install_records` — rattler's installer reads `file://` packages via
`get_or_fetch_from_path`, so the install needs **no network and no conda** and
re-verifies the same sha256 while linking. The CLI exposes `pack` / `unpack` and
the Python binding exposes `nepenthe.pack` / `nepenthe.unpack`.

### Consumer project integration (`sync`/`check`)
`project.rs` reads a `[tool.nepenthe]` stanza from a consumer's `pyproject.toml`
(`toml` crate) — a **reference** (`environment`, `registry`, `version` label,
optional `platform`/`python`/`variant`/`prefix`), not an environment definition.
`sync` resolves the label and installs the published lock into the prefix
(`install::create`); it never re-solves the shared environment against the
project's deps — that would defeat "solve once, install identically". `check`
pulls the lock and compares each `[project.dependencies]` entry against the
pinned set: `check_dependencies` (pure, unit-tested) parses a PEP 508 requirement
to `(normalized name, version specifier)`, matches it by PEP 503-normalized name
against the conda packages, and tests the specifier with rattler's `VersionSpec`,
yielding satisfied / conflict / missing / skipped. The CLI exposes `sync` /
`check` (the latter exits non-zero on conflicts for CI) and the Python binding
exposes `nepenthe.sync` / `nepenthe.check`. Name matching is heuristic where a
PyPI name differs from its conda counterpart.

`name_map.rs` resolves those divergent names: it consults a vendored PyPI→conda
table (`src/data/pypi_to_conda.tsv`, `include_str!`-embedded) when a direct
normalized-name match fails (so `opencv-python` resolves to conda `opencv`). The
table holds only the **divergent** pairs (~few hundred) — conda-forge's grayskull
mapping has ~12k entries, but the ~11.7k identity ones are dropped since a direct
match handles them, keeping the artifact a few KB. `reduce_grayskull` is the pure
reducer (used by the `regenerate_name_map` example, which fetches the upstream
YAML and rewrites the table); its sorted output diffs cleanly, so the vendored
artifact is reproducible in-source.

### Run, overlays & ergonomics (`run`/`try`/`shell`/`list`)
`run.rs` backs `nepenthe run`: a `RunConfig` loaded from `[tool.nepenthe.run]` in
a `pyproject.toml` or an inline `# /// nepenthe` PEP 723-style block. A run pulls
the base lock, lays a **conda overlay** on top (solve the overlay specs with the
base packages pinned as `==` constraints, keep only the new packages, install the
union into a content-keyed cache prefix), then `install::exec_in_prefix` runs the
command with the prefix on `PATH` and `editable` dirs on `PYTHONPATH`. Overlay
channels are recovered from the base lock's embedded manifest. A **PyPI overlay**
(`overlay.pip` / `--with-pip`) is layered by shelling out to `uv`
(`uv pip install --python <prefix>`, found on `PATH` or via `NEPENTHE_UV`) — its
CLI is the stable integration surface, so we delegate rather than link uv's
internal crates; the install is keyed into the prefix once via a
`.nepenthe-pip-ready` marker. `install::exec_in_prefix` builds the activated
environment deterministically (prefix `bin` dirs prepended, `CONDA_PREFIX` set) —
path activation, no `activate.d`.

`producer::trial_solve` backs `nepenthe try`: recover an environment's manifest
(band or sidecar), inject extra conda specs (from `--with` and/or a project's
`[project.dependencies]` mapped PyPI→conda by `project::requirements_to_conda_specs`),
and re-solve the cell — a solver conflict means the env would not build with the
added requirement. `shell` reuses `exec_in_prefix` to drop into `$SHELL` in a
materialized prefix; `list` reads the registry index (`Index::environments` /
`releases_of`). The Python binding exposes `try_solve` and `list_releases`; `run`
and `shell` are CLI-only (they exec a process).

`image.rs` backs `nepenthe image build`: it materializes a published environment
into a prefix (`install::create`, all packages on disk) and packages it into an
Apptainer/SIF (`apptainer build`) or an **OCI image** (`podman`/`docker build`
from a generated `Containerfile`, engine via `NEPENTHE_OCI_ENGINE`). The
generated recipe bootstraps from a small **glibc** base (default
`debian:bookworm-slim`) — a `scratch` root cannot run conda-forge binaries, which
need the system loader and a `/bin/sh` — and copies the environment to the *same*
absolute path it occupies on the host, so conda's baked prefixes, shebangs, and
`RPATH`s resolve without relocation. `apptainer_definition` and `containerfile`
are pure, unit-tested functions. `nepenthe run --image` reuses `package_sif` +
`exec_in_image` to run inside a content-keyed SIF of the run prefix (overlays
baked in, editable dirs bind-mounted).

### Not yet implemented
- **Ecosystem & safety** — SBOM, CVE scan, `verify`, cache GC policies (`pack`/`unpack` are done).
- **lock signing & attestation**, **registry concurrent-publish (CAS)**, **foreign-format override adapters**.
- **Shared package CAS / cache daemon**, **registry web browse**, and **producer `pypi-dependencies`** end to end.

## Key design decisions

1. **Pure Rust, thin bindings.** Using the `rattler_*` crates directly avoids
   py-rattler overhead and yields a single static binary; the Python wheel is a
   thin pyo3 layer.
2. **Backends are pluggable.** The `fsspec_rs::FileSystem` trait dispatches on
   URL scheme; the same code path serves `file://`, `s3://`, and `https://`.
   Python `fsspec` objects plug in for backends without a Rust implementation.
3. **Credentials are injected, never baked in.** Secrets come from `AuthStore`
   at use time; artifacts and logs never carry them (redacted `Debug`,
   `mask_url`, cleartext-HTTP refusal).
4. **Content-addressed, immutable locks.** Locks are stored by `sha256-<hex>`,
   not by version; multiple versions can share one lock. Rollback repoints a
   label, never mutates a lock. Pulls are integrity-checked.
5. **Independent versioning.** Each `(environment, platform, python, variant)`
   has its own semver sequence — no global stamp.

## Testing strategy

- **Offline unit tests** live in each module's `#[cfg(test)] mod tests` and run
  with no network.
- **Integration tests** (`rust/tests/`) drive the public API against file-backed
  YAML fixtures in `rust/testdata/`.
- **Network/live tests** are gated behind `#[ignore]` (and often an env var) so
  CI stays offline; run them with `cargo test -- --ignored`.

### Current inventory (run `cargo test` to reproduce)

Library unit tests (`rust/src`): **98 test functions — 90 run offline, 8
`#[ignore]`** (network/live).

| Module | Offline | Ignored |
|--------|--------:|--------:|
| `manifest` | 35 | 0 |
| `solve` | 19 | 2 |
| `registry` | 11 | 1 |
| `backend` | 9 | 2 |
| `install` | 9 | 2 |
| `export` | 6 | 1 |
| `lib` | 1 | 0 |

Integration tests (`rust/tests`): **5**, all offline.

| File | Tests | What it covers |
|------|------:|----------------|
| `public_api_workflows.rs` | 2 | manifest + registry workflows via the public API |
| `e2e_yaml_workflow.rs` | 1 | load YAML fixtures → apply overrides → resolve matrix → registry round-trip |
| `channel_override_artifactory.rs` | 2 | channels overridden to an internal Artifactory (YAML + builder) |

Runnable examples (`rust/examples`, `cargo run --example <name>`):
`manifest_workflow`, `registry_local`, `e2e_from_yaml`,
`channel_override_artifactory`, `regenerate_name_map` (refreshes the vendored
PyPI→conda mapping from conda-forge's grayskull data; needs network).

## Developer commands

```bash
cd rust
cargo build                       # build the core crate + the nepenthe binary
cargo test                        # all offline unit + integration tests
cargo test -- --ignored           # also run network/live tests
cargo clippy --all-targets --all-features
cargo fmt --all -- --check

# the nepenthe CLI (producer build + consumer install/download lifecycle)
./target/debug/nepenthe --help
make aliases                      # create np/npb symlinks next to the binary
./target/debug/npb --help         # shortcut for `nepenthe build`

# from the repo root: build the pyo3 cdylib (target/debug/libnepenthe.so)
cargo build
```

See [Build from Source](Build-from-Source)
for the full build/lint/test workflow.

## Dependencies

- **Solver**: `rattler_repodata_gateway`, `rattler_solve`, `rattler_virtual_packages`, `rattler_conda_types`
- **Locks**: `rattler_lock`
- **Install**: `rattler` (installer), `rattler_shell` (activation), `rattler_cache` (package cache)
- **Storage**: `fsspec_rs` (local/S3), `reqwest` (HTTP), `url`
- **Versioning**: `semver`, `sha2`, `hex`
- **Data**: `serde`, `serde_yaml`
- **Async**: `tokio` (runtime for the CLI's solve/install commands)
- **Python bindings**: `pyo3`
- **CLI**: `clap`
