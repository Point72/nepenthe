//! `nepenthe license`: report the licenses of a lock's conda packages and flag
//! any that violate a deny policy.
//!
//! The conda `license` field is a free-form string (often an SPDX expression
//! like `BSD-3-Clause`, sometimes a compound `GPL-2.0-or-later AND MIT`, or
//! empty). This module reports it verbatim, grouped by license, and matches a
//! deny list against it case-insensitively and exactly — a pragmatic policy
//! gate, not a full SPDX expression evaluator.

use std::collections::{BTreeMap, BTreeSet};

use rattler_lock::LockFile;

/// What can go wrong while building a license report.
#[derive(Debug)]
pub enum LicenseError {
    /// Reading the lock's package records failed.
    Lock(String),
}

impl std::fmt::Display for LicenseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LicenseError::Lock(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for LicenseError {}

/// The license shown for a package with no declared license.
pub const UNKNOWN: &str = "UNKNOWN";

/// Group a lock's unique conda packages by their declared license. Packages
/// with no license are collected under [`UNKNOWN`]. Each package appears once
/// per license as `"<name> <version>"`, sorted; the map is sorted by license.
pub fn collect(lock: &LockFile) -> Result<BTreeMap<String, Vec<String>>, LicenseError> {
    let mut by_license: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (_env_name, env) in lock.environments() {
        for platform in env.platforms() {
            let records = env
                .conda_repodata_records(platform)
                .map_err(|e| LicenseError::Lock(format!("converting lock records: {e}")))?
                .unwrap_or_default();
            for record in records {
                let pr = &record.package_record;
                let license = pr
                    .license
                    .as_ref()
                    .filter(|l| !l.is_empty())
                    .cloned()
                    .unwrap_or_else(|| UNKNOWN.to_string());
                let pkg = format!("{} {}", pr.name.as_normalized(), pr.version.as_str());
                by_license.entry(license).or_default().insert(pkg);
            }
        }
    }
    Ok(by_license
        .into_iter()
        .map(|(license, pkgs)| (license, pkgs.into_iter().collect()))
        .collect())
}

/// The packages whose license is in `deny` (case-insensitive, exact match on
/// the license string), as `(license, package)` pairs, sorted.
pub fn flagged(
    by_license: &BTreeMap<String, Vec<String>>,
    deny: &[String],
) -> Vec<(String, String)> {
    let denied: BTreeSet<String> = deny.iter().map(|d| d.to_lowercase()).collect();
    let mut out = Vec::new();
    for (license, pkgs) in by_license {
        if denied.contains(&license.to_lowercase()) {
            for pkg in pkgs {
                out.push((license.clone(), pkg.clone()));
            }
        }
    }
    out
}

/// Render a deterministic text report: each license, the count of packages, and
/// the package list.
pub fn render_text(by_license: &BTreeMap<String, Vec<String>>) -> String {
    let mut out = String::new();
    for (license, pkgs) in by_license {
        out.push_str(&format!("{license} ({})\n", pkgs.len()));
        for pkg in pkgs {
            out.push_str(&format!("  {pkg}\n"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCK: &str = r#"version: 6
environments:
  default:
    channels:
    - url: https://conda.anaconda.org/conda-forge/
    packages:
      linux-64:
      - conda: https://conda.anaconda.org/conda-forge/linux-64/aaa-1.0-h0_0.conda
      - conda: https://conda.anaconda.org/conda-forge/linux-64/bbb-2.0-h0_0.conda
packages:
- conda: https://conda.anaconda.org/conda-forge/linux-64/aaa-1.0-h0_0.conda
  sha256: 0a8c9a0b0a0d0e0f0102030405060708090a0b0c0d0e0f101112131415161718
  md5: 9c12429eb8e07e7c5d36a8b8b0d0e0f0
  license: MIT
  size: 1
  timestamp: 1725018903918
- conda: https://conda.anaconda.org/conda-forge/linux-64/bbb-2.0-h0_0.conda
  sha256: 1b8c9a0b0a0d0e0f0102030405060708090a0b0c0d0e0f101112131415161719
  md5: 8c12429eb8e07e7c5d36a8b8b0d0e0f1
  license: GPL-3.0-or-later
  size: 1
  timestamp: 1725018903918
"#;

    fn lock() -> LockFile {
        LockFile::from_str_with_base_directory(LOCK, None).expect("valid lock")
    }

    #[test]
    fn collect_groups_by_license() {
        let by = collect(&lock()).expect("collect");
        assert_eq!(
            by.get("MIT").map(Vec::as_slice),
            Some(&["aaa 1.0".to_string()][..])
        );
        assert_eq!(
            by.get("GPL-3.0-or-later").map(Vec::as_slice),
            Some(&["bbb 2.0".to_string()][..])
        );
    }

    #[test]
    fn flagged_matches_deny_case_insensitively() {
        let by = collect(&lock()).expect("collect");
        // Deny GPL (exact string, any case) flags bbb but not aaa.
        let hits = flagged(&by, &["gpl-3.0-or-later".to_string()]);
        assert_eq!(
            hits,
            vec![("GPL-3.0-or-later".to_string(), "bbb 2.0".to_string())]
        );
        // A license not present flags nothing.
        assert!(flagged(&by, &["Apache-2.0".to_string()]).is_empty());
    }

    #[test]
    fn render_text_is_deterministic() {
        let by = collect(&lock()).expect("collect");
        let text = render_text(&by);
        // Sorted by license: GPL before MIT.
        assert!(text.find("GPL-3.0-or-later").unwrap() < text.find("MIT").unwrap());
        assert!(text.contains("MIT (1)"));
        assert!(text.contains("  aaa 1.0"));
    }
}
