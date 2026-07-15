# Building Images

`nepenthe image build` packages a **published environment** into a self-contained,
reproducible image: the environment is materialized (every package on disk, no
conda) and baked into an **Apptainer/SIF** image (the default, ideal for
research/HPC) or an **OCI/Docker** image (for Kubernetes and registries).

## Building a SIF

```bash
nepenthe image build app \
  --registry file:///srv/nepenthe \
  --python 3.11 \
  --output app.sif
```

This resolves `app` (here the `py3.11` cell) at `latest`, materializes it, and
writes `app.sif`. Select a release the same way as [`create`](Install#create):
`--platform`, `--python`, `--variant`, and `--label` (default `latest`).

Run it:

```bash
apptainer run app.sif python -c "import numpy; print(numpy.__version__)"
# or exec a binary directly
apptainer exec app.sif python --version
```

### Lazy (cache-mounted) images

Pass `--lazy` to build a **thin** SIF that does *not* bake the packages in — the
environment prefix is bound at run time instead. The image is a fraction of the
size and builds almost instantly, at the cost of portability (it needs the host
prefix present):

```bash
nepenthe image build app --registry file:///srv/nepenthe --python 3.11 \
  --output app.lazy.sif --lazy
```

Use a self-contained image to ship anywhere; use `--lazy` for fast, local,
disk-cheap runs that share one materialized environment.

## What "self-contained" means

The image bundles **all of the environment's packages**, so nothing is fetched at
run time. It is layered on a small **base OS image** (default
`debian:bookworm-slim`) that supplies the things a conda environment relies on but
does not itself contain:

- the **glibc dynamic loader** (`/lib64/ld-linux-*`) that conda-forge binaries are
  linked against, and
- a **`/bin/sh`** to interpret the image's runscript.

An empty (`scratch`) image has neither, so it cannot run conda binaries — hence
the minimal base. Choose a different one (for example an internal-mirror tag, or
to match a target cluster) with `--base`:

```bash
nepenthe image build app --registry file:///srv/nepenthe --python 3.11 \
  --output app.sif --base registry.example.com/debian:bookworm-slim
```

> The base must be a **glibc** distribution. `alpine` (musl) will not run
> conda-forge binaries.

## OCI / Docker images

For Kubernetes and registry workflows, build an **OCI image** instead with
`--format oci --tag`:

```bash
nepenthe image build app \
  --registry file:///srv/nepenthe --python 3.11 \
  --format oci --tag nepenthe-app:1.0.0
```

This materializes the environment, generates a `Containerfile` (`FROM <base>` +
`COPY . <prefix>`), and builds the image into your local engine's store. Run it
with any command — the environment is on `PATH`:

```bash
podman run --rm nepenthe-app:1.0.0 python -c "import numpy; print(numpy.__version__)"
docker run --rm nepenthe-app:1.0.0 python --version
```

The engine is **podman** if present, else **docker**; override with
`NEPENTHE_OCI_ENGINE`. The default command is `python`; pass another after the
image tag to run it (standard OCI semantics — there is no fixed entrypoint).

## Running against an image

[`nepenthe run --image`](Running#running-in-an-image---image) executes a command
inside a SIF of the run environment (base + overlays), rather than building a
standalone artifact — the convenient, content-keyed path for one-off isolated
runs. It supports the same `--lazy` thin-image mode, plus writable layers over a
read-only base:

- `--writable` — an ephemeral in-memory writable layer (`--writable-tmpfs`).
- `--overlay-image <file>` — a persistent EXT3 overlay; writes survive across
  runs while the base SIF stays read-only (created on first use via
  `apptainer overlay create`).

Use `image build` when you want a named, shareable image instead.

## How it works

1. **Materialize.** The published lock is pulled and installed into a staging
   prefix (a cache dir by default; override with `--prefix`) — the same
   no-conda installer used by [`create`](Install#create).
1. **Generate a definition.** nepenthe writes an Apptainer definition that
   bootstraps from `--base` and copies the environment into the image at the
   **same absolute path** it occupies on the host. Keeping the path identical
   means conda's baked prefixes, shebangs, and `RPATH`s resolve unchanged — no
   prefix relocation.
1. **Build.** nepenthe shells out to `apptainer build` to produce the SIF. The
   `apptainer` CLI is the stable integration surface (just as PyPI overlays
   delegate to `uv`); point `NEPENTHE_APPTAINER` at a specific binary, otherwise
   `apptainer` on `PATH` is used.

The image carries provenance labels (`org.nepenthe.environment`,
`org.nepenthe.platform`, `org.nepenthe.label`), inspectable with
`apptainer inspect app.sif`.

## Requirements

- **apptainer** for SIF (`--format sif`), or **podman**/**docker** for OCI
  (`--format oci`) — invoked as a subprocess. Install apptainer from
  [apptainer.org](https://apptainer.org/) (or set `NEPENTHE_APPTAINER`); set
  `NEPENTHE_OCI_ENGINE` to pick the OCI engine.
- **Network (once) for the base image**, unless the base is already cached or
  pulled from an internal mirror via `--base`.

## Limitations

- **SIF and OCI today.** SIF supports self-contained, `--lazy` (cache-mounted),
  and writable-overlay modes; OCI export is self-contained only.
- **In-image env path** is the host staging path (it contains your username by
  default). Pin it with `--prefix` for a stable, shareable path.
- **Reproducible modulo the base + timestamps.** Package contents match the lock;
  the base image tag and build timestamps are the remaining variables — pin a
  digest in `--base` for byte-stability.
