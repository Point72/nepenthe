`nepenthe` is written in Python and Rust and provides prebuilt wheels for end
users. To build `nepenthe` from source, use its Python [source distribution](https://packaging.python.org/en/latest/specifications/source-distribution-format/)
or clone the GitHub repository.

- [Make commands](#make-commands)
- [Prerequisites](#prerequisites)
- [Clone](#clone)
- [Install Python dependencies](#install-python-dependencies)
- [Build](#build)
- [Lint and Autoformat](#lint-and-autoformat)
- [Testing](#testing)

## Make commands

`nepenthe` uses a `Makefile` for commonly used commands. Run `make` with no
arguments to print the available commands.

```bash
> make

build                          build the library
clean                          clean the repository
fix                            run autofixers
install                        install library
lint                           run lints
test                           run the tests
```

## Prerequisites

`nepenthe` has a few system-level dependencies which you can install from your machine package manager. Other package managers like `conda`, `nix`, etc, should also work fine.

## Clone

Clone the repo with:

```bash
git clone https://github.com/Point72/nepenthe.git
cd nepenthe
```

## Install Rust

Follow the instructions for [installing Rust](https://rustup.rs) for your system.

## Install Python dependencies

Python build and develop dependencies are specified in the `pyproject.toml`, but you can manually install them:

```bash
make requirements
```

The [PEP517](https://peps.python.org/pep-0517/) / [PEP518](https://peps.python.org/pep-0518/)
build process also installs these dependencies.

## Build

Build the Python package:

```bash
make build
```

## Static standalone binary

The `nepenthe` CLI can be built as a single, fully static binary (musl, no glibc,
no Python required) that runs on any Linux host. Because the dependency tree
includes C code (`aws-lc-rs`, `zstd`), the build cross-compiles the C with
[`zig`](https://ziglang.org/) via
[`cargo-zigbuild`](https://github.com/rust-cross/cargo-zigbuild), so there is no
separate musl C toolchain to install:

```bash
# one-time tooling (zig is pulled in as a dependency)
pip install cargo-zigbuild        # or: conda install -c conda-forge cargo-zigbuild
rustup target add x86_64-unknown-linux-musl

# build it (from the rust/ directory)
cd rust
make dist-static
```

This produces a statically linked, stripped binary plus `np`/`npb` symlinks:

```text
target/x86_64-unknown-linux-musl/release/{nepenthe,np,npb}
```

Verify it carries no dynamic dependencies:

```bash
ldd target/x86_64-unknown-linux-musl/release/nepenthe
# => not a dynamic executable
```

Because `nepenthe` already uses `rustls` rather than OpenSSL throughout, the
static build needs no system TLS libraries. Override the target triple with
`make dist-static MUSL_TARGET=aarch64-unknown-linux-musl` for other platforms.

## Lint and Autoformat

`nepenthe` has linting and auto formatting.

| Language | Linter      | Autoformatter | Description |
| :------- | :---------- | :------------ | :---------- |
| Python   | `ruff`      | `ruff`        | Style       |
| Python   | `ruff`      | `ruff`        | Imports     |
| Rust     | `clippy`    | `clippy`      | Style       |
| Markdown | `mdformat`  | `mdformat`    | Style       |
| Markdown | `codespell` |               | Spelling    |

**Python Linting**

```bash
make lint-py
```

**Python Autoformatting**

```bash
make fix-py
```

**Rust Linting**

```bash
make lint-rs
```

**Rust Autoformatting**

```bash
make fix-rs
```

**Documentation Linting**

```bash
make lint-docs
```

**Documentation Autoformatting**

```bash
make fix-docs
```

## Testing

`nepenthe` has both Python and Rust tests. The bulk of the functionality is tested in Rust; the Python tests cover the binding and can be run via `pytest`. First, install the Python development dependencies with

```bash
make develop
```

**Python**

```bash
make test-py
```

**Rust**

```bash
make test-rs
```
