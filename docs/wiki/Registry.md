# Registry & Versioning

The **registry** is a backend-hosted index that maps a versioned environment to
a content-addressed lock. Each environment has its own versions and labels,
without encoding them in filenames such as `env-{name}-py{py}-{version}.txt`.

A registry lives under a single root URL on any [storage backend](Backends)
(`file://`, `s3://`, `https://`) and holds two things: an `index.yaml` and a
`locks/` directory of immutable, content-addressed lock objects.

## Coordinates and labels

A **coordinate** identifies one version sequence:

```
Coordinates { environment, platform, python?, variant? }
```

Each `(environment, platform, python, variant)` has its own semver sequence, so
`myenv` can be on `2.0.0` while `myenv2` is on `0.5.0`.

A **label** selects a version within a coordinate:

| Label                       | Resolves to                               |
| --------------------------- | ----------------------------------------- |
| `latest`                    | the highest published version             |
| `latest-but-one`            | the second-highest (the previous release) |
| an exact version (`1.2.0`)  | that exact version                        |
| a semver range (`>=1.2,<2`) | the highest version satisfying it         |

`Label::parse` classifies a string: `latest`, `latest-but-one`, anything
starting with a comparator (`>`, `<`, `=`, `^`, `~`, `*`) is a range, otherwise
it's an exact version.

## Content addressing & immutability

A lock is stored by its content address, `sha256-<hex>` of its bytes, under
`<root>/locks/<address>.lock`, written once. The index records, per release,
which address a version points to:

- **Immutable**: republishing a version with identical content is a no-op;
  republishing with different content is rejected.
- **Deduplicated**: versions with identical lock bytes share one object.
- **Integrity-checked**: `pull` validates the address format
  (`sha256-<64 lowercase hex>`) and recomputes the hash of the fetched bytes,
  rejecting tampered or corrupt locks before they reach an installer.
- **Rollback**: update the label; the lock remains unchanged.

## Publishing and resolving

```rust
use nepenthe_core::backend::SpecStore;
use nepenthe_core::registry::{Coordinates, Label, Registry};

// a registry rooted on any backend
let registry = Registry::new(SpecStore::new(), "file:///srv/nepenthe");

let coords = Coordinates::new("app", "linux-64")
    .with_python("3.11")
    .with_variant("cpu");

// publish two versions (lock_bytes comes from the export step)
registry.publish(&coords, "1.0.0", lock_v1)?;
registry.publish(&coords, "1.1.0", lock_v2)?;

// resolve by label
let latest = registry.resolve(&coords, &Label::Latest)?;          // 1.1.0
let prev   = registry.resolve(&coords, &Label::LatestButOne)?;    // 1.0.0
let ranged = registry.resolve(&coords, &Label::parse(">=1.0,<1.1"))?;

// pull the lock bytes (integrity-checked)
let bytes = registry.pull(&coords, &Label::Latest)?;
```

## The index

`index.yaml` at the registry root records published releases:

```yaml
releases:
  - environment: app
    platform: linux-64
    python: "3.11"
    variant: cpu
    version: "1.1.0"
    lock: sha256-…          # content address of the lock object
    created: "2026-06-17T12:00:00Z"
```

It is read on `resolve`/`pull` and appended on `publish`. The index assumes a
single writer; concurrent publishing with compare-and-swap is not implemented.

## End to end

Resolve a manifest, solve its dependencies, export the lock, and publish it:

```rust
// resolve → solve → export → publish
let resolved = manifest.resolve_default("app")?;
let request  = SolveRequest::from_resolved(&resolved, manifest.project.channels.clone(), Default::default());
let outcome  = solve(&request, &ChannelSettings::from_manifest(&manifest)).await?;
let lock     = to_lockfile_string(&outcome, "app")?.into_bytes();

registry.publish(&coords, "1.0.0", &lock)?;
```

See `rust/examples/registry_local.rs` (offline, local registry) and
`rust/examples/e2e_from_yaml.rs` (YAML fixtures → registry round-trip).
Installing **from** a pulled lock is covered in
[Installing Environments](Install).

## Recovering the manifest from a lock

A package lock alone does not include the manifest needed to re-solve its
dependencies. nepenthe stores the composed manifest with each build in two
forms:

- **Embedded in the lock file.** `nepenthe build --output-dir` prepends the
  manifest to each lock as a compressed comment block (`# nepenthe:manifest+…`).
  It is valid YAML that pixi/rattler ignore, so the file stays a usable
  `pixi.lock`. The manifest can be recovered from the file without a registry.
- **A registry sidecar.** `nepenthe build --registry …` stores the manifest as a
  content-addressed object next to the lock and points the release at it. It is
  stored once and shared by every cell/version that solved the same manifest.

Recover it with `nepenthe manifest`, from either source:

```bash
# from a bare lock file (uses the embedded band, offline)
nepenthe manifest --lock app-cpu-py3.11.lock > environment.yaml

# from a registry (tries the lock's band, then the sidecar)
nepenthe manifest --env app --registry file:///srv/nepenthe \
  --python 3.11 --variant cpu -o environment.yaml
```

A tool that rewrites the lock may drop the embedded comment because it is not
part of the lock structure. Reading an immutable, nepenthe-published lock does
not change it, and the registry sidecar is unaffected.
