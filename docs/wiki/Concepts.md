# Concepts

This page defines nepenthe's vocabulary and how the pieces relate. Each term
links to a deeper page where relevant.

## The big picture

nepenthe is **environment-focused**, not repo-focused. Where a tool like pixi
locks the dependencies _of one repository_, nepenthe builds and distributes
**shared environments** — curated collections of packages, often the union of
many teams' needs — that are solved once, versioned, and installed across many
repos and machines.

## Core terms

| Term               | Meaning                                                                                                                                                                                                                                 |
| ------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Manifest**       | The declarative YAML description of one or more environments — channels, base dependencies, features, variants, and environments. See [Manifests](Manifests).                                                                           |
| **Feature**        | A named, composable group of dependencies (e.g. `dev`, `ray`). Environments select features.                                                                                                                                            |
| **Environment**    | A named composition of features for a set of platforms (e.g. `app = dev + ray`). Fans out over a build matrix.                                                                                                                          |
| **Variant**        | A build flavor an environment selects (e.g. `cpu` vs `gpu`), carrying its own dependencies, solver constraints, and virtual-package overrides.                                                                                          |
| **Override layer** | A separate, pullable YAML file that adjusts a solve — version pins, variant constraints, virtual-package assumptions, and matrix `exclude`/`include` tables — without editing the manifest. See [Manifests](Manifests#override-layers). |
| **Channel**        | Where _packages_ come from. Channel **names** in a manifest resolve to **URLs** via an alias and mirrors, so you can point at any server. See [Channels & Artifactory](Channels).                                                       |
| **Spec backend**   | Where _specifications_ (manifests, overrides, locks) are read from / written to: `file://`, `s3://`, `https://`. See [Storage Backends](Backends).                                                                                      |
| **Lock**           | The frozen solve output — a `rattler_lock` lockfile. The primary, reproducible artifact.                                                                                                                                                |
| **Registry**       | A backend-hosted index mapping a `(environment, platform, python, variant, version)` to a content-addressed lock, enabling versioning and label resolution. See [Registry & Versioning](Registry).                                      |

## The build matrix

An environment keeps **one name** and fans out over two axes:

- **variant** — e.g. `cpu`, `gpu`
- **python** — e.g. `3.11`, `3.12`, `3.13`

Each `(variant × python)` combination is one **build cell**. A `Selector` picks
a cell; omitted axes fall back to declared defaults. `Manifest::targets(env)`
enumerates every cell (minus any pruned by the override `exclude`/`include` tables).

## Composition

Resolving an environment unions, in order:

1. base `dependencies` (and `pypi-dependencies`),
1. every selected **feature**'s dependencies (including those inherited via
   `extends`),
1. the selected **variant**'s dependencies, constraints, and virtual packages,
1. the chosen **python** (injected as `python <ver>.*`).

Conda and PyPI dependencies are kept separate throughout. The result is a
`ResolvedEnvironment` — a single build cell ready to solve.

## Solve once, freeze, never re-solve

The **lock** is the contract. Solving happens once, at publish time, against a
pinned view of the channels. Installs read the lock and never re-solve, so every
machine gets a byte-identical environment. Versioning happens at solve time, not
install time.

## Immutability & content addressing

A published lock is stored by its **content address** (`sha256-<hex>`), not by
its version. A version label points at a content address. This means:

- the same lock can back several versions (dedup),
- a published version never changes underneath you,
- rollback is just repointing a label,
- a pull recomputes the hash and rejects tampered bytes.

See [Registry & Versioning](Registry) for the full model.

## Trying it today

nepenthe is currently a Rust library. The `rust/examples/` directory has small,
runnable programs that exercise these concepts end to end:

```bash
cd rust
cargo run --example manifest_workflow          # compose + resolve a manifest
cargo run --example e2e_from_yaml              # load YAML fixtures → resolve → registry
cargo run --example registry_local             # publish/resolve/pull versions locally
cargo run --example channel_override_artifactory  # point channels at an internal Artifactory
```
