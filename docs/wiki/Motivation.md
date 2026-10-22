# Motivation

nepenthe builds shared software environments that multiple repositories and
machines can use. This page explains why those environments need frozen package
versions and how nepenthe compares with other tools.

## Frozen environments

Consistent behavior across machines and over time requires a frozen environment:
an exact, pinned set of packages that installs the same way everywhere.
Pinning every package by hand is tedious. Changing one pin can force changes to
many others, a problem known as
[dependency hell](https://en.wikipedia.org/wiki/Dependency_hell).

nepenthe starts from a dependency list with constraints for the versions you
need or known compatibility problems. Each release solves that list and freezes
the result into a fully pinned collection.

<br />

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/Point72/nepenthe/main/docs/img/frozen-environment-inverted.svg">
  <img width=440 src="https://raw.githubusercontent.com/Point72/nepenthe/main/docs/img/frozen-environment.svg" alt="A short, mostly-unpinned list of dependencies on the left is solved once into a fully-pinned environment on the right.">
</picture>

<br />

Every download of a release uses the same package versions. Installs skip
dependency solving.

To change an environment, edit its root dependencies and build a new release.
Existing releases are not regenerated.

<br />

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/Point72/nepenthe/main/docs/img/environment-evolution-inverted.svg">
  <img width=440 src="https://raw.githubusercontent.com/Point72/nepenthe/main/docs/img/environment-evolution.svg" alt="Editing the root dependency list over time (arrow down the left) produces a new fully-pinned environment at each step (right).">
</picture>

<br />

Teams can choose when to adopt a new release.

## Alternatives

Most tools lock the dependencies of one repository. nepenthe builds and
distributes a shared environment that many repositories consume.

### Per-project lockfiles: pixi, uv

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

### Re-solved manifests: conda `environment.yml`

A conda `environment.yml` is a manifest rather than a lock.
`conda env create -f environment.yml` re-solves every time, which has two
consequences:

- **Not reproducible.** Different machines, or the same machine at a different
  time, can get a different package set as channels evolve.
- **Slow.** Each install requires a full solve.

Maintaining a fully pinned `environment.yml` by hand is difficult: changing one
pin can require changes to transitive dependencies. The manifest alone also
does not provide versioning, immutability, or content-addressed integrity.

nepenthe solves a short dependency list and records the result in a versioned
lock. Consumers install from that lock without re-solving.

### Language-scoped tools: pip, pipenv, poetry, venv

These tools manage Python packages. Scientific and ML environments also need
C/C++/Rust libraries, CUDA, and other native dependencies, which these tools do
not manage. Another tool must provide those packages.

### Native package managers: vcpkg, conan, nix, guix

These handle C/C++/Rust well, but integration with Python is awkward. nix/guix
also have steep learning curves and bespoke ecosystems that can be difficult to
adopt across an organization. nepenthe uses the conda ecosystem, which covers
Python and native code, including packages such as gRPC and CUDA.

### System packages: apt, dnf

System package managers change the machine globally. Running multiple versions
of an environment side by side or reverting to an earlier version is difficult,
and the installed environment is not portable to other hosts. nepenthe installs
into separate prefixes for parallel deployments and supports bundles for
air-gapped or colocated hosts.

### Container images: Docker / OCI

Container images must be rebuilt when their contents change. They also do not
provide a lightweight package environment that you can `diff`, `activate`, or
use to share a package cache. They complement nepenthe: a nepenthe lock can be
baked into an image to give it a reproducible, auditable environment.

## How nepenthe is different

| Property                          | nepenthe behavior                                                                                   |
| --------------------------------- | --------------------------------------------------------------------------------------------------- |
| **Multi-language**                | Python, C, C++, Rust, CUDA, and other native libraries from the conda ecosystem in one environment  |
| **Solve once, freeze**            | Reproducible everywhere and fast to install; installs never re-solve                                |
| **Shared environments**           | Multiple repos consume the same environment                                                         |
| **Independently versioned**       | Each environment has its own semver sequence                                                        |
| **Immutable & content-addressed** | A published version never changes; rollback repoints a label; pulls are integrity-checked           |
| **Portable & isolated**           | Install in parallel, swap a symlink to revert, transfer bundles to restricted networks              |
| **Backend-agnostic**              | Specs and locks live on any backend (`file://`, `s3://`, `https://`); channels point at any server  |
| **Install without conda**         | A single binary installs from the lock into a prefix; conda, mamba, and micromamba are not required |

## Further reading

- [Concepts](Concepts): manifests, features, variants, locks, and registries.
- [Manifests](Manifests): define an environment.
- [Registry & Versioning](Registry): publish and resolve versions.
- [Installing Environments](Install): create a prefix from a lock.
