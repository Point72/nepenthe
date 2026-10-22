# Consuming an Environment in a Project

A repository that uses a shared nepenthe environment declares it in its
`pyproject.toml`. The reference identifies the environment and version; it does
not redefine the environment. Use `sync` to install it and `check` to compare
your project's dependencies with its package versions.

## Why a reference, not a re-solve

A nepenthe environment is a pre-solved, versioned, shared artifact: the producer
solves it once and publishes a frozen lock, and every consumer installs that
exact lock. `sync` therefore installs the published environment as-is. It does
not re-resolve the environment together with your project's dependencies, since
that would give different consumers different environments.

`check` reports whether `[project.dependencies]` is compatible with the
referenced environment.

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

Two optional sub-tables, `[tool.nepenthe.package-mappings]` and
`[tool.nepenthe.extras-mappings]`, adjust how PyPI requirement names are matched
against conda packages. See [Overriding the name map](#overriding-the-name-map).

## `sync` — install the referenced environment

```bash
nepenthe sync                          # reads ./pyproject.toml
nepenthe sync --project path/to/pyproject.toml
# synced myenv (linux-64) at .venv — 312 packages
```

`sync` resolves the version label against the registry, pulls the lock, and
installs it into the prefix, with no conda required. Running `sync` again after
bumping `version` updates the prefix. Pin the environment in `pyproject.toml`,
then run `sync` in CI or on a new machine to install the same base.

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

- **ok**: the environment pins a version satisfying your requirement.
- **conflict**: the environment pins the package, but at a version your
  requirement excludes. `check` exits non-zero, so it fails CI.
- **missing**: the environment has no package with that name. This is reported
  but does not fail the command, allowing dependencies you install separately,
  such as pure-Python packages.
- **skip**: the entry isn't a simple name+version requirement, such as a direct
  URL, so it can't be checked.

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

See the [Python API](Python-API) for function signatures and return values.

## Name matching (conda vs PyPI)

`check` matches names after [PEP 503](https://peps.python.org/pep-0503/)
normalization (lowercase; `-`, `_`, `.` collapse to `-`) against the
environment's conda package names. When a PyPI name differs from its conda
counterpart, as with `opencv-python` (PyPI) and `opencv` (conda), `check`
consults a bundled PyPI-to-conda name map (derived from conda-forge's
[grayskull](https://github.com/conda-forge/conda-forge-bot-data) mapping) so the
dependency still resolves. Only names absent under both spellings report as
missing.

The bundled map contains only the few hundred names that differ; the roughly
12,000 names that agree are matched directly. To regenerate it from the upstream
source, see
[Updating the name map](#updating-the-name-map) below. For an obscure or very new
package, a `missing` result means the map could not confirm a match; it does not
prove that no conda package exists.

### Overriding the name map

The bundled map comes from conda-forge. For distributions packaged elsewhere or
by your project, define mappings locally:

```toml
[tool.nepenthe.package-mappings]
example-package = "example-conda-package"

[tool.nepenthe.extras-mappings]
"example-package[extra]" = ["example-package", "example-package-extra"]
```

`package-mappings` renames a distribution, taking precedence over the vendored
entry (or supplying one where there is none).

`extras-mappings` handles a case a rename cannot express. conda has no equivalent
of a PyPI extras group, so a distribution that splits its optional features into
separate conda packages maps one requirement onto several packages. `check` then
requires every one of them: the requirement is **ok** only when all are present
and satisfy its version specifier, and it reports the first that is **missing**
or in **conflict**. `nepenthe try` expands the same way, adding one conda spec
per package.

Keys are normalized, so casing, separator style and the order of extras in the
key do not matter. An extras group with no mapping resolves to the distribution
name alone, as if the requirement had no extras.

Both tables also apply to `nepenthe try --project`, which reads them without
requiring the rest of the `[tool.nepenthe]` stanza.

### Updating the name map

The vendored table lives at `rust/src/data/pypi_to_conda.tsv` and is reproducible
from conda-forge's grayskull mapping:

```bash
cd rust
cargo run --example regenerate_name_map   # fetches, reduces, rewrites the table
```

The reducer keeps only pairs whose names differ and sorts the output. Commit
the table if it changed.

## Limitations

- **No joint resolution** with `uv` / `pixi` locks. nepenthe installs the shared
  base; you manage your project's own dependencies on top, for example with
  `uv`/`pip` into the same prefix. `check` reports version conflicts; nepenthe
  does not merge or re-solve locks.
- **No `pixi.toml` / `uv.lock` import.** The reference lives in
  `[tool.nepenthe]`; other lockfiles are independent.
