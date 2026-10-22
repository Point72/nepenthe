<a href="https://github.com/point72/nepenthe">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://github.com/point72/nepenthe/raw/main/docs/img/logo-dark.png?raw=true">
    <img alt="nepenthe logo, a greek-style pitcher jar with blue liquid" src="https://github.com/point72/nepenthe/raw/main/docs/img/logo-light.png?raw=true" width="400">
  </picture>
</a>

Forget your environment sorrows

[![Build Status](https://github.com/Point72/nepenthe/actions/workflows/build.yaml/badge.svg?branch=main&event=push)](https://github.com/Point72/nepenthe/actions/workflows/build.yaml)
[![codecov](https://codecov.io/gh/Point72/nepenthe/branch/main/graph/badge.svg)](https://codecov.io/gh/Point72/nepenthe)
[![License](https://img.shields.io/github/license/Point72/nepenthe)](https://github.com/Point72/nepenthe)
[![PyPI](https://img.shields.io/pypi/v/nepenthe.svg)](https://pypi.python.org/pypi/nepenthe)

**nepenthe** builds, versions, and distributes large shared conda/PyPI
environments. It solves a dependency manifest with
[rattler](https://github.com/conda/rattler) and records the result in a portable
lockfile. Publish that lock to a versioned registry so other machines can install
the same packages without re-solving.

- **Manifests** describe dependencies and compose environments.
- **Rust solver** uses rattler without a Python interpreter.
- **Multi-platform locks** store `linux-64`, `osx-arm64`, and `win-64` environments in one `rattler_lock` file, solved from one host.
- **Storage backends** read and write specs using `file://`, `s3://`, and `https://`; package channels can use servers such as Artifactory.
- **Versioned registry** tracks each environment independently and stores immutable, content-addressed locks.
- **Installation** links packages into a prefix with rattler; conda, mamba, and micromamba are not required.
- **Conda export** converts a lock to an `@EXPLICIT` spec for `conda create --file`.
- **Credentials** are supplied at use time and never written into artifacts.

The CLI and Python API support building, publishing, and installing environments.
Use `pack` / `unpack` bundles for air-gapped machines and `pyproject.toml`
integration to check a project's dependencies against its shared environment.

## Why nepenthe?

Frozen environments keep package versions consistent across machines and over
time. Maintaining every pin by hand makes upgrades difficult and can lead to
[dependency hell](https://en.wikipedia.org/wiki/Dependency_hell).

With nepenthe, you specify dependencies and the version constraints you need.
Each release solves that list and freezes the result. Consumers install those
packages without re-solving.

<br />

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/Point72/nepenthe/refs/heads/main/docs/img/frozen-environment-inverted.svg">
  <img width=800 src="https://raw.githubusercontent.com/Point72/nepenthe/refs/heads/main/docs/img/frozen-environment.svg" alt="An unpinned list of dependencies is solved once into a fully-pinned environment.">
</picture>

<br />

To update an environment, edit its root dependencies and build a new release.
Teams can choose when to adopt it.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/Point72/nepenthe/refs/heads/main/docs/img/environment-evolution-inverted.svg">
  <img width=800 src="https://raw.githubusercontent.com/Point72/nepenthe/refs/heads/main/docs/img/environment-evolution.svg" alt="Editing the root dependency list over time (arrow down the left) produces a new fully-pinned environment at each step (right).">
</picture>

Unlike per-project lockfiles (pixi, uv) and re-solved manifests
(`environment.yml`), nepenthe distributes versioned, immutable environments for
multiple repos to share. See
[Motivation](docs/wiki/Motivation.md) for the full comparison.

## Documentation

| For                                             | Read                                                                                                                                                                                                                                                                                                                                                                                      |
| ----------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Why it exists:** the problem and alternatives | [Motivation](docs/wiki/Motivation.md)                                                                                                                                                                                                                                                                                                                                                     |
| **Users:** concepts and how-to guides           | The [wiki](docs/wiki/Home.md): [Concepts](docs/wiki/Concepts.md), [Manifests](docs/wiki/Manifests.md), [Channels & Artifactory](docs/wiki/Channels.md), [Registry](docs/wiki/Registry.md), [Installing](docs/wiki/Install.md), [Consuming in a Project](docs/wiki/Projects.md), [Packing](docs/wiki/Packing.md), [Python API](docs/wiki/Python-API.md), [Backends](docs/wiki/Backends.md) |
| **Getting started**                             | [Installation](docs/wiki/Installation.md)                                                                                                                                                                                                                                                                                                                                                 |
| **Contributors:** how it's built                | [Architecture](docs/wiki/contribute/Architecture.md)                                                                                                                                                                                                                                                                                                                                      |

## Quickstart

Create a manifest with the dependencies and version constraints you need:

```yaml
# environment.yaml
project:
  name: demo
  channels: [conda-forge]
  platforms: [linux-64]
  python: ["3.11"]

dependencies:
  - numpy >=2
  - pip

environments:
  app: []
```

Build a fully pinned lockfile without a registry or conda:

```bash
nepenthe build --manifest environment.yaml --env app --output-dir .
# → app-py3.11.lock      the reproducible artifact: every install gives these exact packages
```

Export the lock as an `@EXPLICIT` spec for installation with conda or mamba:

```bash
nepenthe export --env app --lock app-py3.11.lock --platform linux-64 -o app.txt
# → app.txt              @EXPLICIT list of package URLs
```

Install from the spec with conda, or publish the lock and install with nepenthe:

```bash
# with conda/mamba, from the spec — nepenthe not needed on this machine
conda create --name app --file app.txt

# …or publish the lock to a versioned registry and install with nepenthe — no conda:
nepenthe build --manifest environment.yaml --env app \
  --registry file:///srv/nepenthe --version 1.0.0
nepenthe create app --registry file:///srv/nepenthe --python 3.11 --prefix ./envs/app
eval "$(nepenthe activate --prefix ./envs/app)"
```

The CLI is a single multicall binary: `np` and `npb` are symlinks to `nepenthe`
(dispatch on the invoked name). `np` behaves exactly like `nepenthe`, and `npb`
is a shortcut for `nepenthe build` (so the build step above is just
`npb --manifest environment.yaml --env app --output-dir .`).

The Python API provides the same operations without requiring conda:

```python
import nepenthe

# producer: solve "app" and publish its lock as v1.0.0
nepenthe.build("environment.yaml", "app", registry="file:///srv/nepenthe", version="1.0.0")

# consumer: install the latest published lock into a prefix
nepenthe.create("app", "file:///srv/nepenthe", "./envs/app")
print(nepenthe.activate("./envs/app"))
```

A repo can pin its shared environment in `pyproject.toml` and check that its
dependencies are compatible:

```toml
[tool.nepenthe]
environment = "app"
registry = "file:///srv/nepenthe"
version = "1.0.0"
```

```bash
nepenthe sync      # install the referenced environment into .venv
nepenthe check     # verify [project.dependencies] are compatible (fails on conflicts)
```

See [Installing Environments](docs/wiki/Install.md) for the complete CLI
reference, [Consuming in a Project](docs/wiki/Projects.md) for the
`pyproject.toml` workflow, [Python API](docs/wiki/Python-API.md) for the bindings,
[Manifests](docs/wiki/Manifests.md) for the manifest format, and the
runnable [`rust/examples/`](rust/examples) for the producer side.

## Contributing

Contributions are welcome under the [Apache 2.0 license](LICENSE). See
[Contributing](docs/wiki/contribute/Contribute.md) and
[Build from Source](docs/wiki/contribute/Build-from-Source.md).

## License

Apache-2.0. See [LICENSE](LICENSE) for details.
