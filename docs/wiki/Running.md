# Running in an Environment

`nepenthe run` executes a command in a **versioned, pre-solved** environment with
an optional **overlay** of extra dependencies — a structured `uv run`, where the
reproducibility unit is `(base lock + overlay + command)`. The base is frozen and
installed without conda; a **conda** overlay is re-solved on top with the base
pinned as constraints, and a **PyPI** overlay is layered with
[uv](https://docs.astral.sh/uv/) — so the combined environment is always
consistent.

## Declaring a run

Two config sources share one schema.

**Structured — `[tool.nepenthe.run]` in `pyproject.toml`** (for a tool/library):

```toml
[tool.nepenthe.run]
environment = "ccrt"
registry = "file:///srv/nepenthe"
version = "1.3.0"                  # label: latest | exact | range
python = "3.11"                    # + optional platform / variant
overlay = { conda = ["polars>=1"], pip = ["rich"] }  # conda + PyPI (uv) on top
editable = ["."]                   # prepend the working tree to PYTHONPATH
command = "python -m mytool"       # string, or an array ["python", "-m", "mytool"]
```

```bash
nepenthe run                       # reads ./pyproject.toml
```

**Inline — a PEP 723-style block in a script** (for one-off scripts):

```python
# /// nepenthe
# environment = "ccrt"
# registry = "file:///srv/nepenthe"
# python = "3.11"
# with = ["polars>=1"]             # shorthand for overlay.conda
# ///
import polars
print("hello")
```

```bash
nepenthe run --config script.py    # runs `python script.py` in the environment
```

## Overriding from the CLI

```bash
# add a conda overlay spec on top of the config
nepenthe run --with "polars>=1"

# add a PyPI overlay (installed with uv)
nepenthe run --with-pip "rich" --with-pip "httpx>=0.27"

# replace the command entirely (everything after `--`)
nepenthe run -- python -c "import polars; print(polars.__version__)"
```

The command's stdout/stderr pass through unchanged; nepenthe prints its own
one-line summary to stderr.

## How it works

1. **Materialize the base.** The published lock for the coordinates is pulled and
   installed into a content-keyed cache prefix (reused across runs — no re-install
   when the `(base, overlay)` pair is unchanged). No conda required.
2. **Lay the conda overlay.** The `overlay.conda` / `--with` specs are solved with
   the base packages pinned as `==` constraints (so the overlay can only *add*
   packages, never change a base version), against the base environment's
   channels (recovered from the lock's [embedded manifest](Registry#recovering-the-manifest-from-a-lock)).
   Only the new packages are installed on top.
3. **Lay the PyPI overlay.** The `overlay.pip` / `--with-pip` requirements are
   installed into the prefix with [uv](https://docs.astral.sh/uv/) (`uv pip
   install --python <prefix>`), resolving against and reusing what the base
   already provides. This runs once per content-keyed prefix (a
   `.nepenthe-pip-ready` marker makes it self-healing after a partial failure).
   uv is found on `PATH`, or via the `NEPENTHE_UV` environment variable.
4. **Exec.** The command runs with the prefix on `PATH` and `editable`
   directories prepended to `PYTHONPATH`. With `--image`, it instead runs inside a
   SIF image of the prefix (see below).

## Capturing the overlay as a lock

The run prefix is already content-keyed, but you can also emit a **standalone
overlay lock** — the conda packages solved on top of the base plus the
uv-compiled (`uv pip compile`) PyPI closure — so the delta is reproducible on its
own:

```bash
nepenthe run --with polars --with-pip rich \
  --emit-overlay-lock overlay.lock -- python -m mytool
```

```text
# nepenthe overlay lock
# conda (solved against the base)
polars=1.9.0=py311h0
# pypi (uv pip compile)
rich==13.9.4
```

## Materialization tiers

By default the overlay is installed into a **sibling prefix** (cheap, userspace).
Two stronger tiers are available:

- **Copy-on-write clone** (`--clone`) — materialize the base **once**, then clone
  it per overlay and install only the delta on top. On a reflink-capable
  filesystem (btrfs, XFS-reflink, APFS) the clone is near-instant and
  space-efficient; elsewhere it falls back to a plain copy.
- **Image** (`--image`) — run inside a SIF (see below).

```bash
nepenthe run --clone --with polars -- python -m mytool
```

### Running in an image (`--image`)

`nepenthe run --image` runs the command **inside an Apptainer/SIF image** of the
materialized environment instead of directly in the host prefix — stronger
isolation, same reproducibility unit. The image is packaged from the exact
content-keyed prefix (base + conda + PyPI overlays all baked in) and cached
alongside it (`<prefix>.sif`), so it is built once and reused.

```bash
# build (once) a SIF of the env + overlay, then exec inside it
nepenthe run --config script.py --image -- python -m mytool

# overlays are baked into the image
nepenthe run --image --with-pip rich -- python -c "import rich"

# a thin, fast image that binds the host prefix instead of baking it in
nepenthe run --image --lazy -- python -m mytool

# a writable layer over a read-only image (ephemeral, or persisted to a file)
nepenthe run --image --writable -- python -m mytool
nepenthe run --image --overlay-image scratch.img -- python -m mytool
```

`editable` directories are bind-mounted into the container at their host paths
and added to `PYTHONPATH`, so a working tree still overlays a baked image.
`--base` overrides the OS base image (default `debian:bookworm-slim`). `--lazy`
trades portability for a much smaller image (it needs the host prefix at run
time); `--writable` adds an in-memory writable layer, and `--overlay-image`
persists writes to an EXT3 overlay file. These need `apptainer` (see
[Building Images](Images)).

### Editable / working-tree overlay

`editable = ["."]` prepends your working tree to `PYTHONPATH`, so it runs against
a base that does **not** contain your library — no shadowing, no skew. This is the
clean way to develop a library against the very environment that ships it: the
base provides the dependencies, your tree provides the library.

## Limitations

- **PyPI overlays require uv.** `overlay.pip` / `--with-pip` shells out to the
  `uv` binary (its CLI is the stable surface); install uv or point `NEPENTHE_UV`
  at it. The delta is installed into the run prefix; pass `--emit-overlay-lock`
  to also capture it (conda pins + `uv pip compile`) as a standalone lock.
- **`--image` needs apptainer.** The image tier shells out to `apptainer` and
  bakes the whole prefix into a SIF; see [Building Images](Images) for details
  and the `NEPENTHE_APPTAINER` override.
- **Path activation, not full activation.** `activate.d` hook scripts are not
  run; the prefix's interpreter and tools are on `PATH` with `CONDA_PREFIX` set,
  which covers running commands. Use [`activate`](Install#activate) for a full
  activation script.
- **Editable is import-path injection**, not a full `pip install -e` (no entry
  points / metadata) — it suits running modules and scripts from a working tree.

## From Python

The Python API exposes the data-returning helpers; `run` itself is a CLI command
(it execs a process). See [Python API](Python-API) for `try_solve` and
`list_releases`.
