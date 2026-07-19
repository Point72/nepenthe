//! Registry & versioning.
//!
//! A backend-hosted **index** is the source of truth for resolution: it maps
//! `(environment, platform, python, variant, version)` to a content-addressed
//! lock object, so environments are versioned **independently** and labels
//! (`latest` / `latest-but-one` / an exact version / a semver range) resolve
//! against the index rather than by parsing encoded filenames. Published locks
//! are immutable and content-addressed; republishing a version with different
//! content is rejected.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::backend::fsspec_rs::FsError;
use crate::backend::{BackendError, SpecStore};

/// Name of the index object at the registry root.
const INDEX_FILE: &str = "index.yaml";

/// Errors raised by the registry layer.
#[derive(Debug)]
pub enum RegistryError {
    /// A backend read/write failed.
    Backend(BackendError),
    /// The index could not be parsed or serialised.
    Parse(serde_yaml::Error),
    /// A version string is not valid semver.
    InvalidVersion(String),
    /// A label range is not a valid semver requirement.
    InvalidRange(String),
    /// No release matches the requested coordinates and label.
    NotFound(String),
    /// A version is already published with different content (immutability).
    VersionExists(String),
    /// A lock address in the index is not a well-formed `sha256-<hex>`.
    InvalidLockAddress(String),
    /// A pulled lock object's bytes do not match its content address.
    IntegrityMismatch {
        /// The content address recorded in the index.
        expected: String,
        /// The address recomputed from the fetched bytes.
        actual: String,
    },
}

impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RegistryError::Backend(e) => write!(f, "registry backend failed: {e}"),
            RegistryError::Parse(e) => write!(f, "invalid registry index: {e}"),
            RegistryError::InvalidVersion(v) => write!(f, "invalid semver version '{v}'"),
            RegistryError::InvalidRange(r) => write!(f, "invalid semver range '{r}'"),
            RegistryError::NotFound(what) => write!(f, "no release for {what}"),
            RegistryError::VersionExists(what) => {
                write!(
                    f,
                    "version already published with different content: {what}"
                )
            }
            RegistryError::InvalidLockAddress(addr) => {
                write!(
                    f,
                    "invalid lock address '{addr}' (expected sha256-<64 hex>)"
                )
            }
            RegistryError::IntegrityMismatch { expected, actual } => {
                write!(
                    f,
                    "lock integrity check failed: index has {expected} but bytes hash to {actual}"
                )
            }
        }
    }
}

impl std::error::Error for RegistryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RegistryError::Backend(e) => Some(e),
            RegistryError::Parse(e) => Some(e),
            _ => None,
        }
    }
}

impl From<BackendError> for RegistryError {
    fn from(e: BackendError) -> Self {
        RegistryError::Backend(e)
    }
}

impl From<serde_yaml::Error> for RegistryError {
    fn from(e: serde_yaml::Error) -> Self {
        RegistryError::Parse(e)
    }
}

/// Content-address some bytes as `sha256-<hex>`. Identical bytes always produce
/// the same address, so a lock object is stored once regardless of how many
/// versions point at it.
pub fn content_address(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("sha256-{}", hex::encode(Sha256::digest(bytes)))
}

/// Validate a lock address as `sha256-<64 lowercase hex>`. A lock address is
/// concatenated into a backend path, so rejecting separators, `..`, and
/// non-hex characters prevents a tampered index from causing path traversal or
/// unintended backend requests.
fn validate_lock_address(addr: &str) -> Result<(), RegistryError> {
    let hex = addr
        .strip_prefix("sha256-")
        .ok_or_else(|| RegistryError::InvalidLockAddress(addr.to_string()))?;
    if hex.len() == 64
        && hex
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
    {
        Ok(())
    } else {
        Err(RegistryError::InvalidLockAddress(addr.to_string()))
    }
}

/// One published, content-addressed release in the index.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Release {
    /// Environment name.
    pub environment: String,
    /// Target platform (conda subdir, e.g. `linux-64`).
    pub platform: String,
    /// Python axis value, if the environment fans out over python.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub python: Option<String>,
    /// Variant axis value (e.g. `cpu`/`gpu`), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    /// Semver version of this release of the environment.
    pub version: String,
    /// Content address of the lock object (`sha256-<hex>`).
    pub lock: String,
    /// Content address of the manifest object (`sha256-<hex>`) the lock was
    /// solved from, if one was published alongside it. Lets a consumer recover
    /// the producer's manifest to re-solve (e.g. for a trial solve).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest: Option<String>,
    /// RFC3339 timestamp of when the release was published.
    pub created: String,
}

/// The registry index: the full set of published releases.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Index {
    /// Every published release, across all environments and platforms.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub releases: Vec<Release>,
}

/// Identifies one environment's version sequence: an environment on a platform,
/// optionally narrowed to a python and/or variant axis value.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Coordinates {
    /// Environment name.
    pub environment: String,
    /// Target platform.
    pub platform: String,
    /// Python axis value, if any.
    pub python: Option<String>,
    /// Variant axis value, if any.
    pub variant: Option<String>,
}

impl Coordinates {
    /// Coordinates for `environment` on `platform`, with no axis narrowing.
    pub fn new(environment: impl Into<String>, platform: impl Into<String>) -> Self {
        Self {
            environment: environment.into(),
            platform: platform.into(),
            python: None,
            variant: None,
        }
    }

    /// Narrow to a python axis value.
    pub fn with_python(mut self, python: impl Into<String>) -> Self {
        self.python = Some(python.into());
        self
    }

    /// Narrow to a variant axis value.
    pub fn with_variant(mut self, variant: impl Into<String>) -> Self {
        self.variant = Some(variant.into());
        self
    }

    /// Whether a release belongs to this version sequence.
    fn matches(&self, r: &Release) -> bool {
        r.environment == self.environment
            && r.platform == self.platform
            && r.python == self.python
            && r.variant == self.variant
    }
}

/// A request to resolve a version within a set of [`Coordinates`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Label {
    /// The highest published version.
    Latest,
    /// The second-highest published version (the previous release).
    LatestButOne,
    /// An exact version.
    Exact(String),
    /// The highest version satisfying a semver requirement (e.g. `>=1.2,<2`).
    Range(String),
}

impl Label {
    /// Parse a label string: `latest`, `latest-but-one`, a semver requirement
    /// (anything beginning with a comparator like `>`, `<`, `=`, `^`, `~`, `*`),
    /// or otherwise an exact version.
    pub fn parse(s: &str) -> Self {
        match s {
            "latest" => Label::Latest,
            "latest-but-one" => Label::LatestButOne,
            other if other.starts_with(['>', '<', '=', '^', '~', '*']) => {
                Label::Range(other.to_string())
            }
            other => Label::Exact(other.to_string()),
        }
    }
}

impl Index {
    /// Parse an index from YAML.
    pub fn from_yaml(yaml: &str) -> Result<Self, RegistryError> {
        Ok(serde_yaml::from_str(yaml)?)
    }

    /// The distinct environment names in the index, sorted.
    pub fn environments(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .releases
            .iter()
            .map(|r| r.environment.clone())
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// Every release of `environment` across all platforms/axes, sorted by
    /// version descending (highest first) then by platform.
    pub fn releases_of(&self, environment: &str) -> Vec<&Release> {
        let mut releases: Vec<&Release> = self
            .releases
            .iter()
            .filter(|r| r.environment == environment)
            .collect();
        releases.sort_by(|a, b| {
            let (av, bv) = (
                semver::Version::parse(&a.version).ok(),
                semver::Version::parse(&b.version).ok(),
            );
            bv.cmp(&av)
                .then_with(|| a.platform.cmp(&b.platform))
                .then_with(|| a.python.cmp(&b.python))
                .then_with(|| a.variant.cmp(&b.variant))
        });
        releases
    }

    /// Serialise the index to YAML.
    pub fn to_yaml(&self) -> Result<String, RegistryError> {
        Ok(serde_yaml::to_string(self)?)
    }

    /// Releases matching `coords`, paired with their parsed version and sorted
    /// by version descending (highest first).
    fn sorted_matches(
        &self,
        coords: &Coordinates,
    ) -> Result<Vec<(semver::Version, &Release)>, RegistryError> {
        let mut matches = Vec::new();
        for r in &self.releases {
            if coords.matches(r) {
                let ver = semver::Version::parse(&r.version)
                    .map_err(|_| RegistryError::InvalidVersion(r.version.clone()))?;
                matches.push((ver, r));
            }
        }
        matches.sort_by(|a, b| b.0.cmp(&a.0));
        Ok(matches)
    }

    /// Resolve a `label` within `coords` to the matching release.
    pub fn resolve(&self, coords: &Coordinates, label: &Label) -> Result<&Release, RegistryError> {
        let matches = self.sorted_matches(coords)?;
        let describe = || format!("{} on {}", coords.environment, coords.platform);
        if matches.is_empty() {
            return Err(RegistryError::NotFound(describe()));
        }
        match label {
            Label::Latest => Ok(matches[0].1),
            Label::LatestButOne => matches
                .get(1)
                .map(|m| m.1)
                .ok_or_else(|| RegistryError::NotFound(format!("{} (latest-but-one)", describe()))),
            Label::Exact(v) => matches
                .iter()
                .find(|(_, r)| &r.version == v)
                .map(|m| m.1)
                .ok_or_else(|| RegistryError::NotFound(format!("{} {v}", describe()))),
            Label::Range(range) => {
                let req = semver::VersionReq::parse(range)
                    .map_err(|_| RegistryError::InvalidRange(range.clone()))?;
                // matches are sorted descending, so the first satisfying the
                // requirement is the highest.
                matches
                    .iter()
                    .find(|(ver, _)| req.matches(ver))
                    .map(|m| m.1)
                    .ok_or_else(|| RegistryError::NotFound(format!("{} {range}", describe())))
            }
        }
    }
}

/// A versioned registry over a [`SpecStore`]. The index and the immutable,
/// content-addressed lock objects live under a single `root` URL on any
/// supported backend (`file://`, `s3://`, `https://`).
#[derive(Clone, Debug)]
pub struct Registry {
    store: SpecStore,
    root: String,
}

impl Registry {
    /// A registry rooted at `root` (e.g. `file:///srv/envs`, `s3://bucket/envs`)
    /// backed by `store`.
    pub fn new(store: SpecStore, root: impl Into<String>) -> Self {
        Self {
            store,
            root: root.into().trim_end_matches('/').to_string(),
        }
    }

    fn index_url(&self) -> String {
        format!("{}/{INDEX_FILE}", self.root)
    }

    fn lock_url(&self, address: &str) -> String {
        format!("{}/locks/{address}.lock", self.root)
    }

    fn manifest_url(&self, address: &str) -> String {
        format!("{}/manifests/{address}.yaml", self.root)
    }

    /// Load the index, returning an empty one if it does not yet exist.
    pub fn load_index(&self) -> Result<Index, RegistryError> {
        match self.store.get(&self.index_url()) {
            Ok(bytes) => Ok(serde_yaml::from_slice(&bytes)?),
            Err(BackendError::Fs(FsError::NotFound(_))) => Ok(Index::default()),
            Err(e) => Err(RegistryError::Backend(e)),
        }
    }

    fn save_index(&self, index: &Index) -> Result<(), RegistryError> {
        self.store
            .put(&self.index_url(), index.to_yaml()?.as_bytes())?;
        Ok(())
    }

    /// Publish `lock_bytes` as `version` of the environment at `coords`.
    ///
    /// The lock is content-addressed and written once (idempotent); the index
    /// gains a release pointing at it. Republishing the same `(coords, version)`
    /// with identical content is a no-op; with different content it is rejected
    /// to keep published versions immutable.
    pub fn publish(
        &self,
        coords: &Coordinates,
        version: &str,
        lock_bytes: &[u8],
    ) -> Result<Release, RegistryError> {
        self.publish_with_manifest(coords, version, lock_bytes, None)
    }

    /// Like [`publish`](Self::publish), but also store the `manifest_bytes` the
    /// lock was solved from as a content-addressed sidecar, recording its
    /// address on the release. The manifest is deduped across every release that
    /// shares it (one blob, many pointers), so a consumer can recover the
    /// producer's manifest to re-solve.
    pub fn publish_with_manifest(
        &self,
        coords: &Coordinates,
        version: &str,
        lock_bytes: &[u8],
        manifest_bytes: Option<&[u8]>,
    ) -> Result<Release, RegistryError> {
        semver::Version::parse(version)
            .map_err(|_| RegistryError::InvalidVersion(version.to_string()))?;

        let address = content_address(lock_bytes);
        let manifest_address = manifest_bytes.map(content_address);
        let mut index = self.load_index()?;

        if let Some(existing) = index
            .releases
            .iter()
            .find(|r| coords.matches(r) && r.version == version)
        {
            if existing.lock == address {
                return Ok(existing.clone());
            }
            return Err(RegistryError::VersionExists(format!(
                "{} {version} on {}",
                coords.environment, coords.platform
            )));
        }

        self.store.put(&self.lock_url(&address), lock_bytes)?;
        if let (Some(manifest_bytes), Some(manifest_address)) =
            (manifest_bytes, manifest_address.as_ref())
        {
            self.store
                .put(&self.manifest_url(manifest_address), manifest_bytes)?;
        }

        let release = Release {
            environment: coords.environment.clone(),
            platform: coords.platform.clone(),
            python: coords.python.clone(),
            variant: coords.variant.clone(),
            version: version.to_string(),
            lock: address,
            manifest: manifest_address,
            created: jiff::Timestamp::now().to_string(),
        };
        index.releases.push(release.clone());
        self.save_index(&index)?;
        Ok(release)
    }

    /// Resolve `label` within `coords` to a release (reads the index).
    pub fn resolve(&self, coords: &Coordinates, label: &Label) -> Result<Release, RegistryError> {
        let index = self.load_index()?;
        index.resolve(coords, label).cloned()
    }

    /// Resolve `label` within `coords` and return the bytes of its lock object.
    pub fn pull(&self, coords: &Coordinates, label: &Label) -> Result<Vec<u8>, RegistryError> {
        let release = self.resolve(coords, label)?;
        // A lock address from the index is used to build a backend path, so
        // reject anything that isn't a well-formed `sha256-<hex>` before using
        // it (guards against path traversal from a tampered index).
        validate_lock_address(&release.lock)?;
        let bytes = self.store.get(&self.lock_url(&release.lock))?;
        // Verify the bytes match the content address, rejecting tampered or
        // corrupt lock objects before they reach an installer.
        let actual = content_address(&bytes);
        if actual != release.lock {
            return Err(RegistryError::IntegrityMismatch {
                expected: release.lock,
                actual,
            });
        }
        Ok(bytes)
    }

    /// Resolve `label` within `coords` and return the manifest the release was
    /// solved from, if the producer published one as a sidecar. Returns
    /// `Ok(None)` when the release has no manifest sidecar.
    pub fn pull_manifest(
        &self,
        coords: &Coordinates,
        label: &Label,
    ) -> Result<Option<Vec<u8>>, RegistryError> {
        let release = self.resolve(coords, label)?;
        let Some(address) = release.manifest else {
            return Ok(None);
        };
        // The manifest address from the index builds a backend path, so reject
        // anything that isn't a well-formed `sha256-<hex>` (path-traversal guard).
        validate_lock_address(&address)?;
        let bytes = self.store.get(&self.manifest_url(&address))?;
        // Verify the bytes match the content address, rejecting tampered or
        // corrupt manifest objects.
        let actual = content_address(&bytes);
        if actual != address {
            return Err(RegistryError::IntegrityMismatch {
                expected: address,
                actual,
            });
        }
        Ok(Some(bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(env: &str, platform: &str, py: Option<&str>, version: &str, lock: &str) -> Release {
        Release {
            environment: env.to_string(),
            platform: platform.to_string(),
            python: py.map(String::from),
            variant: None,
            version: version.to_string(),
            lock: lock.to_string(),
            manifest: None,
            created: "2026-06-01T12:00:00Z".to_string(),
        }
    }

    #[test]
    fn content_address_is_stable_and_distinct() {
        let a = content_address(b"lock-bytes-a");
        assert_eq!(a, content_address(b"lock-bytes-a"));
        assert!(a.starts_with("sha256-"));
        assert_eq!(a.len(), "sha256-".len() + 64);
        assert_ne!(a, content_address(b"lock-bytes-b"));
    }

    #[test]
    fn index_yaml_round_trips() {
        let index = Index {
            releases: vec![
                release("myenv", "linux-64", Some("3.11"), "1.2.0", "sha256-aaa"),
                release("altenv", "osx-arm64", None, "0.4.1", "sha256-bbb"),
            ],
        };
        let yaml = index.to_yaml().expect("serialises");
        assert_eq!(Index::from_yaml(&yaml).expect("parses"), index);
    }

    #[test]
    fn label_parse_classifies_strings() {
        assert_eq!(Label::parse("latest"), Label::Latest);
        assert_eq!(Label::parse("latest-but-one"), Label::LatestButOne);
        assert_eq!(
            Label::parse(">=1.2,<2"),
            Label::Range(">=1.2,<2".to_string())
        );
        assert_eq!(Label::parse("1.3.0"), Label::Exact("1.3.0".to_string()));
    }

    #[test]
    fn resolve_latest_and_latest_but_one() {
        let index = Index {
            releases: vec![
                release("myenv", "linux-64", Some("3.11"), "1.0.0", "sha256-a"),
                release("myenv", "linux-64", Some("3.11"), "1.2.0", "sha256-c"),
                release("myenv", "linux-64", Some("3.11"), "1.1.0", "sha256-b"),
            ],
        };
        let coords = Coordinates::new("myenv", "linux-64").with_python("3.11");
        assert_eq!(
            index.resolve(&coords, &Label::Latest).unwrap().version,
            "1.2.0"
        );
        assert_eq!(
            index
                .resolve(&coords, &Label::LatestButOne)
                .unwrap()
                .version,
            "1.1.0"
        );
    }

    #[test]
    fn resolve_exact_and_range_pick_the_right_version() {
        let index = Index {
            releases: vec![
                release("myenv", "linux-64", None, "1.0.0", "sha256-a"),
                release("myenv", "linux-64", None, "1.1.0", "sha256-b"),
                release("myenv", "linux-64", None, "2.0.0", "sha256-c"),
            ],
        };
        let coords = Coordinates::new("myenv", "linux-64");
        assert_eq!(
            index
                .resolve(&coords, &Label::Exact("1.0.0".into()))
                .unwrap()
                .lock,
            "sha256-a"
        );
        // highest version satisfying the range
        assert_eq!(
            index
                .resolve(&coords, &Label::Range(">=1.0,<2".into()))
                .unwrap()
                .version,
            "1.1.0"
        );
    }

    #[test]
    fn lists_environments_and_releases_sorted() {
        let index = Index {
            releases: vec![
                release("zebra", "linux-64", None, "1.0.0", "sha256-a"),
                release("app", "linux-64", Some("3.11"), "1.0.0", "sha256-b"),
                release("app", "linux-64", Some("3.11"), "1.2.0", "sha256-c"),
                release("app", "osx-arm64", Some("3.11"), "1.1.0", "sha256-d"),
            ],
        };
        // distinct names, sorted
        assert_eq!(index.environments(), vec!["app", "zebra"]);
        // releases of "app": version descending, then platform
        let app = index.releases_of("app");
        assert_eq!(app.len(), 3);
        assert_eq!(app[0].version, "1.2.0");
        assert_eq!(app[1].version, "1.1.0");
        assert_eq!(app[2].version, "1.0.0");
        // an unknown environment has no releases
        assert!(index.releases_of("nope").is_empty());
    }

    #[test]
    fn resolve_filters_by_coordinates() {
        let index = Index {
            releases: vec![
                release("myenv", "linux-64", Some("3.11"), "1.0.0", "sha256-a"),
                release("myenv", "linux-64", Some("3.12"), "9.9.9", "sha256-z"),
                release("myenv", "osx-arm64", Some("3.11"), "5.0.0", "sha256-y"),
            ],
        };
        // only the 3.11 / linux-64 sequence is considered
        let coords = Coordinates::new("myenv", "linux-64").with_python("3.11");
        assert_eq!(
            index.resolve(&coords, &Label::Latest).unwrap().version,
            "1.0.0"
        );
    }

    #[test]
    fn resolve_unknown_is_not_found() {
        let index = Index::default();
        let coords = Coordinates::new("nope", "linux-64");
        assert!(matches!(
            index.resolve(&coords, &Label::Latest),
            Err(RegistryError::NotFound(_))
        ));
    }

    #[test]
    fn invalid_version_in_index_is_reported() {
        let index = Index {
            releases: vec![release("myenv", "linux-64", None, "not-semver", "sha256-a")],
        };
        let coords = Coordinates::new("myenv", "linux-64");
        assert!(matches!(
            index.resolve(&coords, &Label::Latest),
            Err(RegistryError::InvalidVersion(_))
        ));
    }

    /// End-to-end over a real `file://` registry (local I/O, no network):
    /// publish several versions of two environments, then resolve by label,
    /// pull lock bytes, and verify immutability offline.
    #[test]
    fn registry_publishes_and_resolves_two_environments() {
        let mut root = std::env::temp_dir();
        root.push(format!("nepenthe-registry-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let root_url = format!("file://{}", root.to_str().expect("utf-8 temp path"));

        let registry = Registry::new(SpecStore::new(), &root_url);
        let myenv = Coordinates::new("myenv", "linux-64").with_python("3.11");
        let altenv = Coordinates::new("altenv", "linux-64").with_python("3.11");

        // two independently-versioned environments
        registry
            .publish(&myenv, "1.0.0", b"myenv lock 1.0.0")
            .unwrap();
        registry
            .publish(&myenv, "1.1.0", b"myenv lock 1.1.0")
            .unwrap();
        registry
            .publish(&myenv, "1.2.0", b"myenv lock 1.2.0")
            .unwrap();
        registry
            .publish(&altenv, "0.4.0", b"altenv lock 0.4.0")
            .unwrap();
        registry
            .publish(&altenv, "0.5.0", b"altenv lock 0.5.0")
            .unwrap();

        // label resolution, no filename encoding
        assert_eq!(
            registry.resolve(&myenv, &Label::Latest).unwrap().version,
            "1.2.0"
        );
        assert_eq!(
            registry
                .resolve(&myenv, &Label::LatestButOne)
                .unwrap()
                .version,
            "1.1.0"
        );
        assert_eq!(
            registry.resolve(&altenv, &Label::Latest).unwrap().version,
            "0.5.0"
        );
        assert_eq!(
            registry
                .resolve(&myenv, &Label::parse(">=1.0,<1.2"))
                .unwrap()
                .version,
            "1.1.0"
        );

        // pull returns the exact bytes that were published
        assert_eq!(
            registry.pull(&myenv, &Label::Latest).unwrap(),
            b"myenv lock 1.2.0"
        );
        assert_eq!(
            registry
                .pull(&myenv, &Label::Exact("1.0.0".into()))
                .unwrap(),
            b"myenv lock 1.0.0"
        );

        // republishing identical content is idempotent
        registry
            .publish(&myenv, "1.2.0", b"myenv lock 1.2.0")
            .unwrap();
        // republishing a version with different content is rejected (immutable)
        assert!(matches!(
            registry.publish(&myenv, "1.2.0", b"tampered"),
            Err(RegistryError::VersionExists(_))
        ));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn publishes_and_pulls_a_manifest_sidecar() {
        let mut root = std::env::temp_dir();
        root.push(format!("nepenthe-registry-manifest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let root_url = format!("file://{}", root.display());
        let registry = Registry::new(SpecStore::new(), &root_url);

        let coords = Coordinates::new("app", "linux-64").with_python("3.11");
        let manifest = b"project:\n  name: demo\ndependencies:\n  - numpy >=2\n";

        // publish with a manifest sidecar; the release records its address
        let release = registry
            .publish_with_manifest(&coords, "1.0.0", b"app lock 1.0.0", Some(manifest))
            .unwrap();
        assert!(release.manifest.is_some());

        // the sidecar round-trips
        assert_eq!(
            registry
                .pull_manifest(&coords, &Label::Latest)
                .unwrap()
                .as_deref(),
            Some(&manifest[..])
        );

        // a second cell sharing the same manifest dedups to one blob
        let coords2 = Coordinates::new("app", "linux-64").with_python("3.12");
        let release2 = registry
            .publish_with_manifest(&coords2, "1.0.0", b"app lock py312", Some(manifest))
            .unwrap();
        assert_eq!(release.manifest, release2.manifest);
        let blobs = std::fs::read_dir(root.join("manifests")).unwrap().count();
        assert_eq!(blobs, 1, "identical manifests should dedup to one blob");

        // a release published without a sidecar reports no manifest
        let plain = Coordinates::new("bare", "linux-64");
        registry.publish(&plain, "1.0.0", b"bare lock").unwrap();
        assert_eq!(
            registry.pull_manifest(&plain, &Label::Latest).unwrap(),
            None
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn validate_lock_address_accepts_valid_and_rejects_bad() {
        let good = content_address(b"hello");
        assert!(validate_lock_address(&good).is_ok());
        assert!(validate_lock_address("sha256-deadbeef").is_err()); // too short
        assert!(validate_lock_address("md5-abc").is_err()); // wrong prefix
        assert!(validate_lock_address("sha256-../../etc/passwd").is_err());
        // uppercase hex rejected: the format is lowercase
        let upper = format!("sha256-{}", "A".repeat(64));
        assert!(validate_lock_address(&upper).is_err());
    }

    #[test]
    fn pull_rejects_tampered_lock_bytes() {
        let mut root = std::env::temp_dir();
        root.push(format!("nepenthe-registry-tamper-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let root_url = format!("file://{}", root.to_str().expect("utf-8 temp path"));

        let registry = Registry::new(SpecStore::new(), &root_url);
        let coords = Coordinates::new("app", "linux-64");
        let release = registry.publish(&coords, "1.0.0", b"genuine lock").unwrap();

        // Overwrite the stored lock object with different bytes, leaving the
        // index (and its recorded content address) untouched.
        registry
            .store
            .put(&registry.lock_url(&release.lock), b"tampered")
            .unwrap();

        assert!(matches!(
            registry.pull(&coords, &Label::Latest),
            Err(RegistryError::IntegrityMismatch { .. })
        ));

        std::fs::remove_dir_all(&root).ok();
    }

    /// Capstone: solve python live, export real `rattler_lock`
    /// locks, publish two versions to a `file://` registry, resolve `latest`,
    /// pull the bytes, and confirm they reparse as a valid lock. Ignored by
    /// default so CI stays offline; run with `cargo test -- --ignored`.
    #[ignore = "requires network access to conda-forge"]
    #[tokio::test]
    async fn real_registry_round_trips_a_solved_lock() {
        use crate::export::to_lockfile_string;
        use crate::solve::{solve, ChannelSettings, SolveRequest};
        use rattler_lock::LockFile;

        let base = SolveRequest {
            channels: vec!["conda-forge".to_string()],
            platform: "linux-64".to_string(),
            specs: vec!["python 3.11.*".to_string()],
            ..Default::default()
        };
        // an older repodata cutoff yields an older python patch, so the two
        // locks genuinely differ in content (and thus content address).
        let old = solve(
            &base.clone().with_exclude_newer("2025-01-01T00:00:00Z"),
            &ChannelSettings::default(),
        )
        .await
        .expect("old solve");
        let new = solve(&base, &ChannelSettings::default())
            .await
            .expect("new solve");

        let old_lock = to_lockfile_string(&old, "default").expect("old lock");
        let new_lock = to_lockfile_string(&new, "default").expect("new lock");
        assert_ne!(old_lock, new_lock, "expected two distinct locks");

        let mut root = std::env::temp_dir();
        root.push(format!("nepenthe-registry-live-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let root_url = format!("file://{}", root.to_str().expect("utf-8 temp path"));

        let registry = Registry::new(SpecStore::new(), &root_url);
        let coords = Coordinates::new("py", "linux-64").with_python("3.11");
        registry
            .publish(&coords, "1.0.0", old_lock.as_bytes())
            .unwrap();
        registry
            .publish(&coords, "1.1.0", new_lock.as_bytes())
            .unwrap();

        let latest = registry.resolve(&coords, &Label::Latest).unwrap();
        assert_eq!(latest.version, "1.1.0");

        let pulled = registry.pull(&coords, &Label::Latest).unwrap();
        assert_eq!(pulled, new_lock.as_bytes());
        // the pulled bytes are a valid lock
        let yaml = std::str::from_utf8(&pulled).expect("utf-8");
        let lock = LockFile::from_str_with_base_directory(yaml, None).expect("valid lock");
        assert!(lock.environment("default").is_some());

        std::fs::remove_dir_all(&root).ok();
    }
}
