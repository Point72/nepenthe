# Motivation

nepenthe exists to make **large, shared software environments** reproducible,
fast to install, and safe to evolve — across many machines, many repositories,
and time. This page explains the problem it solves and why the common
alternatives fall short for that job.

## Frozen environments, without the pain

To behave consistently across many machines and over time, you generally want
**frozen environments** — an exact, pinned set of packages that installs the
same way everywhere. But pinning *everything* by hand makes upgrades miserable
and quickly leads to [dependency hell](https://en.wikipedia.org/wiki/Dependency_hell):
change one pin and you may have to re-pin a cascade of others.

nepenthe starts from a list of dependencies where you pin **only what you must**
— the things you actively care about or know have compatibility issues. Each
release **solves once** and freezes the result into a fully-pinned collection.

<br />

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/Point72/nepenthe/main/docs/img/frozen-environment-inverted.svg">
  <img width=440 src="https://raw.githubusercontent.com/Point72/nepenthe/main/docs/img/frozen-environment.svg" alt="A short, mostly-unpinned list of dependencies on the left is solved once into a fully-pinned environment on the right.">
</picture>

<br />

Every download of that release gives the **same** pinned packages, installed
**without re-solving** — so installs are fast and identical on every machine.

You evolve an environment by editing its **root** dependencies, not the solved
set. Each change produces a *new* frozen environment; old releases are never
regenerated, so they stay stable forever.

<br />

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/Point72/nepenthe/main/docs/img/environment-evolution-inverted.svg">
  <img width=440 src="https://raw.githubusercontent.com/Point72/nepenthe/main/docs/img/environment-evolution.svg" alt="Editing the root dependency list over time (arrow down the left) produces a new fully-pinned environment at each step (right).">
</picture>

<br />

Frequent releases provide upgrade paths, and teams move from older environments
to newer ones in whatever increment and on whatever timeline fits their needs.

## Why not just use…?

The crucial distinction: most tools are **project-focused** — they lock the
dependencies *of one repository*. nepenthe is **environment-focused** — it
builds and distributes a **shared environment that many repositories consume**.
That difference is why the usual options don't fit this job.

### Per-project lockfiles — pixi, uv

Tools like **pixi** (`pixi.lock`) and **uv** (`uv.lock`) are excellent at what
they do: lock the dependencies of a single project so its developers get a fast,
reproducible setup. nepenthe deliberately reuses the same building blocks
(rattler, the `rattler_lock` format) — but they solve the *opposite* half of the
problem:

- A lock is a property of **one project**. Locks from different repos are
  independent and aren't meant to be merged — combining two repos' environments
  may not even solve.
- There's no notion of a large **shared** environment (often the union of many
  teams' and projects' needs) that is solved once, versioned, and distributed
  centrally for many repos to install.
- Every repository re-derives and re-locks its own dependencies; there's no
  single blessed environment that fifty repos install and roll forward together
  (optionally, at their own pace).
- uv is **Python-only** — it can't manage the conda/C/C++/CUDA/native packages
  that scientific and ML stacks depend on.

**pixi locks a repo's environment; nepenthe produces and distributes the shared
environments that repos consume.** The two are complementary.

### Re-solved manifests — conda `environment.yml`

A conda `environment.yml` is a **manifest, not a lock**. `conda env create -f environment.yml` **re-solves every time**, which has two consequences:

- **Not reproducible.** Different machines, or the same machine at a different
  time, can get a different package set as channels evolve.
- **Slow.** You pay for a full solve on every install.

The usual workaround — hand-maintaining a huge, fully-pinned `environment.yml` —
is brittle: editing one pin can force you to re-pin a cascade of transitive
dependencies (dependency hell again). And a manifest gives you no independent
versioning, no immutability, no content-addressed integrity, and no
"solve once, distribute everywhere."

nepenthe keeps the friendly *input* (a short, mostly-unpinned dependency list)
but turns it into a frozen, versioned, installable **artifact** that never
re-solves on install.

### Language-scoped tools — pip, pipenv, poetry, venv

These are great **within Python**, but they're limited to Python. The moment a
package needs a C/C++/Rust library, CUDA, or another native dependency — which
is most of a real scientific or ML stack — you fall back to something else to
provide it. They can't manage the whole environment.

### Native package managers — vcpkg, conan, nix, guix

These handle C/C++/Rust well, but integrating them with Python is awkward, and
nix/guix in particular carry a steep learning curve and bespoke ecosystems that
are hard to adopt across an organization. nepenthe leans on the conda ecosystem
instead, which spans Python **and** native code and already models hard cases
like gRPC and CUDA.

### System packages — apt, dnf

System package managers change the machine **globally**: you can't easily run
multiple versions of an environment side by side, reverting is disruptive, and
nothing is portable to other hosts. That makes parallel production deployments
and air-gapped/colo installs painful — exactly the cases nepenthe targets.

### Container images — Docker / OCI

Container images are heavyweight and opaque: they rebuild per change and aren't a
lightweight, installable environment you can `diff`, `activate`, or share a
package cache across. They're complementary, not competing — a nepenthe lock can
be baked **into** an image, giving the image a reproducible, auditable
environment.

## How nepenthe is different

| Property                          | What nepenthe gives you                                                                            |
| --------------------------------- | -------------------------------------------------------------------------------------------------- |
| **Multi-language**                | The conda ecosystem — Python, C, C++, Rust, CUDA, native libs — in one environment                 |
| **Solve once, freeze**            | Reproducible everywhere and fast to install; installs never re-solve                               |
| **Environment-focused**           | A shared collection many repos consume, not a per-repo lock                                        |
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
