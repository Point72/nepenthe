# Running in an Environment

`nepenthe run` executes a command in a versioned, pre-solved environment with an
optional overlay of extra dependencies. The base is installed from a frozen
lock without conda. Conda overlay dependencies are solved with the base packages
pinned as constraints; PyPI overlay dependencies are installed with
[uv](https://docs.astral.sh/uv/).

## Declaring a run

Run configuration uses the same schema in two locations.

**`[tool.nepenthe.run]` in `pyproject.toml`**, for a tool or library:

```toml
[tool.nepenthe.run]
environment = "myenv"
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

**A PEP 723-style block in a script**, for one-off scripts:

```python
# /// nepenthe
# environment = "myenv"
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
nepenthe run --with-pip "rich" --with-pip "httpx2>=0.27"

# replace the command entirely (everything after `--`)
nepenthe run -- python -c "import polars; print(polars.__version__)"
```

The command's stdout/stderr pass through unchanged; nepenthe prints its own
one-line summary to stderr.

## How it works

1. **Install the base.** The published lock for the coordinates is pulled and
   installed into a content-keyed cache prefix. Runs reuse the prefix when the
   `(base, overlay)` pair is unchanged.
1. **Install the conda overlay.** The `overlay.conda` / `--with` specs are solved with
   the base packages pinned as `==` constraints, so the overlay can only add
   packages and never change a base version, against the base environment's
   channels (recovered from the lock's [embedded manifest](Registry#recovering-the-manifest-from-a-lock)).
   Only the new packages are installed on top.
1. **Install the PyPI overlay.** The `overlay.pip` / `--with-pip` requirements are
   installed into the prefix with [uv](https://docs.astral.sh/uv/) (`uv pip install --python <prefix>`), resolving against and reusing what the base
   already provides. This runs once per content-keyed prefix. A
   `.nepenthe-pip-ready` marker allows recovery after a partial failure.
   uv is found on `PATH`, or via the `NEPENTHE_UV` environment variable.
1. **Run the command.** The command runs with the prefix on `PATH` and `editable`
   directories prepended to `PYTHONPATH`. With `--image`, it instead runs inside a
   SIF image of the prefix (see below).

## Capturing the overlay as a lock

Use `--emit-overlay-lock` to save the overlay's resolved dependencies separately.
The file records conda packages solved against the base and the PyPI dependency
set produced by `uv pip compile`:

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

## Installation and isolation options

By default the overlay is installed into a sibling prefix in userspace. Two
other options are available:

- **Copy-on-write clone** (`--clone`): install the base once, clone it per overlay,
  and install the additional packages. Reflink-capable filesystems (btrfs,
  XFS-reflink, APFS) share unchanged data between clones; other filesystems use
  a plain copy.
- **Image** (`--image`): run inside a SIF (see below).

```bash
nepenthe run --clone --with polars -- python -m mytool
```

### Running in an image (`--image`)

`nepenthe run --image` runs the command inside an Apptainer/SIF image of the
installed environment instead of directly in the host prefix. The image includes
the base and both overlays from the content-keyed prefix. It is cached alongside
the prefix as `<prefix>.sif` and reused across runs.

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
and added to `PYTHONPATH`, so commands can import from the working tree.
`--base` overrides the OS base image (default `debian:bookworm-slim`). `--lazy`
creates a smaller image that requires the host prefix at run time.
`--writable` adds an in-memory writable layer, and `--overlay-image`
persists writes to an EXT3 overlay file. These need `apptainer` (see
[Building Images](Images)).

### Editable / working-tree overlay

`editable = ["."]` prepends your working tree to `PYTHONPATH`. To develop a library
against its shared dependencies, use a base without the library installed so
Python imports it from your working tree.

## Limitations

- **PyPI overlays require uv.** `overlay.pip` / `--with-pip` shells out to the
  `uv` binary; install uv or point `NEPENTHE_UV`
  at it. The additional packages are installed into the run prefix; pass `--emit-overlay-lock`
  to also capture it (conda pins + `uv pip compile`) as a standalone lock.
- **`--image` needs apptainer.** The command calls `apptainer` to package
  the whole prefix into a SIF; see [Building Images](Images) for details
  and the `NEPENTHE_APPTAINER` override.
- **Path activation, not full activation.** `activate.d` hook scripts are not
  run. The prefix's interpreter and tools are on `PATH` with `CONDA_PREFIX` set,
  which covers running commands. Use [`activate`](Install#activate) for a full
  activation script.
- **Editable is import-path injection**, not a full `pip install -e`, so there
  are no entry points or metadata. It suits running modules and scripts from a
  working tree.

## From Python

The Python API exposes the data-returning helpers; `run` itself is a CLI command
(it execs a process). See [Python API](Python-API) for `try_solve` and
`list_releases`.
