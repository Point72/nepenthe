# Channels & Artifactory

Channels are where **packages** come from. In a manifest you reference channels
by **name**; nepenthe resolves those names to **URLs** at solve time. Nothing is
hardcoded — the same manifest can solve against public conda servers or an
internal mirror by changing only how names resolve.

This is the mechanism that replaces hardcoded package servers: you point
channels at your own Artifactory (or any server) without editing dependencies.

## Two knobs

`ChannelSettings` has two override knobs:

| Knob | Effect |
|------|--------|
| **`channel-alias`** | Base URL that **bare names** resolve against. `conda-forge` → `<alias>/conda-forge`. |
| **`mirror`** (per channel) | Rewrite a channel **before** resolution, keeping its public identity. `conda-forge` → `conda-forge-mirror`, which then resolves against the alias. |

Resolution order: apply a mirror first, then prefix the alias if the result is
still a bare name. A value that is already a URL (`scheme://…`) is returned
unchanged.

## In a manifest

```yaml
project:
  name: artifactory-demo
  # every bare channel name resolves against this internal Artifactory base
  channel-alias: https://artifactory.mycompany.net/artifactory/api/conda
  channels: [conda-forge, my-custom-channel]
  platforms: [linux-64]

channels:
  # keep the public identity `conda-forge`, but fetch it from the internal
  # `conda-forge-mirror` (which itself resolves against the alias above).
  conda-forge:
    mirror: conda-forge-mirror
  # `my-custom-channel` needs no entry: a bare name resolves directly
  # against the alias.

dependencies:
  - numpy >=2

environments:
  app: []
```

With the manifest above, the two channels resolve to:

| Channel name | Resolves to |
|--------------|-------------|
| `conda-forge` | `https://artifactory.mycompany.net/artifactory/api/conda/conda-forge-mirror` |
| `my-custom-channel` | `https://artifactory.mycompany.net/artifactory/api/conda/my-custom-channel` |

## From the manifest in code

```rust
use nepenthe_core::manifest::Manifest;
use nepenthe_core::solve::ChannelSettings;

let manifest = Manifest::load("manifest.yaml")?;
let settings = ChannelSettings::from_manifest(&manifest);

for channel in &manifest.project.channels {
    println!("{channel} => {}", settings.resolve(channel));
}
// conda-forge       => https://artifactory.mycompany.net/artifactory/api/conda/conda-forge-mirror
// my-custom-channel => https://artifactory.mycompany.net/artifactory/api/conda/my-custom-channel
```

## Building settings programmatically

You don't need a manifest — the same overrides can be built directly:

```rust
use nepenthe_core::solve::ChannelSettings;

let settings = ChannelSettings::with_alias(
        "https://artifactory.mycompany.net/artifactory/api/conda",
    )
    .mirror("conda-forge", "conda-forge-mirror");

assert_eq!(
    settings.resolve("conda-forge"),
    "https://artifactory.mycompany.net/artifactory/api/conda/conda-forge-mirror",
);
assert_eq!(
    settings.resolve("my-custom-channel"),
    "https://artifactory.mycompany.net/artifactory/api/conda/my-custom-channel",
);
// an already-resolved URL is left untouched
assert_eq!(
    settings.resolve("https://conda.anaconda.org/conda-forge"),
    "https://conda.anaconda.org/conda-forge",
);
```

## Per-channel explicit URL

A channel can also pin an explicit `url` (used as-is, instead of resolving the
name against the alias):

```yaml
channels:
  my-custom-channel:
    url: https://artifactory.mycompany.net/artifactory/api/conda/my-custom-channel
```

## Authentication

A channel can require credentials (e.g. a private Artifactory repository).
Credentials are **never** part of the channel definition, a manifest, or a
lock — they live in an out-of-band auth store, keyed by host, and are applied as
an `Authorization` header at request time. Package URLs in the resulting lock
stay bare.

### Add the private channel to one environment

Put the private channel on just the environment that needs it (not the project
channels), so other environments don't require its credentials:

```yaml
environments:
  private-app:
    features: [app]
    channels: [my-private-channel]   # resolves via channel-alias, like any name
```

### Supply the credential out of band

Credentials are read by rattler's authentication storage, in order, from:

1. the JSON file named by the `RATTLER_AUTH_FILE` environment variable, then
2. `~/.rattler/credentials.json`.

Either is a JSON map of **host → credential**:

```json
{
  "artifactory.mycompany.net": {
    "BasicHTTP": { "username": "myuser", "password": "<token>" }
  }
}
```

`BearerToken` (`{ "BearerToken": "<token>" }`) and `CondaToken` are also
supported. The host key is the bare hostname (no scheme, no path). With the
credential in place, a private channel solves and installs exactly like a public
one — and the token never appears in the lock.

### CI: credentials from an environment variable

Some environments (e.g. GitHub Actions) can inject secrets as environment
variables but cannot write files. Set `NEPENTHE_CHANNEL_AUTH` to the **same
JSON** a credentials file would hold — a map of host → credential:

```yaml
# GitHub Actions
- name: Solve private environment
  env:
    NEPENTHE_CHANNEL_AUTH: |
      {"artifactory.mycompany.net":{"BasicHTTP":{"username":"svc-user","password":"${{ secrets.ARTIFACTORY_TOKEN }}"}}}
  run: nepenthe build --manifest env.yaml --env private --output-dir dist
```

`NEPENTHE_CHANNEL_AUTH` is layered on top of the file/keyring sources at highest
priority, so it wins for any host it defines. An unset secret expands to an empty
string, which is ignored (requests for public channels still go out
unauthenticated). As with the file, the token is applied as a request header and
never lands in the lock.

> This is **channel** (package) authentication. Credentials for the **registry**
> that stores published manifests and locks are a separate mechanism — see
> [Storage Backends](Backends#authentication).

## Try it

A runnable example and its fixtures ship with the crate:

```bash
cd rust
cargo run --example channel_override_artifactory     # rust/testdata/artifactory/manifest.yaml
cargo test --test channel_override_artifactory       # asserts the resolutions above
```
