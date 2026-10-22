# Building Images

`nepenthe image build` installs a published environment and packages it into an
Apptainer/SIF image by default, or an OCI/Docker image for Kubernetes and
container registries. Installation does not require conda.

## Building a SIF

```bash
nepenthe image build app \
  --registry file:///srv/nepenthe \
  --python 3.11 \
  --output app.sif
```

This resolves `app` (here the `py3.11` cell) at `latest`, installs it, and
writes `app.sif`. Select a release the same way as [`create`](Install#create):
`--platform`, `--python`, `--variant`, and `--label` (default `latest`).

Run it:

```bash
apptainer run app.sif python -c "import numpy; print(numpy.__version__)"
# or exec a binary directly
apptainer exec app.sif python --version
```

### Lazy (cache-mounted) images

Pass `--lazy` to build a SIF without copying packages into it. The environment
prefix is mounted at run time instead, reducing image size and build time but
requiring the host prefix to remain available:

```bash
nepenthe image build app --registry file:///srv/nepenthe --python 3.11 \
  --output app.lazy.sif --lazy
```

Use a self-contained image to transfer the environment between hosts. Use
`--lazy` for local runs that share an installed environment.

## What "self-contained" means

The image bundles all of the environment's packages, so nothing is fetched at run
time. It is layered on a small base OS image (default `debian:bookworm-slim`)
that supplies two dependencies not included in the conda environment:

- the **glibc dynamic loader** (`/lib64/ld-linux-*`) that conda-forge binaries are
  linked against, and
- a **`/bin/sh`** to interpret the image's runscript.

An empty (`scratch`) image has neither, so it cannot run these conda binaries.
Use `--base` to select another base, such as an internal mirror or an image
matching the target cluster:

```bash
nepenthe image build app --registry file:///srv/nepenthe --python 3.11 \
  --output app.sif --base registry.example.com/debian:bookworm-slim
```

> The base must be a **glibc** distribution. `alpine` (musl) will not run
> conda-forge binaries.

## OCI / Docker images

For Kubernetes and registry workflows, build an OCI image instead with
`--format oci --tag`:

```bash
nepenthe image build app \
  --registry file:///srv/nepenthe --python 3.11 \
  --format oci --tag nepenthe-app:1.0.0
```

This installs the environment, generates a `Containerfile` (`FROM <base>` +
`COPY . <prefix>`), and builds the image into your local engine's store. Run it
with any command; the environment is on `PATH`:

```bash
podman run --rm nepenthe-app:1.0.0 python -c "import numpy; print(numpy.__version__)"
docker run --rm nepenthe-app:1.0.0 python --version
```

The engine is podman if present, else docker; override with
`NEPENTHE_OCI_ENGINE`. The default command is `python`; pass another after the
image tag to run it (standard OCI semantics, with no fixed entrypoint).

## Running against an image

[`nepenthe run --image`](Running#running-in-an-image---image) executes a command
inside a SIF of the run environment (base plus overlays), rather than building a
standalone artifact. It caches the image by content for reuse across runs and
supports `--lazy` images and writable layers over a read-only base:

- `--writable`: an ephemeral in-memory writable layer (`--writable-tmpfs`).
- `--overlay-image <file>`: a persistent EXT3 overlay; writes persist across
  runs while the base SIF stays read-only (created on first use via
  `apptainer overlay create`).

Use `image build` when you want a named, shareable image instead.

## How it works

1. **Install.** The published lock is pulled and installed into a staging
   prefix (a cache directory by default; override with `--prefix`), using the same
   no-conda installer as [`create`](Install#create).
1. **Generate a definition.** nepenthe writes an Apptainer definition that
   bootstraps from `--base` and copies the environment into the image at the
   same absolute path it occupies on the host. Keeping the path identical means
   conda's embedded prefixes, shebangs, and `RPATH`s resolve unchanged, with no
   prefix relocation.
1. **Build.** nepenthe calls `apptainer build` to produce the SIF. Set
   `NEPENTHE_APPTAINER` to use a specific binary; otherwise it uses `apptainer`
   on `PATH`.

The image includes provenance labels (`org.nepenthe.environment`,
`org.nepenthe.platform`, `org.nepenthe.label`), inspectable with
`apptainer inspect app.sif`.

## Requirements

- **apptainer** for SIF (`--format sif`), or **podman**/**docker** for OCI
  (`--format oci`), invoked as a subprocess. Install apptainer from
  [apptainer.org](https://apptainer.org/) (or set `NEPENTHE_APPTAINER`); set
  `NEPENTHE_OCI_ENGINE` to pick the OCI engine.
- **Network (once) for the base image**, unless the base is already cached or
  pulled from an internal mirror via `--base`.

## Limitations

- **Image formats.** SIF supports self-contained, `--lazy` (cache-mounted),
  and writable-overlay modes; OCI export is self-contained only.
- **In-image env path** is the host staging path, which contains your username by
  default. Pin it with `--prefix` for a stable, shareable path.
- **Base image and timestamps.** Package contents match the
  lock; the base image tag and build timestamps are the remaining variables. Pin
  a digest in `--base` to fix the base image version.
