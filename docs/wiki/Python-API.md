# Python API

nepenthe ships a Python extension (built in Rust with [pyo3](https://pyo3.rs))
that mirrors the [CLI](Install): the same producer `build` and consumer
lifecycle, callable from Python with **no conda required**.

```python
import nepenthe

nepenthe.version()            # the nepenthe-core version, e.g. "0.1.0"
nepenthe.current_platform()   # this machine's conda subdir, e.g. "linux-64"
```

Every function maps to a CLI command. Paths accept `str` or `os.PathLike`.
Registry-backed functions take a `registry` root URL plus coordinates
(`platform`, `python`, `variant`) and a `label` (default `"latest"`); `platform`
defaults to the current platform. Functions that solve or install perform I/O
and release the GIL while they work.

## Producer: `build`

Solve a manifest environment into one lock per build cell, writing the locks to
`output_dir` and/or publishing them to a `registry` under a `version`. At least
one destination is required. Returns one dict per built cell.

```python
cells = nepenthe.build(
    "environment.yaml",
    "app",
    overrides="prod.yaml",        # optional override layer
    output_dir="locks",           # write locks here…
    registry="file:///srv/nepenthe",  # …and/or publish here (needs version=)
    version="1.0.0",
    channel_priority="strict",    # or "disabled"
    exclude_newer="2025-01-01T00:00:00Z",  # optional RFC3339 repodata cutoff
)

for cell in cells:
    print(cell["cell"], cell["platforms"], cell["lock_path"])
    for release in cell["releases"]:
        print("  published", release["version"], "→", release["lock"])
```

Each cell dict has: `cell` (the `<env>[-<variant>][-py<python>]` stem),
`variant`, `python`, `platforms`, `lock_path` (or `None`), and `releases` (a
list of release dicts, one per platform published).

## Consumer lifecycle

```python
# resolve latest → pull → install into a prefix (no conda)
summary = nepenthe.create(
    "app", "file:///srv/nepenthe", "./envs/app",
    platform="linux-64", python="3.11", variant="cpu",
)
print(summary["packages"])     # the installed package set

# download a lock without installing it
n = nepenthe.pull("app", "file:///srv/nepenthe", "app.lock", python="3.11")

# publish a lock you exported earlier
release = nepenthe.publish(
    "app", "file:///srv/nepenthe", "1.2.0", "app.lock", python="3.11",
)

# see what a label resolves to
release = nepenthe.show("app", "file:///srv/nepenthe", python="3.11")

# compare an installed prefix against a lock
d = nepenthe.diff("app.lock", "app", "./envs/app", platform="linux-64")
if not d["up_to_date"]:
    print(d["added"], d["removed"], d["changed"])

# report what is installed
status = nepenthe.status("./envs/app")

# render an activation script (defaults to the current shell + platform)
script = nepenthe.activate("./envs/app", shell="bash")

# remove a prefix
nepenthe.remove("./envs/app")
```

## Air-gapped bundles

Pack a lock's packages into one offline bundle, then install from it with no
network (see [Packing](Packing)):

```python
# producer host: bundle every package the lock pins
summary = nepenthe.pack("app.lock", "app", "app.tar")          # all platforms
summary = nepenthe.pack("app.lock", "app", "app.tar", platforms=["linux-64"])

# air-gapped host: install offline from the bundle
nepenthe.unpack("app.tar", "./envs/app")
nepenthe.unpack("app.tar", "./envs/app", env="app", platform="linux-64")
```

## Project integration

Install and verify the environment a `pyproject.toml` references in its
`[tool.nepenthe]` stanza (see [Consuming in a Project](Projects)):

```python
summary = nepenthe.sync("pyproject.toml")          # install the referenced env
report = nepenthe.check("pyproject.toml")          # compatibility report

if report["has_conflicts"]:
    for dep in report["dependencies"]:
        if dep["status"] == "conflict":
            print(dep["requirement"], "→ env pins", dep["found"])
```

Both default to `./pyproject.toml` when the path is omitted; `check` accepts a
`platform=` override.

## Recovering a manifest

Recover the manifest a lock was solved from — from a bare lock file's embedded
band, or from a registry release (see
[Recovering the manifest from a lock](Registry#recovering-the-manifest-from-a-lock)):

```python
# from a lock file (offline; uses the embedded comment band)
yaml = nepenthe.manifest(lock="app-cpu-py3.11.lock")

# from a registry (tries the lock's band, then the manifest sidecar)
yaml = nepenthe.manifest(env="app", registry="file:///srv/nepenthe", python="3.11")
```

## Trial solve & registry listing

Check whether an environment would still solve with extra requirements (see
[Running](Running)), and browse a registry:

```python
# will the env still solve with these added? (recovers the manifest + re-solves)
report = nepenthe.try_solve(
    "app", "file:///srv/nepenthe", with_=["polars>=1"], python="3.11", variant="cpu",
)
if not report["satisfiable"]:
    print("conflict:", report["conflict"])

# inject a project's [project.dependencies] (PyPI → conda) instead of `with_`
report = nepenthe.try_solve("app", "file:///srv/nepenthe", project="pyproject.toml")

# list a registry's releases (newest first)
for release in nepenthe.list_releases("file:///srv/nepenthe"):
    print(release["environment"], release["version"], release["platform"])
```

A solver conflict is reported as `satisfiable=False` with a `conflict` message
— it does not raise.

## Return shapes

| Function           | Returns                                                                                                 |
| ------------------ | ------------------------------------------------------------------------------------------------------- |
| `build`            | list of cell dicts (`cell`, `variant`, `python`, `platforms`, `lock_path`, `releases`)                  |
| `create`           | summary dict (`prefix`, `environment`, `platform`, `packages`)                                          |
| `pull`             | `int` — bytes written                                                                                   |
| `manifest`         | `str` — the recovered manifest YAML                                                                     |
| `publish` / `show` | release dict (`environment`, `platform`, `python`, `variant`, `version`, `lock`, `manifest`, `created`) |
| `diff`             | dict (`up_to_date`, `added`, `removed`, `changed`)                                                      |
| `status`           | dict (`prefix`, `exists`, `packages`)                                                                   |
| `activate`         | `str` — the activation script                                                                           |
| `pack`             | summary dict (`output`, `environment`, `platforms`, `packages`, `bytes`)                                |
| `unpack`           | summary dict (`prefix`, `environment`, `platform`, `packages`)                                          |
| `sync`             | summary dict (`prefix`, `environment`, `platform`, `packages`)                                          |
| `check`            | report dict (`dependencies`, `satisfied`, `conflicts`, `missing`, `skipped`, `has_conflicts`)           |
| `try_solve`        | report dict (`satisfiable`, `platform`, `packages`, `conflict`)                                         |
| `list_releases`    | list of release dicts (newest first)                                                                    |
| `remove`           | `None`                                                                                                  |

Packages are rendered as conda triplets (`name=version=build`); `changed` is a
list of `(installed, desired)` pairs.

## Custom storage backends (fsspec bridge)

Two low-level helpers read and write spec bytes through a Python
[fsspec](https://filesystem-spec.readthedocs.io) object, for a backend that only
exists in Python:

```python
import fsspec

fs = fsspec.filesystem("memory")
nepenthe.fsspec_publish(fs, "specs/app.lock", b"...")
data = nepenthe.fsspec_pull(fs, "specs/app.lock")
```

These move a whole spec file at a time and are not wired into the high-level
commands (`build`, `create`, `sync`), which address backends by URL. See
[Storage Backends](Backends) for the native `file://` / `s3://` / `https://`
backends.
