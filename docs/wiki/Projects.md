# Consuming an Environment in a Project

A repository that **uses** a shared nepenthe environment declares it in its
`pyproject.toml`. This is a small **reference** — which environment, which
version — not the environment definition. From it, nepenthe can install the
environment (`sync`) and check your project's dependencies against it (`check`).

## Why a reference, not a re-solve

A nepenthe environment is a **pre-solved, versioned, shared artifact**: the
producer solves it once and publishes a frozen lock, and every consumer installs
that exact lock. `sync` therefore installs the published environment as-is — it
does **not** re-resolve the environment together with your project's
dependencies. That is the whole point: every consumer gets the identical
environment.

`check` is the seam that keeps the two in sync. It tells you whether what your
project declares in `[project.dependencies]` is compatible with the environment
you reference, so you catch drift before it bites.

## Register the environment

Add a `[tool.nepenthe]` section to your `pyproject.toml`:

```toml
[project]
name = "my-app"
dependencies = [
    "numpy>=2",
    "requests",
]

[tool.nepenthe]
environment = "myenv"                 # required: which published environment
registry = "file:///srv/nepenthe"    # required: where it is published
version = "1.3.0"                     # optional: label (default "latest")
platform = "linux-64"                # optional: defaults to the current platform
python = "3.11"                      # optional: the python axis value
variant = "cpu"                      # optional: the variant axis value
prefix = ".venv"                     # optional: install location (default ".venv")
```

`version` accepts a label: `latest`, an exact version (`1.3.0`), or a range
(`>=1.2,<2`).

## `sync` — install the referenced environment

```bash
nepenthe sync                          # reads ./pyproject.toml
nepenthe sync --project path/to/pyproject.toml
# synced myenv (linux-64) at .venv — 312 packages
```

`sync` resolves the version label against the registry, pulls the lock, and
installs it into the prefix — no conda required. Re-running `sync` after bumping
`version` updates the prefix to the new release. This is how you "register" the
environment: pin it once in `pyproject.toml`, then `sync` in CI or on a new
machine to get the exact same base every time.

## `check` — verify your dependencies are compatible

```bash
nepenthe check                         # reads ./pyproject.toml
nepenthe check --platform osx-arm64
#   [ok]       numpy>=2 (env pins 2.4.6)
#   [conflict] pandas>=3 — env pins 2.1.0 (needs >=3)
#   [missing]  internal-tool — not in env
#   [skip]     torch @ https://… — not a name+version requirement
# 1 ok, 1 conflict, 1 missing, 1 skipped
```

`check` pulls the environment's lock and compares each `[project.dependencies]`
entry against the pinned package set:

- **ok** — the environment pins a version satisfying your requirement.
- **conflict** — the environment pins the package, but at a version your
  requirement excludes. **`check` exits non-zero**, so it fails CI.
- **missing** — the environment has no package with that name. This is often
  fine (a pure-Python dependency you install yourself on top), so it is reported
  but does **not** fail the command.
- **skip** — the entry isn't a simple name+version requirement (e.g. a direct
  URL), so it can't be checked.

A typical CI step:

```bash
nepenthe sync && nepenthe check
```

## From Python

```python
import nepenthe

summary = nepenthe.sync("pyproject.toml")          # install the referenced env
report = nepenthe.check("pyproject.toml")          # structured compatibility report
if report["has_conflicts"]:
    for dep in report["dependencies"]:
        if dep["status"] == "conflict":
            print(dep["requirement"], "→ env pins", dep["found"])
```

See the [Python API](Python-API) for the full surface.

## Name matching (conda vs PyPI)

`check` matches names after [PEP 503](https://peps.python.org/pep-0503/)
normalization (lowercase; `-`, `_`, `.` collapse to `-`) against the
environment's **conda** package names. When a PyPI name differs from its conda
counterpart — e.g. `opencv-python` (PyPI) vs `opencv` (conda) — `check` consults
a bundled PyPI→conda name map (derived from conda-forge's
[grayskull](https://github.com/conda-forge/conda-forge-bot-data) mapping) so the
dependency still resolves. Only names absent under **both** spellings report as
**missing**.

The map is small by design: only the few hundred *divergent* names are vendored
(the ~12k names that already agree are handled by the direct match). It is
regenerated from the upstream source with a single command — see
[Updating the name map](#updating-the-name-map) below. Even so, treat an
occasional `missing` as "couldn't confirm" rather than "definitely absent" for an
obscure or very new package.

### Updating the name map

The vendored table lives at `rust/src/data/pypi_to_conda.tsv` and is reproducible
from conda-forge's grayskull mapping:

```bash
cd rust
cargo run --example regenerate_name_map   # fetches, reduces, rewrites the table
```

The reducer keeps only divergent pairs and sorts the output, so a regenerated
table diffs cleanly — commit it if it changed.

## What's not here yet

- **No joint resolution** with `uv` / `pixi` locks. nepenthe installs the shared
  base; you manage your project's own dependencies on top (e.g. `uv`/`pip` into
  the same prefix). `check` keeps the two honest, but nepenthe does not merge or
  re-solve locks.
- **No `pixi.toml` / `uv.lock` import.** The reference lives in
  `[tool.nepenthe]`; other lockfiles are independent.
