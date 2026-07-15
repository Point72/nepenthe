//! PyPI ↔ conda package-name mapping.
//!
//! conda and PyPI sometimes name the same project differently — `opencv-python`
//! on PyPI is `opencv` on conda-forge. [`check`](crate::project::check) matches a
//! project's PyPI requirements against an environment's conda packages, so when
//! a direct (PEP 503-normalized) name match fails it consults this mapping.
//!
//! The data is derived from conda-forge's grayskull PyPI→conda mapping. The vast
//! majority of grayskull's ~12k entries are *identity* mappings (the PyPI and
//! conda names already agree after normalization) and need no table — a direct
//! name match handles them. Only the **divergent** pairs are vendored here, in
//! `data/pypi_to_conda.tsv` (a few hundred entries, a few KB).
//!
//! ## Regenerating the vendored table
//!
//! The table is reproducible from the upstream source. [`reduce_grayskull`] is
//! the pure reducer (fetch → reduce → write); the `regenerate_name_map` example
//! wires it to the network and writes the artifact:
//!
//! ```bash
//! cargo run --example regenerate_name_map
//! ```

use std::collections::BTreeMap;
use std::sync::OnceLock;

use serde::Deserialize;

/// Upstream source of the PyPI→conda mapping (conda-forge's grayskull data).
pub const GRAYSKULL_URL: &str = "https://raw.githubusercontent.com/conda-forge/conda-forge-bot-data/refs/heads/main/mappings/pypi/grayskull_pypi_mapping.yaml";

/// The vendored divergent pairs: `<normalized-pypi-name>\t<conda-name>` per line.
const VENDORED: &str = include_str!("data/pypi_to_conda.tsv");

/// Parse the vendored table once, on first use.
fn table() -> &'static BTreeMap<String, String> {
    static TABLE: OnceLock<BTreeMap<String, String>> = OnceLock::new();
    TABLE.get_or_init(|| parse_tsv(VENDORED))
}

/// The conda package name for a PyPI distribution name **when it differs** from
/// the PyPI name. `pypi_name` is matched after [`normalize_name`]; the returned
/// conda name is the raw conda package name. Returns `None` when there is no
/// divergent mapping — the caller should then fall back to the normalized PyPI
/// name itself (which covers the identity majority).
pub fn pypi_to_conda(pypi_name: &str) -> Option<&'static str> {
    table().get(&normalize_name(pypi_name)).map(String::as_str)
}

/// The number of divergent pairs in the vendored table.
pub fn len() -> usize {
    table().len()
}

fn parse_tsv(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            let (pypi, conda) = line.split_once('\t')?;
            let (pypi, conda) = (pypi.trim(), conda.trim());
            if pypi.is_empty() || conda.is_empty() {
                return None;
            }
            Some((pypi.to_string(), conda.to_string()))
        })
        .collect()
}

/// Normalize a package name per [PEP 503](https://peps.python.org/pep-0503/):
/// lowercase, with runs of `-`, `_`, `.` collapsed to a single `-`. conda names
/// are already lowercase-with-dashes, so this yields a common key for matching
/// PyPI requirements against conda packages.
pub fn normalize_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut prev_sep = false;
    for c in name.chars() {
        if c == '-' || c == '_' || c == '.' {
            if !out.is_empty() && !prev_sep {
                out.push('-');
            }
            prev_sep = true;
        } else {
            out.push(c.to_ascii_lowercase());
            prev_sep = false;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// One entry in the grayskull mapping YAML (other fields are ignored).
#[derive(Deserialize)]
struct GrayskullEntry {
    #[serde(default)]
    conda_name: Option<String>,
    #[serde(default)]
    pypi_name: Option<String>,
}

/// Reduce a grayskull PyPI→conda mapping (the full upstream YAML) into the
/// compact vendored table: sorted `<normalized-pypi-name>\t<conda-name>` lines,
/// keeping only the **divergent** pairs.
///
/// Identity mappings (PyPI and conda names equal after normalization) are
/// dropped — a direct name match covers them. Hash-like junk keys (grayskull
/// records some entries under a long hex digest) and entries without a conda
/// name are skipped. The output is deterministic (sorted), so a regenerated
/// table diffs cleanly against the vendored one.
pub fn reduce_grayskull(yaml: &str) -> Result<String, serde_yaml::Error> {
    let data: BTreeMap<String, GrayskullEntry> = serde_yaml::from_str(yaml)?;

    let mut pairs: BTreeMap<String, String> = BTreeMap::new();
    for (key, entry) in data {
        let pypi = entry.pypi_name.unwrap_or(key);
        let Some(conda) = entry.conda_name else {
            continue;
        };
        if is_hash_like(&pypi) {
            continue;
        }
        let normalized_pypi = normalize_name(&pypi);
        let normalized_conda = normalize_name(&conda);
        if normalized_pypi.is_empty() || normalized_conda.is_empty() {
            continue;
        }
        if normalized_pypi == normalized_conda {
            continue;
        }
        pairs.insert(normalized_pypi, conda);
    }

    let mut out = String::new();
    for (pypi, conda) in &pairs {
        out.push_str(pypi);
        out.push('\t');
        out.push_str(conda);
        out.push('\n');
    }
    Ok(out)
}

/// Whether a name is a long hexadecimal digest (grayskull stores some entries
/// keyed by a digest rather than a real distribution name).
fn is_hash_like(name: &str) -> bool {
    name.len() >= 40 && name.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_name_follows_pep503() {
        assert_eq!(normalize_name("Flask_SQLAlchemy"), "flask-sqlalchemy");
        assert_eq!(normalize_name("ruamel.yaml"), "ruamel-yaml");
        assert_eq!(normalize_name("OpenCV-Python"), "opencv-python");
        assert_eq!(normalize_name("numpy"), "numpy");
    }

    #[test]
    fn vendored_table_loads_and_maps_known_names() {
        // A couple of well-known divergent names from grayskull.
        assert_eq!(pypi_to_conda("opencv-python"), Some("opencv"));
        assert_eq!(pypi_to_conda("opencv_python"), Some("opencv")); // normalized
        assert_eq!(pypi_to_conda("tables"), Some("pytables"));
        // An identity name is not in the divergent table.
        assert_eq!(pypi_to_conda("numpy"), None);
        assert!(len() > 50);
    }

    #[test]
    fn reduce_keeps_only_divergent_pairs() {
        let yaml = r#"
numpy:
  conda_name: numpy
  pypi_name: numpy
  mapping_source: regro-bot
opencv-python:
  conda_name: opencv
  pypi_name: opencv-python
  mapping_source: regro-bot
Flask_Thing:
  conda_name: flask-thing
  pypi_name: Flask_Thing
deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef:
  conda_name: junk
  pypi_name: deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef
no-conda:
  pypi_name: no-conda
"#;
        let reduced = reduce_grayskull(yaml).unwrap();
        // Only the genuinely-divergent opencv pair survives:
        // - numpy is identity, Flask_Thing normalizes to flask-thing (identity),
        // - the hex key is hash-like junk, no-conda has no conda_name.
        assert_eq!(reduced, "opencv-python\topencv\n");
    }

    #[test]
    fn reduce_output_is_sorted() {
        let yaml = r#"
zzz-pkg:
  conda_name: zzz-other
  pypi_name: zzz-pkg
aaa-pkg:
  conda_name: aaa-other
  pypi_name: aaa-pkg
"#;
        let reduced = reduce_grayskull(yaml).unwrap();
        assert_eq!(reduced, "aaa-pkg\taaa-other\nzzz-pkg\tzzz-other\n");
    }
}
