# Storage Backends

A **spec backend** stores manifests, override layers, and locks. `SpecStore`
selects the backend by URL scheme for local files, S3 buckets, and Artifactory
HTTP endpoints.

This is distinct from [channels](Channels): channels are where _packages_ come
from; backends are where _specs and locks_ live.

## Supported schemes

| Scheme                 | Backend         | Notes                                                                                           |
| ---------------------- | --------------- | ----------------------------------------------------------------------------------------------- |
| `file://`              | `LocalFs`       | Local filesystem; parent directories are created on write.                                      |
| `s3://`                | `S3Fs`          | Bucket is the host, key is the path. Works with any S3-compatible store via `AWS_ENDPOINT_URL`. |
| `https://` / `http://` | `ArtifactoryFs` | HTTP GET/PUT/DELETE. TLS uses the system trust store (honors an internal CA).                   |

```rust
use nepenthe_core::backend::SpecStore;

let store = SpecStore::new();

// publish a spec/lock
store.put("file:///srv/nepenthe/locks/app.lock", &bytes)?;

// pull it back
let got = store.get("file:///srv/nepenthe/locks/app.lock")?;
```

`nepenthe build` reads its `--manifest` and `--overrides` through this same
`SpecStore`, so each may be a local path **or** a backend URL. This lets a team
reuse a published override layer directly by version, without installing the
package that ships it:

```bash
nepenthe build --manifest myenvs.yaml \
  --overrides https://artifacts.example.com/generic/my-env/1.3.0/overrides.yaml \
  --env research --registry file:///tmp/reg --version 1.0.0
```

A manifest loaded from a URL must be self-contained: `imports` are resolved
relative to a local directory, so they are rejected for a remote manifest.

## Authentication

Credentials live in an `AuthStore`, keyed by host, never in a manifest, override,
or lock. They are applied when a backend is constructed and sent as a bearer
token or HTTP basic auth header, never embedded in a URL.

```rust
use nepenthe_core::backend::{AuthStore, Credential, SpecStore};

let mut auth = AuthStore::new();
auth.set("artifactory.mycompany.net", Credential::bearer("…token…"));
// or: Credential::basic("user", "password")

let store = SpecStore::with_auth(auth);
```

For S3, credentials and region/endpoint come from the `AuthStore` entry for the
bucket, or fall back to the ambient AWS chain (env vars, profile, instance
metadata). Set `AWS_ENDPOINT_URL` to target an S3-compatible store.

## Credential handling

nepenthe handles credentials as follows:

- **Logging**: `mask_url` replaces URL userinfo with `***` in logs and errors.
  If URL parsing fails, it redacts userinfo from the string directly.
- **Debug output**: credential types do not print secrets.
- **Transport**: a `http://` URL with configured credentials is rejected; use
  `https://`.
- **Artifacts**: published manifests and locks do not contain
  credentials.

## HTTP error mapping

The HTTP backend maps response statuses to errors, with the URL
masked: `404 → NotFound`, `401`/`403 → PermissionDenied`. Query strings are
preserved, so signed or versioned URLs work.

## Python fsspec backends

When a backend exists only as a Python
[`fsspec.AbstractFileSystem`](https://filesystem-spec.readthedocs.io/),
two low-level helpers read and write spec bytes
through it by adapting the object to the Rust `FileSystem` trait:

```python
import fsspec
import nepenthe

fs = fsspec.filesystem("memory")
nepenthe.fsspec_publish(fs, "/specs/app.lock", b"…lock bytes…")
data = nepenthe.fsspec_pull(fs, "/specs/app.lock")
```

These helpers move a whole spec file at a time (`fsspec_publish` writes all the
bytes, `fsspec_pull` reads them back). The high-level commands (`build`, `create`,
`sync`) address backends by URL (`file://`, `s3://`, `https://`) and do not
accept an fsspec object. Use the helpers to transfer a manifest or lock through a store that has
no native URL backend.

## Registries are built on backends

A [registry](Registry) is a `SpecStore` plus a root URL. Point it at a `file://`
directory, an `s3://` bucket, or an `https://` Artifactory repo and the versioned
index and lock objects live there.
