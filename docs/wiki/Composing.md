# Comparing & Composing Environments

Two registry-level workflows for working with **published** environments:
comparing what changed between releases, and unioning environments into one lock.

## Diffing two releases

`nepenthe diff-versions` shows what changed between two published versions of an
environment — added, removed, and version-changed packages — to inform an
upgrade before you take it:

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

`+` is new in the newer release, `-` was dropped, and `~` changed version. Select
the cell with `--platform`, `--python`, and `--variant` as for
[`create`](Install#create).

> This compares **published locks** (a registry-level diff). To compare an
> installed prefix against a lock instead, use [`diff`](Install#diff).

## Composing published environments

`nepenthe compose` unions two or more **published** environments into a single
lock — extending manifest-level feature composition to the published-artifact
level (e.g. a shared `base` plus a team's `ml-extras`):

```bash
nepenthe compose \
  --registry file:///srv/nepenthe --python 3.11 \
  --env base@1.3.0 --env ml-extras@2.1.0 \
  --name research --output research.lock
```

The result is a normal lock you can publish and install like any other:

```bash
nepenthe publish research --lock research.lock \
  --registry file:///srv/nepenthe --python 3.11 --version 1.0.0
nepenthe create research --registry file:///srv/nepenthe --python 3.11 --prefix ./env
```

### How it works

- Packages are **unioned by name** across the inputs, for every platform common
  to all of them. The first input to provide a package wins.
- Channels are unioned in input order.
- If two inputs pin the **same package to a different version or build**, compose
  fails with a clear conflict (it does **not** silently pick one):

  ```text
  error: cannot compose: 'numpy' is pinned to 2.4.6=py311h0 and 2.2.6=py311h1 on linux-64
  ```

  Resolve it by aligning the inputs' versions (or composing compatible releases).

> Compose is a **set union of frozen locks**, not a re-solve. It is exact and
> fast, but it cannot reconcile incompatible pins — that is what a producer-side
> re-solve ([`build`](Install)) is for. Use [`try`](Install) to check whether a
> set of requirements would co-solve before publishing.

### Inputs

Each `--env` is `name` or `name@version` (default `latest`). All inputs must
share the selected `--platform` / `--python` / `--variant` cell. Give the output
a name with `--name` (default `composed`) and a path with `--output`.
