# Concepts

This page defines nepenthe's terms and how they relate, with links to detailed
documentation.

## Shared environments

Where a tool like pixi locks the dependencies of one repository, nepenthe builds
and distributes shared environments: collections of packages used by multiple
teams, solved once, versioned, and installed across
many repos and machines.

## Core terms

| Term               | Meaning                                                                                                                                                                                                           |
| ------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Manifest**       | A YAML description of channels, base dependencies, features, variants, and environments. See [Manifests](Manifests).                                                                                              |
| **Feature**        | A named, composable group of dependencies (e.g. `dev`, `ray`). Environments select features.                                                                                                                      |
| **Environment**    | A named composition of features for a set of platforms (e.g. `app = dev + ray`), built for each selected variant and Python version.                                                                              |
| **Variant**        | A build option an environment selects (e.g. `cpu` vs `gpu`), with its own dependencies, solver constraints, and virtual-package overrides.                                                                        |
| **Override layer** | A separate YAML file that adjusts version pins, variant constraints, virtual-package assumptions, and matrix `exclude`/`include` tables without editing the manifest. See [Manifests](Manifests#override-layers). |
| **Channel**        | Where _packages_ come from. Channel **names** in a manifest resolve to **URLs** via an alias and mirrors, so you can point at any server. See [Channels & Artifactory](Channels).                                 |
| **Spec backend**   | Where _specifications_ (manifests, overrides, locks) are read from / written to: `file://`, `s3://`, `https://`. See [Storage Backends](Backends).                                                                |
| **Lock**           | A `rattler_lock` lockfile recording the solved package set for installation without re-solving.                                                                                                                   |
| **Registry**       | A backend-hosted index mapping a `(environment, platform, python, variant, version)` to a content-addressed lock, enabling versioning and label resolution. See [Registry & Versioning](Registry).                |

## The build matrix

An environment has one name and can produce targets across two axes:

- **variant**, e.g. `cpu`, `gpu`
- **python**, e.g. `3.11`, `3.12`, `3.13`

Each `(variant × python)` combination is one **build cell**. A `Selector` picks
a cell; omitted axes fall back to declared defaults. `Manifest::targets(env)`
enumerates every cell, minus any pruned by the override `exclude`/`include`
tables.

## Composition

Resolving an environment unions, in order:

1. base `dependencies` (and `pypi-dependencies`),
1. every selected **feature**'s dependencies (including those inherited via
   `extends`),
1. the selected **variant**'s dependencies, constraints, and virtual packages,
1. the chosen **python** (injected as `python <ver>.*`).

Conda and PyPI dependencies are kept separate throughout. The result is a
`ResolvedEnvironment`, a single build cell ready to solve.

## Solving and installation

The build resolves dependencies and records the selected packages in a lock.
Installs read that lock without re-solving, so machines use the same package
versions for each target platform. Versions are assigned when publishing, not
when installing.

## Immutability & content addressing

A published lock is stored by its content address (`sha256-<hex>`) rather than by
its version. A version label points at a content address. This means:

- Several versions can refer to the same lock, avoiding duplicate storage.
- A published version cannot be changed.
- Repoint a label to roll back to an earlier release.
- A pull recomputes the hash and rejects content that does not match.

See [Registry & Versioning](Registry) for the full model.

## Rust examples

The `rust/examples/` directory contains runnable programs for manifest
composition, registry operations, and channel configuration:

```bash
cd rust
cargo run --example manifest_workflow          # compose + resolve a manifest
cargo run --example e2e_from_yaml              # load YAML fixtures → resolve → registry
cargo run --example registry_local             # publish/resolve/pull versions locally
cargo run --example channel_override_artifactory  # point channels at an internal Artifactory
```
