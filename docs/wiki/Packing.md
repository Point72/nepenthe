# Packing for Air-Gapped Hosts

For machines that cannot reach conda channels, such as isolated build hosts or
offline labs, `pack` creates a bundle of the lock and its package archives.
Transfer the bundle to the target host and install without a network or conda.

A bundle is a `.tar` archive containing the lock, a manifest, and every package
archive the lock pins:

```text
nepenthe-pack.yml     # format, environment, platforms, packages (file + sha256 + size)
environment.lock      # the lock that was packed
pkgs/<filename>       # every .conda / .tar.bz2 the lock references
```

Packages inside `.conda` archives are already compressed, so the outer tar is
left uncompressed.

## Pack (on a connected host)

`pack` reads a lock, downloads every package it pins, verifies each against the
lock's recorded sha256, and writes the bundle:

```bash
# pack every platform the lock covers for the "app" environment
nepenthe pack --lock app.lock --env app --output app.tar
# packed app (linux-64) — 39 packages, 65.6 MiB → app.tar

# …or restrict to specific platforms (repeat --platform)
nepenthe pack --lock app.lock --env app \
  --platform linux-64 --platform osx-arm64 --output app.tar
```

A lock with multiple platforms shares noarch packages between them, so they are
bundled once.

## Unpack (on the air-gapped host)

Transfer the `.tar` to the target host, then install from it. `unpack` extracts the bundle,
rewrites every package URL to the bundle's local copy, and installs into a
prefix, offline and without conda:

```bash
nepenthe unpack --pack app.tar --prefix ./envs/app
# installed app (linux-64) at ./envs/app — 39 packages

# the environment defaults to the bundle's; override env/platform if needed
nepenthe unpack --pack app.tar --prefix ./envs/app \
  --env app --platform linux-64 --stage-dir ./unpacked
```

By default the bundle is extracted to a temporary directory that is removed after
the install. Pass `--stage-dir` to keep the extracted packages, for instance to
install several prefixes from one bundle without re-extracting.

The installed packages match an online install from the same lock;
[`diff`](Install#inspect-status-and-diff) reports no differences.

## From Python

```python
import nepenthe

# producer host
summary = nepenthe.pack("app.lock", "app", "app.tar")          # all platforms
summary = nepenthe.pack("app.lock", "app", "app.tar", platforms=["linux-64"])
print(summary["packages"], summary["bytes"])

# air-gapped host
nepenthe.unpack("app.tar", "./envs/app")                        # offline install
nepenthe.unpack("app.tar", "./envs/app", env="app", platform="linux-64")
```

See the [Python API](Python-API) for function signatures and return values.

## How it works

- **Integrity end to end.** Every package is verified against the lock's sha256
  at pack time; rattler re-verifies the same hash when linking from the bundle at
  install time. A corrupt or tampered package is rejected.
- **No conda, no re-solve.** `unpack` rewrites each lock record's URL to a
  `file://` path inside the bundle; rattler's installer reads the local archive
  directly (`get_or_fetch_from_path`) and links it into the prefix.
- **Reproducible.** The bundle contains the exact lock, so the air-gapped install
  matches the online one package for package.

## Limitations

- **Authenticated channels.** Package downloads during `pack` are unauthenticated.
  Packing from a channel that requires credentials is not yet supported; pack
  from a mirror that permits unauthenticated downloads.
- **Size.** A bundle contains the full package set, so it is as large as the
  environment and can be tens to hundreds of MiB.
