//! Generate a [CycloneDX](https://cyclonedx.org/) 1.5 SBOM (JSON) from a solved
//! lock.
//!
//! A lock already pins every conda package with its exact version, build, and
//! content hash — a software bill of materials is a direct projection of that.
//! The document is **deterministic**: components are keyed by package URL and
//! emitted in sorted order, and no generation timestamp is included, so the same
//! lock always yields byte-identical SBOM output (itself a useful property to
//! attest).

use std::collections::BTreeMap;

use rattler_lock::LockFile;
use serde::Serialize;

/// Errors raised while generating an SBOM.
#[derive(Debug)]
pub enum SbomError {
    /// Reading conda records out of the lock failed.
    Lock(String),
    /// Serialising the SBOM document to JSON failed.
    Serialize(serde_json::Error),
}

impl std::fmt::Display for SbomError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SbomError::Lock(msg) => write!(f, "{msg}"),
            SbomError::Serialize(e) => write!(f, "serialising SBOM failed: {e}"),
        }
    }
}

impl std::error::Error for SbomError {}

#[derive(Serialize)]
struct Bom {
    #[serde(rename = "bomFormat")]
    bom_format: &'static str,
    #[serde(rename = "specVersion")]
    spec_version: &'static str,
    version: u32,
    metadata: Metadata,
    components: Vec<Component>,
}

#[derive(Serialize)]
struct Metadata {
    tools: Vec<Tool>,
}

#[derive(Serialize)]
struct Tool {
    vendor: &'static str,
    name: &'static str,
    version: &'static str,
}

#[derive(Serialize)]
struct Component {
    #[serde(rename = "bom-ref")]
    bom_ref: String,
    #[serde(rename = "type")]
    kind: &'static str,
    name: String,
    version: String,
    purl: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    hashes: Vec<Hash>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    licenses: Vec<LicenseChoice>,
}

#[derive(Serialize)]
struct Hash {
    alg: &'static str,
    content: String,
}

#[derive(Serialize)]
struct LicenseChoice {
    license: License,
}

#[derive(Serialize)]
struct License {
    name: String,
}

/// Build a conda package URL (purl): `pkg:conda/<name>@<version>` with `build`
/// and `subdir` qualifiers. conda names, versions, and builds use a purl-safe
/// character set, so no percent-encoding is required.
fn conda_purl(name: &str, version: &str, build: &str, subdir: &str) -> String {
    format!("pkg:conda/{name}@{version}?build={build}&subdir={subdir}")
}

/// Render `lock` as a CycloneDX 1.5 JSON SBOM. Every distinct conda package
/// across all environments and platforms in the lock becomes one component,
/// deduplicated and sorted by package URL.
pub fn to_cyclonedx(lock: &LockFile) -> Result<String, SbomError> {
    let mut components: BTreeMap<String, Component> = BTreeMap::new();

    for (_env_name, env) in lock.environments() {
        for platform in env.platforms() {
            let records = env
                .conda_repodata_records(platform)
                .map_err(|e| SbomError::Lock(format!("converting lock records: {e}")))?
                .unwrap_or_default();
            for record in records {
                let pr = &record.package_record;
                let name = pr.name.as_normalized().to_string();
                let version = pr.version.as_str().to_string();
                let build = pr.build.clone();
                let subdir = pr.subdir.clone();
                let purl = conda_purl(&name, &version, &build, &subdir);
                if components.contains_key(&purl) {
                    continue;
                }
                let hashes = pr
                    .sha256
                    .as_ref()
                    .map(|digest| {
                        vec![Hash {
                            alg: "SHA-256",
                            content: hex::encode(digest),
                        }]
                    })
                    .unwrap_or_default();
                let licenses = pr
                    .license
                    .as_ref()
                    .filter(|l| !l.is_empty())
                    .map(|name| {
                        vec![LicenseChoice {
                            license: License { name: name.clone() },
                        }]
                    })
                    .unwrap_or_default();
                components.insert(
                    purl.clone(),
                    Component {
                        bom_ref: purl.clone(),
                        kind: "library",
                        name,
                        version,
                        purl,
                        hashes,
                        licenses,
                    },
                );
            }
        }
    }

    let bom = Bom {
        bom_format: "CycloneDX",
        spec_version: "1.5",
        version: 1,
        metadata: Metadata {
            tools: vec![Tool {
                vendor: "nepenthe",
                name: "nepenthe",
                version: env!("CARGO_PKG_VERSION"),
            }],
        },
        components: components.into_values().collect(),
    };

    serde_json::to_string_pretty(&bom).map_err(SbomError::Serialize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conda_purl_carries_build_and_subdir() {
        assert_eq!(
            conda_purl("numpy", "2.1.0", "py311h0001_0", "linux-64"),
            "pkg:conda/numpy@2.1.0?build=py311h0001_0&subdir=linux-64"
        );
    }

    /// A minimal real lock (one package) renders a valid, deterministic
    /// CycloneDX document with the package as a component.
    #[test]
    fn renders_cyclonedx_from_a_lock() {
        let yaml = r#"version: 6
environments:
  default:
    channels:
    - url: https://conda.anaconda.org/conda-forge/
    packages:
      linux-64:
      - conda: https://conda.anaconda.org/conda-forge/linux-64/ca-certificates-2024.8.30-hbcca054_0.conda
packages:
- conda: https://conda.anaconda.org/conda-forge/linux-64/ca-certificates-2024.8.30-hbcca054_0.conda
  sha256: 0a8c9a0b0a0d0e0f0102030405060708090a0b0c0d0e0f101112131415161718
  md5: 9c12429eb8e07e7c5d36a8b8b0d0e0f0
  license: ISC
  size: 159003
  timestamp: 1725018903918
"#;
        let lock = LockFile::from_str_with_base_directory(yaml, None).expect("valid lock");
        let json = to_cyclonedx(&lock).expect("renders");
        let doc: serde_json::Value = serde_json::from_str(&json).expect("valid json");

        assert_eq!(doc["bomFormat"], "CycloneDX");
        assert_eq!(doc["specVersion"], "1.5");
        let components = doc["components"].as_array().expect("components array");
        assert_eq!(components.len(), 1);
        let c = &components[0];
        assert_eq!(c["name"], "ca-certificates");
        assert_eq!(c["type"], "library");
        assert_eq!(
            c["purl"],
            "pkg:conda/ca-certificates@2024.8.30?build=hbcca054_0&subdir=linux-64"
        );
        assert_eq!(c["licenses"][0]["license"]["name"], "ISC");
        assert_eq!(c["hashes"][0]["alg"], "SHA-256");

        // Deterministic: same lock → byte-identical output.
        assert_eq!(json, to_cyclonedx(&lock).expect("renders again"));
    }
}
