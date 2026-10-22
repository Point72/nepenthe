# Comparing & Composing Environments

Compare package changes between published releases or combine multiple
environments into one lock.

## Diffing two releases

`nepenthe diff-versions` shows which packages were added, removed, or changed
version between two published releases. Review these changes before upgrading:

```bash
nepenthe diff-versions app \
  --registry file:///srv/nepenthe --python 3.11 \
  --from 1.2.0 --to 1.3.0
```

```text
app: 1.2.0 → 1.3.0 (linux-64)
+ polars 1.9.0
- six 1.16.0
~ numpy 2.1.0 -> 2.4.6
```

`+` marks an added package, `-` a removed package, and `~` a version change. Select
the cell with `--platform`, `--python`, and `--variant` as for
[`create`](Install#create).

> This compares published locks, a registry-level diff. To compare an installed
> prefix against a lock instead, use [`diff`](Install#diff).

## Composing published environments

`nepenthe compose` combines packages from two or more published environments
into one lock. For example, combine a shared `base` with a team's `ml-extras`:

```bash
nepenthe compose \
  --registry file:///srv/nepenthe --python 3.11 \
  --env base@1.3.0 --env ml-extras@2.1.0 \
  --name research --output research.lock
```

Publish and install the resulting lock:

```bash
nepenthe publish research --lock research.lock \
  --registry file:///srv/nepenthe --python 3.11 --version 1.0.0
nepenthe create research --registry file:///srv/nepenthe --python 3.11 --prefix ./env
```

### How it works

- Packages are combined by name across the inputs, for every platform common to
  all of them. The first input to provide a package takes precedence.

- Channels are unioned in input order.

- If two inputs pin the same package to different versions or builds, compose
  reports a conflict and fails:

  ```text
  error: cannot compose: 'numpy' is pinned to 2.4.6=py311h0 and 2.2.6=py311h1 on linux-64
  ```

  Resolve it by aligning the inputs' versions (or composing compatible releases).

> Compose combines frozen locks without re-solving and cannot reconcile
> incompatible pins. To resolve conflicts, rebuild from the
> requirements with [`build`](Install). Use [`try`](Install) to check whether a
> set of requirements can be solved together before publishing.

### Inputs

Each `--env` is `name` or `name@version` (default `latest`). All inputs must
share the selected `--platform` / `--python` / `--variant` cell. Give the output
a name with `--name` (default `composed`) and a path with `--output`.
