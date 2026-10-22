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

Install system dependencies with your machine's package manager or a package
manager such as `conda` or `nix`.

## Clone

Clone the repo with:

```bash
git clone https://github.com/Point72/nepenthe.git
cd nepenthe
```

## Install Rust

Follow the instructions for [installing Rust](https://rustup.rs) for your system.

## Install Python dependencies

Install the build and development dependencies listed in `pyproject.toml`:

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

The `nepenthe` CLI can be built as a statically linked Linux binary using musl,
with no glibc or Python requirement. Its dependencies include C code
(`aws-lc-rs`, `zstd`), which is cross-compiled with
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

Verify that the binary has no dynamic dependencies:

```bash
ldd target/x86_64-unknown-linux-musl/release/nepenthe
# => not a dynamic executable
```

Because `nepenthe` uses `rustls` rather than OpenSSL, the
static build needs no system TLS libraries. Override the target triple with
`make dist-static MUSL_TARGET=aarch64-unknown-linux-musl` for other platforms.

## Lint and Autoformat

The project uses these linters and formatters:

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

Rust tests cover the core functionality; Python tests cover the bindings and run
with `pytest`. Install the development dependencies first:

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
