# Motivation

nepenthe exists to make large, shared software environments reproducible, fast
to install, and safe to change, across many machines, many repositories, and
time. This page explains the problem nepenthe solves and how common alternatives
compare.

## Frozen environments without the pain

Consistent behavior across machines and over time requires a frozen environment:
an exact, pinned set of packages that installs the same way everywhere.
Pinning every package by hand is tedious. Changing one pin can force changes to
many others, a problem known as
[dependency hell](https://en.wikipedia.org/wiki/Dependency_hell).

nepenthe starts from a dependency list that pins only what it has to: the things
you actively care about, or ones with known compatibility problems. Each release
solves that list once and freezes the result into a fully-pinned collection.

<br />

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/Point72/nepenthe/main/docs/img/frozen-environment-inverted.svg">
  <img width=440 src="https://raw.githubusercontent.com/Point72/nepenthe/main/docs/img/frozen-environment.svg" alt="A short, mostly-unpinned list of dependencies on the left is solved once into a fully-pinned environment on the right.">
</picture>

<br />

Every download of a release gives you the same packages, installed without
re-solving, so installs are fast and identical on every machine.

To change an environment you edit its root dependencies, not the solved set.
Each change produces a new frozen environment. Old releases are never
regenerated, so they stay stable indefinitely.

<br />

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/Point72/nepenthe/main/docs/img/environment-evolution-inverted.svg">
  <img width=440 src="https://raw.githubusercontent.com/Point72/nepenthe/main/docs/img/environment-evolution.svg" alt="Editing the root dependency list over time (arrow down the left) produces a new fully-pinned environment at each step (right).">
</picture>

<br />

Frequent releases provide upgrade paths, and teams move from older environments
to newer ones in whatever increment and on whatever timeline fits their needs.

## Why not just use…?

Most tools lock the dependencies of one repository. nepenthe builds and
distributes a shared environment that many repositories consume. That difference
rules out most of the usual options.

### Per-project lockfiles — pixi, uv

[pixi](https://pixi.sh) (`pixi.lock`) and [uv](https://docs.astral.sh/uv/)
(`uv.lock`) lock the dependencies of a single project so its developers get a
fast, reproducible setup. nepenthe reuses the same building blocks (rattler, the
`rattler_lock` format) for shared environments:

- A lock belongs to one project. Locks from different repos are independent and
  aren't designed to be merged; combining two repos' environments may not solve.
- These tools have no model for a shared environment that combines the needs of
  multiple teams and projects, is solved once, then versioned and distributed to
  many repos.
- Each repository solves and locks its dependencies independently. Teams cannot
  publish one shared environment and let repositories upgrade on their own
  schedules.
- uv is Python-only, so it can't manage the conda, C/C++, or CUDA packages that
  scientific and ML stacks depend on.

pixi locks a repo's environment; nepenthe produces and distributes the shared
environments that repos consume. The two are complementary.

### Re-solved manifests — conda `environment.yml`

A conda `environment.yml` is a manifest rather than a lock.
`conda env create -f environment.yml` re-solves every time, which has two
consequences:

- **Not reproducible.** Different machines, or the same machine at a different
  time, can get a different package set as channels evolve.
- **Slow.** You pay for a full solve on every install.

The usual workaround, hand-maintaining a huge fully-pinned `environment.yml`, is
brittle: editing one pin can force you to re-pin a cascade of transitive
dependencies (dependency hell again). And a manifest gives you no independent
versioning, no immutability, no content-addressed integrity, and no way to solve
once and distribute everywhere.

nepenthe keeps the friendly input, a short, mostly-unpinned dependency list, and
turns it into a frozen, versioned, installable artifact that never re-solves on
install.

### Language-scoped tools — pip, pipenv, poetry, venv

These tools manage Python packages. Scientific and ML environments also need
C/C++/Rust libraries, CUDA, and other native dependencies, which these tools do
not manage. Another tool must provide those packages.

### Native package managers — vcpkg, conan, nix, guix

These handle C/C++/Rust well, but integration with Python is awkward. nix/guix
also have steep learning curves and bespoke ecosystems that can be difficult to
adopt across an organization. nepenthe uses the conda ecosystem, which covers
Python and native code, including packages such as gRPC and CUDA.

### System packages — apt, dnf

System package managers change the machine globally: you can't easily run
multiple versions of an environment side by side, reverting is disruptive, and
nothing is portable to other hosts. That makes parallel production deployments
and air-gapped or colo installs painful, which are exactly the cases nepenthe
targets.

### Container images — Docker / OCI

Container images must be rebuilt when their contents change. They also do not
provide a lightweight package environment that you can `diff`, `activate`, or
use to share a package cache. They complement nepenthe: a nepenthe lock can be
baked into an image to give it a reproducible, auditable environment.

## How nepenthe is different

| Property                          | What nepenthe gives you                                                                            |
| --------------------------------- | -------------------------------------------------------------------------------------------------- |
| **Multi-language**                | The conda ecosystem — Python, C, C++, Rust, CUDA, native libs — in one environment                 |
| **Solve once, freeze**            | Reproducible everywhere and fast to install; installs never re-solve                               |
| **Shared, not per-repo**          | A collection many repos consume, rather than a lock belonging to one of them                       |
| **Independently versioned**       | Each environment has its own semver sequence — no global stamp, no filename encoding               |
| **Immutable & content-addressed** | A published version never changes; rollback repoints a label; pulls are integrity-checked          |
| **Portable & isolated**           | Install in parallel, swap a symlink to revert, sync wholesale to restricted networks               |
| **Backend-agnostic**              | Specs and locks live on any backend (`file://`, `s3://`, `https://`); channels point at any server |
| **Install without conda**         | A single binary links the lock into a prefix — no conda/mamba/micromamba on the target             |

## Where next?

- **[Concepts](Concepts)** — the vocabulary (manifests, features, variants, locks, registry).
- **[Manifests](Manifests)** — author an environment.
- **[Registry & Versioning](Registry)** — publish and resolve versions.
- **[Installing Environments](Install)** — create a prefix from a lock.
