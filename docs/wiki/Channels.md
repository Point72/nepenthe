# Channels & Artifactory

Channels provide packages. A manifest refers to channels by name, and nepenthe
resolves those names to URLs at solve time. Configure resolution to use public
conda servers, Artifactory, or another mirror without changing dependencies.

## Channel settings

`ChannelSettings` supports two overrides:

| Setting                    | Effect                                                                                                                                             |
| -------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------- |
| **`channel-alias`**        | Base URL that **bare names** resolve against. `conda-forge` → `<alias>/conda-forge`.                                                               |
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

| Channel name        | Resolves to                                                                  |
| ------------------- | ---------------------------------------------------------------------------- |
| `conda-forge`       | `https://artifactory.mycompany.net/artifactory/api/conda/conda-forge-mirror` |
| `my-custom-channel` | `https://artifactory.mycompany.net/artifactory/api/conda/my-custom-channel`  |

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

The same overrides can be configured without a manifest:

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

A channel can require credentials, for example a private Artifactory repository.
Credentials are never part of the channel definition, a manifest, or a lock.
They are stored separately in an auth store, keyed by host, and applied as an
`Authorization` header at request time. Package URLs in the resulting lock do
not contain credentials.

### Add the private channel to one environment

Put the private channel on just the environment that needs it, rather than the
project channels, so other environments don't require its credentials:

```yaml
environments:
  private-app:
    features: [app]
    channels: [my-private-channel]   # resolves via channel-alias, like any name
```

### Supply credentials separately

Credentials are read by rattler's authentication storage, in order, from:

1. the JSON file named by the `RATTLER_AUTH_FILE` environment variable, then
1. `~/.rattler/credentials.json`.

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
one, and the token never appears in the lock.

### CI: credentials from an environment variable

Some environments, GitHub Actions among them, can inject secrets as environment
variables but cannot write files. Set `NEPENTHE_CHANNEL_AUTH` to the same JSON a
credentials file would contain, a map of host to credential:

```yaml
# GitHub Actions
- name: Solve private environment
  env:
    NEPENTHE_CHANNEL_AUTH: |
      {"artifactory.mycompany.net":{"BasicHTTP":{"username":"svc-user","password":"${{ secrets.ARTIFACTORY_TOKEN }}"}}}
  run: nepenthe build --manifest env.yaml --env private --output-dir dist
```

`NEPENTHE_CHANNEL_AUTH` takes precedence over file/keyring sources for each host
it defines. An unset secret expands to an empty string, which is ignored;
requests for public channels remain unauthenticated. The token is applied as a
request header and is not written to the lock.

> This is **channel** (package) authentication. Credentials for the **registry**
> that stores published manifests and locks use a separate mechanism. See
> [Storage Backends](Backends#authentication).

## Example and tests

The crate includes a runnable example and fixtures:

```bash
cd rust
cargo run --example channel_override_artifactory     # rust/testdata/artifactory/manifest.yaml
cargo test --test channel_override_artifactory       # asserts the resolutions above
```
