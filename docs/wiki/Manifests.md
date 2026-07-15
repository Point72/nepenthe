# Manifests

A **manifest** is the declarative YAML description of one or more environments.
It replaces a folder of `requirements*.txt` files: channels, base dependencies,
composable features, build variants, and named environments all live in one
(optionally split) document.

This page covers the manifest format and how it composes. For terminology, see
[Concepts](Concepts); for pointing channels at your own servers, see
[Channels & Artifactory](Channels).

## A complete example

```yaml
project:
  name: demo
  channels: [conda-forge]          # priority order
  platforms: [linux-64]
  python: ["3.11", "3.12"]         # the python axis
  default-python: "3.11"

# base dependencies — injected into every environment.
# each entry is a conda match-spec STRING (quote-free, like environment.yml).
dependencies:
  - numpy >=2
  - pip

# build variants: a flavor an environment selects.
variants:
  cpu: {}                          # constraints can be filled by an override layer
  gpu:
    dependencies: [cuda >=12]
    constraints: ["pytorch * cuda*"]

# composable feature groups.
features:
  dev:
    dependencies: [pytest, ruff]
  ray:
    dependencies: [ray]
  docs:
    pypi-dependencies: [gendoc]    # pip-only routing is explicit, not name-munging

# environments compose features and fan out over variant × python.
environments:
  app:
    features: [dev, ray]
    variants: [cpu, gpu]
    default-variant: cpu
```

## Sections

### `project`
| Key | Meaning |
|-----|---------|
| `name` | Project name. |
| `channels` | Channel **names** in priority order (resolved to URLs — see [Channels](Channels)). |
| `channel-alias` | Base URL that bare channel names resolve against (e.g. an internal Artifactory). |
| `platforms` | Target platforms (`linux-64`, `osx-arm64`, `win-64`, …). |
| `python` | The python axis — each value produces one build target. |
| `default-python` | The python chosen when a selection omits one (must be in `python`). |

### `dependencies` / `pypi-dependencies`
Base dependencies injected into every environment. Conda deps are **match-spec
strings** (`numpy >=2`); PyPI deps are routed explicitly via
`pypi-dependencies` rather than name prefixes.

> PyPI resolution is not implemented yet. A manifest may declare
> `pypi-dependencies`, but driving a full solve over an environment that has
> them fails fast rather than silently producing a conda-only lock.

### `features`
Named, composable dependency groups. An environment lists the features it wants;
their dependencies union into the solve.

### `variants`
A build flavor (`cpu`, `gpu`, …) an environment selects. A variant carries its
own `dependencies`, `pypi-dependencies`, `constraints` (bound the solve without
adding a dependency), and `virtual-packages` (e.g. `cuda: "12.9"`).

### `activation`
Activation hooks run when the environment is **activated** (via
[`activate`](Install#activate) / `conda activate`), not merely placed on `PATH`.
They are materialized into the prefix's `etc/conda/activate.d/` on
[`create`](Install#create), and may be declared at the manifest base, on a
feature, and on a variant — they **merge** (env vars: later wins on a key clash;
scripts: appended in order). This replaces the need for per-environment
activation packages (e.g. ones that emit telemetry when an environment is
activated).

```yaml
activation:
  env:                             # exported on activation
    MY_FLAG: "1"
  scripts:                         # run after the env vars are set
    - 'echo "activated $NEPENTHE_ENVIRONMENT $NEPENTHE_VERSION"'
```

nepenthe always injects the environment's identity — `NEPENTHE_ENVIRONMENT`,
`NEPENTHE_PLATFORM`, and `NEPENTHE_VERSION` — before the declared env vars, so
hooks can reference it. Hooks run only under a **full activation**; `run` / `shell`
use path activation and do not execute them.

### `environments`
Either a bare list of feature names, or a detailed form:

```yaml
environments:
  app: [ray]                       # shorthand: just features

  full:
    features: [ray, dev]
    variants: [cpu, gpu]           # variant axis
    default-variant: cpu
    python: ["3.11"]               # narrow the python axis for this env
    platforms: [linux-64]

  win:
    extends: full                  # inherit features from another environment
    platforms: [win-64]            # override platforms

  private:
    features: [ray, dev]
    channels: [my-private-channel] # extra channel, only this env solves against
```

`extends` inherits the base environment's features; `variant`/`variants`,
`python`, and `platforms` can be set or overridden per environment. `channels`
adds channels (by name, resolved like any other) on top of the project channels
for just this environment — use it for a private channel only one environment
needs, so other environments don't require its credentials. See
[Channels](Channels#authentication).

## Composition & the build matrix

Resolving a `(variant, python)` cell unions: base deps → selected features
(incl. inherited) → selected variant → the chosen python (`python <ver>.*`).
Conda and PyPI deps stay separate.

```rust
use nepenthe_core::manifest::{Manifest, Selector};

let manifest = Manifest::load("environment.yaml")?;

// every build cell (variant × python), minus excluded ones
let cells = manifest.targets("app")?;

// the default cell (defaults for each axis)
let default = manifest.resolve_default("app")?;

// a specific cell
let gpu_312 = manifest.resolve("app", &Selector {
    variant: Some("gpu".into()),
    python: Some("3.12".into()),
})?;
```

A default that isn't a member of its axis (a typo in `default-variant` or
`default-python`) is rejected rather than silently producing an undeclared cell.

## Splitting across files: `imports`

A large definition can be split; each fragment is merged before the importing
file's own definitions:

```yaml
# environment.yaml
imports:
  - features.yaml
  - variants.yaml
dependencies: [numpy >=2]
environments:
  app: { features: [dev], variants: [cpu, gpu], default-variant: cpu }
```

Imports are **sandboxed**: an absolute path or one containing `..` is rejected,
so a manifest can't read files outside its own tree. Merging carries every
field — including `virtual-packages` and the `exclude`/`include` matrix tables.

See the runnable fixture set in `rust/testdata/e2e/` (`manifest.yaml` imports
`features.yaml` + `variants.yaml`, plus `overrides.yaml`).

## Override layers

An **override layer** is a separate YAML file that adjusts a solve without
editing the manifest. It supplies version pins, variant constraints,
virtual-package assumptions, and per-python matrix `exclude`/`include` tables:

```yaml
# overrides.yaml
pins:
  numpy: ">=2,<2.2"              # bake a version onto a bare dependency

variants:
  cpu:
    constraints: ["pytorch * cpu*"]   # fill the empty cpu variant

virtual-packages:
  cuda: "12.9"
  archspec: skylake_avx512

exclude:
  "3.13": [ccml]                # denylist: don't build ccml on python 3.13

include:
  "3.13": [ccrt, gmor]          # allowlist: on 3.13 build ONLY these (shorthand
                                # when fewer are built than excluded)
```

Apply it to a manifest:

```rust
use nepenthe_core::manifest::{Manifest, Overrides};

let mut manifest = Manifest::load("environment.yaml")?;
let overrides = Overrides::from_yaml_path("overrides.yaml")?;
manifest.apply(&overrides);       // variant merge, pin baking, matrix pruning, vpkg recording
```

Applying an override:
- **merges** each variant's deps/constraints/virtual-packages into the manifest's variant (filling, e.g., an empty `cpu: {}`),
- **bakes pins** into every matching conda dependency (`numpy` → `numpy >=2,<2.2`), keyed by package name,
- **records** global virtual packages, and
- **prunes** the build matrix via the `exclude` denylist and `include` allowlist (a cell is built only if its environment is allowed by `include` for that python — when present — and not named in `exclude`; `exclude` wins on overlap).

## Validation lints

`Manifest::lint()` reports common problems before a solve:

- **HardPin** — a `==` pin in base or a feature (locations reported).
- **TooManyHardPins** — an environment exceeds the hard-pin budget.
- **BaseFeatureCollision** — a feature re-declares a base package.
