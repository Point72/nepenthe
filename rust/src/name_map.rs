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

/// The lookup key for a requirement that carries an extras group: the
/// normalized distribution name followed by its sorted, normalized extras.
pub fn extras_key(name: &str, extras: &[String]) -> String {
    let mut extras: Vec<String> = extras.iter().map(|e| normalize_name(e)).collect();
    extras.sort();
    extras.dedup();
    format!("{}[{}]", normalize_name(name), extras.join(","))
}

/// Project-supplied additions to the vendored table.
///
/// The vendored table covers what conda-forge knows about. A project that
/// depends on distributions packaged outside conda-forge — or that packages its
/// own — needs to say how those names translate, without waiting for the
/// vendored table to be regenerated.
///
/// Two kinds of override are supported:
///
/// - **package**: a distribution name maps to a different conda package name,
///   overriding the vendored entry (or supplying one where there is none).
/// - **extras**: a requirement's extras group maps to *several* conda packages.
///   conda has no equivalent of an extras group, so a distribution that splits
///   its optional features into separate conda packages cannot be expressed as
///   a rename.
///
/// An extras group with no override resolves to the distribution name alone,
/// which is what the extras-less requirement would have produced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Overrides {
    packages: BTreeMap<String, String>,
    extras: BTreeMap<String, Vec<String>>,
}

impl Overrides {
    /// Build from raw (un-normalized) `package` and `extras` tables. `packages`
    /// is keyed by distribution name; `extras` by a `name[extra,…]` string.
    /// Keys are normalized, so casing, separator style and extras order in the
    /// source are irrelevant. An `extras` key without a `[…]` group is ignored.
    pub fn new(
        packages: impl IntoIterator<Item = (String, String)>,
        extras: impl IntoIterator<Item = (String, Vec<String>)>,
    ) -> Self {
        let packages = packages
            .into_iter()
            .map(|(pypi, conda)| (normalize_name(&pypi), conda))
            .collect();
        let extras = extras
            .into_iter()
            .filter_map(|(key, conda)| Some((normalize_extras_key(&key)?, conda)))
            .collect();
        Self { packages, extras }
    }

    /// Whether any override is configured.
    pub fn is_empty(&self) -> bool {
        self.packages.is_empty() && self.extras.is_empty()
    }

    /// The conda package name(s) a requirement resolves to. `extras` is the
    /// requirement's extras group, empty when it has none.
    ///
    /// An extras override wins; otherwise a package override; otherwise the
    /// vendored table; otherwise the normalized name itself.
    pub fn conda_names(&self, pypi_name: &str, extras: &[String]) -> Vec<String> {
        if !extras.is_empty() {
            if let Some(names) = self.extras.get(&extras_key(pypi_name, extras)) {
                return names.clone();
            }
        }
        vec![self.conda_name(pypi_name)]
    }

    /// The single conda package name a distribution name resolves to, ignoring
    /// any extras group.
    pub fn conda_name(&self, pypi_name: &str) -> String {
        let normalized = normalize_name(pypi_name);
        if let Some(conda) = self.packages.get(&normalized) {
            return conda.clone();
        }
        pypi_to_conda(&normalized)
            .map(str::to_string)
            .unwrap_or(normalized)
    }
}

/// Normalize a `name[extra,…]` key. Returns `None` when there is no extras
/// group, or when the group is empty or unterminated.
fn normalize_extras_key(key: &str) -> Option<String> {
    let (name, rest) = key.split_once('[')?;
    let extras = rest.strip_suffix(']')?;
    let extras: Vec<String> = extras
        .split(',')
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .map(str::to_string)
        .collect();
    if name.trim().is_empty() || extras.is_empty() {
        return None;
    }
    Some(extras_key(name.trim(), &extras))
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
    fn overrides_default_to_the_vendored_table() {
        let overrides = Overrides::default();
        assert!(overrides.is_empty());
        assert_eq!(overrides.conda_name("opencv-python"), "opencv");
        assert_eq!(overrides.conda_name("numpy"), "numpy");
        assert_eq!(overrides.conda_name("Flask_SQLAlchemy"), "flask-sqlalchemy");
    }

    #[test]
    fn a_package_override_wins_over_the_vendored_table() {
        let overrides = Overrides::new(
            [("OpenCV_Python".to_string(), "example-opencv".to_string())],
            [],
        );
        assert!(!overrides.is_empty());
        // The key is normalized, so any spelling of the requirement matches.
        assert_eq!(overrides.conda_name("opencv-python"), "example-opencv");
    }

    #[test]
    fn an_extras_override_expands_to_several_packages() {
        let overrides = Overrides::new(
            [],
            [(
                "Example.Package[Two, One]".to_string(),
                vec![
                    "example-package".to_string(),
                    "example-package-extra".to_string(),
                ],
            )],
        );
        // Extras order and separator style in the key do not matter.
        assert_eq!(
            overrides.conda_names("example-package", &["one".to_string(), "two".to_string()]),
            vec!["example-package", "example-package-extra"]
        );
        // A different extras group is not the same key.
        assert_eq!(
            overrides.conda_names("example-package", &["one".to_string()]),
            vec!["example-package"]
        );
    }

    #[test]
    fn an_unmapped_extras_group_resolves_to_the_name_alone() {
        let overrides = Overrides::default();
        assert_eq!(
            overrides.conda_names("opencv-python", &["extra".to_string()]),
            vec!["opencv"]
        );
    }

    #[test]
    fn extras_keys_without_a_group_are_ignored() {
        let overrides = Overrides::new(
            [],
            [
                ("example-package".to_string(), vec!["a".to_string()]),
                ("example-package[]".to_string(), vec!["b".to_string()]),
                ("[extra]".to_string(), vec!["c".to_string()]),
            ],
        );
        assert!(overrides.is_empty());
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
