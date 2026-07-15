# Installing Environments

Once a lock is [published to a registry](Registry), nepenthe installs it into a
prefix **without conda** — packages are fetched into a shared cache and linked
into the target directory by rattler's installer. This page covers the consumer
lifecycle, available both through the `nepenthe` CLI and the Rust library.

## The CLI

```
nepenthe <command> [options]

  build     Solve a manifest into lock(s) and optionally publish them
  create    Create an environment from a published lock (no conda required)
  pull      Download a lock from a registry without installing it
  export    Render a lock as a conda `--file` (`@EXPLICIT`) spec
  manifest  Recover the manifest a lock was solved from
  publish   Publish a lock file to a registry under a version
  show      Show the release a label resolves to
  diff      Compare an installed prefix against a lock file
  status    Report what is installed in a prefix
  remove    Remove an environment prefix
  activate  Print an activation script for a prefix
  pack      Pack a lock's packages into a self-contained offline bundle
  unpack    Install an environment from a packed bundle (offline, no conda)
  sync      Install the environment a pyproject.toml references
  check     Check a project's dependencies against its referenced environment
  run       Run a command in a base environment plus an optional conda/PyPI overlay
  try       Trial-solve an environment with extra requirements, before publishing
  shell     Open an activated subshell in a published environment
  list      List the environments and releases in a registry
  diff-versions  Compare the package sets of two published releases
  compose   Compose several published environments into one lock
  image     Build a SIF or OCI image from a published environment
  cache     Manage the shared package cache
```

Two aliases ship with the CLI: `np` is a drop-in shortcut for `nepenthe`, and
`npb` is a shortcut for `nepenthe build` (so `npb --manifest … --env …` is the
same as `nepenthe build --manifest … --env …`). These are **symlinks** to a
single multicall `nepenthe` binary, which picks its behaviour from the invoked
name. `pip install` ships the `nepenthe` binary; the `np`/`npb` symlinks are
created post-build (the `aliases` make target), since wheel archives don't
preserve symlinks.

Registry-backed commands take a `--registry <root-url>` plus coordinates
(`--platform`, `--python`, `--variant`) and a `--label` (default `latest`).

### Build (producer)

`build` is the producer entry point: it loads a manifest, applies any override
layer, solves every cell of the requested environment (its variant × python
matrix), and writes and/or publishes one lock per cell.

```bash
# write one lock per cell to a directory
nepenthe build --manifest environment.yaml --env app --output-dir ./locks
# wrote ./locks/app-cpu-py3.11.lock (1 platforms)
# wrote ./locks/app-gpu-py3.11.lock (1 platforms)

# …or publish them straight to a registry under a version (needs --version)
nepenthe build --manifest environment.yaml --env app \
  --registry file:///srv/nepenthe --version 1.0.0
# published app 1.0.0 on linux-64 → sha256-4b4d4e3d…

# apply an override layer and pin the repodata for a reproducible solve
nepenthe build --manifest environment.yaml --overrides prod.yaml --env app \
  --output-dir ./locks --channel-priority strict --exclude-newer 2025-01-01T00:00:00Z
```

Pass `--output-dir`, `--registry --version`, or both; at least one is required.
Lock filenames encode the cell as `<env>[-<variant>][-py<python>].lock`. When
publishing a multi-platform cell, the lock is registered under each platform
coordinate (content-addressed, so the same lock is stored once).

By default `build` solves the environment's whole matrix. Pass `--python` and/or
`--variant` to build just one cell (or a slice of it) — useful for fanning the
matrix out across independent CI jobs so one unsolvable cell doesn't fail the
rest:

```bash
# build only the Python 3.11 cell(s) of app
nepenthe build --manifest environment.yaml --env app --python 3.11 --output-dir ./locks
# build exactly the gpu / 3.12 cell
nepenthe build --manifest environment.yaml --env app --variant gpu --python 3.12 --output-dir ./locks
```

A `--python` (or `--variant`) that selects no cell — e.g. one the override
layer's `exclude` table removes — is reported as "produced no build cells" so a
caller can treat it as a skipped cell.

### Create

Resolve a label, pull the lock, and install it — all in one step:

```bash
nepenthe create app \
  --registry file:///srv/nepenthe \
  --platform linux-64 --python 3.11 \
  --label latest \
  --prefix ./envs/app
# created app (linux-64) at ./envs/app — 42 packages
```

No conda, mamba, or micromamba is required on the machine.

If the environment declares [`activation`](Manifests#activation) hooks, `create`
also materializes them into `etc/conda/activate.d/` (recovered from the manifest
the lock was solved from), so a subsequent [`activate`](#activate) runs them.

### Publish, show, pull

```bash
# publish a lock you exported earlier
nepenthe publish app --registry file:///srv/nepenthe \
  --platform linux-64 --python 3.11 --version 1.2.0 --lock app.lock

# see what a label resolves to
nepenthe show app --registry file:///srv/nepenthe --platform linux-64 --python 3.11
# version: 1.2.0 …

# download a lock without installing
nepenthe pull app --registry file:///srv/nepenthe \
  --platform linux-64 --python 3.11 -o app.lock
```

### Export a conda `@EXPLICIT` spec

For people who install with conda/mamba rather than nepenthe, render a lock as a
`conda create --file` spec (`@EXPLICIT`: the header plus each package URL,
topologically sorted). Source the lock from a local file or straight from the
registry:

```bash
# from a local lock file
nepenthe export --env app --lock app.lock --platform linux-64 -o app.txt

# or resolve it from the registry
nepenthe export --env app --registry file:///srv/nepenthe \
  --platform linux-64 --python 3.11 --label 1.2.0 -o app.txt

# then install with conda/mamba (no nepenthe required)
conda create -n app --file app.txt
```

Omit `-o` to write the spec to stdout. The exported URLs are bare (no embedded
credentials); private channels are authenticated at download time the same way
`nepenthe create` is — see [Channels](Channels#authentication).

### Inspect: status and diff

```bash
# what is installed in a prefix?
nepenthe status --prefix ./envs/app

# how does a prefix differ from a lock?
nepenthe diff --lock app.lock --env app --platform linux-64 --prefix ./envs/app
# + ruff=0.6.0=h2          (in the lock, not installed)
# - pip=24.0=h3            (installed, not in the lock)
# ~ numpy=2.1.0=OLD -> numpy=2.1.0=h0   (changed)
```

`diff` and `status` are pure, offline operations — they read the prefix's
`conda-meta` directory and compare against the lock.

### Activate

Generate a cross-platform activation script (sets `PATH`, exports the
environment's variables, runs its `activate.d` hooks):

```bash
nepenthe activate --prefix ./envs/app          # current shell
nepenthe activate --prefix ./envs/app --shell fish
eval "$(nepenthe activate --prefix ./envs/app)"  # activate in the current shell
```

### Cache

```bash
nepenthe cache clean          # show the shared package-cache location
nepenthe cache clean --all    # remove it
```

## From the library

The same operations are available as functions in `nepenthe_core::install`:

```rust
use std::path::Path;
use nepenthe_core::backend::SpecStore;
use nepenthe_core::install;
use nepenthe_core::registry::{Coordinates, Label, Registry};

let registry = Registry::new(SpecStore::new(), "file:///srv/nepenthe");
let coords = Coordinates::new("app", "linux-64").with_python("3.11");

// resolve latest → pull → install (async; needs a tokio runtime)
let summary = install::create(&registry, &coords, &Label::Latest, Path::new("./envs/app")).await?;

// inspect
let st = install::status(Path::new("./envs/app"))?;
let lock = install::parse_lock(&registry.pull(&coords, &Label::Latest)?)?;
let d = install::diff(&lock, "app", "linux-64", Path::new("./envs/app"))?;

// remove
install::remove_prefix(Path::new("./envs/app"))?;
```

`install_lock(&lock, env, platform, prefix)` installs a lock you already have in
memory; `create` is the registry-driven convenience that pulls it first.

## How it works

- **No conda dependency.** Packages are linked into the prefix by rattler's
  installer, which fetches into a shared package cache (parallel downloads,
  hardlink dedup) — there is no shell-out to conda/mamba/micromamba.
- **Reproducible.** The lock is the contract; install never re-solves, so every
  machine gets a byte-identical environment.
- **Cross-platform activation.** `rattler_shell` renders activation for bash,
  zsh, fish, xonsh, cmd, PowerShell, and nushell, so there are no bash-only
  hooks.
