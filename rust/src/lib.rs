//! `nepenthe-core` — foundational library for nepenthe.
//!
//! This crate will grow to hold the manifest model (features, environments,
//! channels), feature/environment composition, override layering, the solver
//! core, and the install side. Today it exposes the crate version, the
//! [`manifest`] model, and two seam modules that pin the key dependencies:
//!
//! - [`manifest`] — the YAML environment manifest, composition, and lints
//! - [`backend`] — spec storage via `fsspec_rs`
//! - [`solve`] — the solver core via rattler
//! - [`export`] — lock + `@EXPLICIT`/`environment.yml` exports
//! - [`registry`] — versioned index + label resolution over a spec backend
//! - [`install`] — install a lock into a prefix (no conda), diff/status/activate
//! - [`embed`] — embed/extract the manifest in a lock's leading comment band
//! - [`pack`] — air-gapped bundles: pack a lock's packages, install offline
//! - [`producer`] — build orchestration: manifest → solve → locks → publish
//! - [`project`] — consumer `pyproject.toml` integration: sync + dependency check
//! - [`run`] — run a command in a base environment plus a conda overlay
//! - [`name_map`] — PyPI ↔ conda package-name mapping (grayskull-derived)
//! - [`cli`] — the `nepenthe` command-line interface (shared by all binaries)

pub mod backend;
pub mod cli;
pub mod embed;
pub mod export;
pub mod image;
pub mod install;
pub mod manifest;
pub mod name_map;
pub mod net;
pub mod pack;
pub mod producer;
pub mod project;
pub mod registry;
pub mod run;
pub mod solve;

/// Returns the `nepenthe-core` crate version (from `CARGO_PKG_VERSION`).
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Returns the current platform as a conda subdir string (e.g. `linux-64`).
pub fn current_platform() -> String {
    rattler_conda_types::Platform::current().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_non_empty() {
        assert!(!version().is_empty());
    }
}
