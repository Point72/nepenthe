//! Spec storage backends.
//!
//! nepenthe reads manifests and override layers, and writes locks, through the
//! [`fsspec_rs`] `FileSystem` trait — the mechanism for backend connects. A
//! [`SpecStore`] dispatches `get`/`put` to a concrete backend chosen by the URL
//! scheme (`file://` local, `s3://`), pulling any credentials from an
//! [`AuthStore`] rather than from the artifact itself. fsspec_rs ships the
//! pure-Rust local and S3 backends; an [`ArtifactoryFs`] HTTP backend and a
//! Python-`fsspec` bridge (in the pyo3 layer) build on the same trait.

use std::collections::BTreeMap;
use std::fmt;

use fsspec_rs::{
    FileInfo, FileSystem, FsError, FsFile, FsResult, LocalFs, OpenMode, OpenOptions, S3Config, S3Fs,
};
use url::Url;

pub use fsspec_rs;

/// Errors raised by the spec-store layer.
#[derive(Debug)]
pub enum BackendError {
    /// The spec URL could not be parsed.
    InvalidUrl(String),
    /// No backend is wired for the URL scheme.
    UnsupportedScheme(String),
    /// A cleartext `http://` URL would send configured credentials in the
    /// clear or read bytes over an unauthenticated channel; rejected for any
    /// non-loopback host.
    InsecureScheme(String),
    /// The underlying filesystem operation failed.
    Fs(FsError),
}

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BackendError::InvalidUrl(msg) => write!(f, "invalid spec url: {msg}"),
            BackendError::UnsupportedScheme(s) => {
                write!(f, "unsupported spec backend scheme '{s}'")
            }
            BackendError::InsecureScheme(url) => {
                write!(
                    f,
                    "refusing to use cleartext http for a non-loopback host: {url}"
                )
            }
            BackendError::Fs(e) => write!(f, "spec backend i/o failed: {e}"),
        }
    }
}

impl std::error::Error for BackendError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            BackendError::Fs(e) => Some(e),
            _ => None,
        }
    }
}

impl From<FsError> for BackendError {
    fn from(e: FsError) -> Self {
        BackendError::Fs(e)
    }
}

/// A credential set for one backend host. Secrets live here — never in a
/// manifest, override, or lock.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Credential {
    /// Username, or an S3 access-key id.
    pub username: Option<String>,
    /// Password, or an S3 secret-access key.
    pub password: Option<String>,
    /// Bearer token, or an S3 session token.
    pub token: Option<String>,
    /// Region (S3); not a secret, carried here for convenience.
    pub region: Option<String>,
}

impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never print secret material; show only whether each field is set.
        let redact = |o: &Option<String>| o.as_ref().map(|_| "***");
        f.debug_struct("Credential")
            .field("username", &redact(&self.username))
            .field("password", &redact(&self.password))
            .field("token", &redact(&self.token))
            .field("region", &self.region)
            .finish()
    }
}

impl Credential {
    /// A username/password (or access-key/secret) pair.
    pub fn basic(username: impl Into<String>, password: impl Into<String>) -> Self {
        Self {
            username: Some(username.into()),
            password: Some(password.into()),
            ..Default::default()
        }
    }

    /// A bearer token.
    pub fn bearer(token: impl Into<String>) -> Self {
        Self {
            token: Some(token.into()),
            ..Default::default()
        }
    }
}

/// Maps a backend host to its [`Credential`]. Credentials are supplied
/// explicitly or read from the ambient environment; they are applied when a
/// backend is constructed and never written into an artifact.
#[derive(Clone, Debug, Default)]
pub struct AuthStore {
    creds: BTreeMap<String, Credential>,
}

impl AuthStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a credential for `host` (an S3 bucket or an HTTPS host).
    pub fn set(&mut self, host: impl Into<String>, cred: Credential) {
        self.creds.insert(host.into(), cred);
    }

    /// The credential recorded for `host`, if any.
    pub fn get(&self, host: &str) -> Option<&Credential> {
        self.creds.get(host)
    }

    /// Build an [`S3Config`] for `bucket`: explicit credentials win, otherwise
    /// the region is read from `AWS_REGION` / `AWS_DEFAULT_REGION` and the
    /// access keys are left unset so `object_store` resolves them from the
    /// ambient AWS credential chain (env vars, profile, instance metadata).
    pub fn s3_config(&self, bucket: &str) -> S3Config {
        let mut cfg = S3Config::new(bucket);
        cfg.region = std::env::var("AWS_REGION")
            .or_else(|_| std::env::var("AWS_DEFAULT_REGION"))
            .ok();
        // Standard override for S3-compatible stores (MinIO, LocalStack, an
        // internal object store). Path-style is the default for such endpoints.
        cfg.endpoint_url = std::env::var("AWS_ENDPOINT_URL").ok();
        if let Some(c) = self.creds.get(bucket) {
            if c.username.is_some() {
                cfg.access_key_id = c.username.clone();
            }
            if c.password.is_some() {
                cfg.secret_access_key = c.password.clone();
            }
            if c.token.is_some() {
                cfg.session_token = c.token.clone();
            }
            if c.region.is_some() {
                cfg.region = c.region.clone();
            }
        }
        cfg
    }
}

/// Replace any credentials embedded in a URL's userinfo with `***`, for safe
/// logging. Falls back to a string redaction when the URL does not fully parse,
/// so an invalid URL can never leak a secret through an error message.
pub fn mask_url(url: &str) -> String {
    if let Ok(mut u) = Url::parse(url) {
        let had_user = !u.username().is_empty();
        let had_pass = u.password().is_some();
        if had_pass {
            let _ = u.set_password(Some("***"));
        }
        if had_user {
            let _ = u.set_username("***");
        }
        if u.query().is_some() {
            let redacted: Vec<(String, String)> = u
                .query_pairs()
                .map(|(k, v)| {
                    if is_sensitive_query_key(&k) {
                        (k.into_owned(), "***".to_string())
                    } else {
                        (k.into_owned(), v.into_owned())
                    }
                })
                .collect();
            u.query_pairs_mut().clear().extend_pairs(redacted);
        }
        return u.to_string();
    }
    mask_userinfo_fallback(url)
}

/// Query-parameter keys whose values commonly carry secrets in signed URLs
/// (AWS SigV4, Azure SAS, bearer tokens). Matched case-insensitively as
/// substrings, biased toward over-redaction for safety in error messages.
fn is_sensitive_query_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    [
        "sig",
        "token",
        "secret",
        "password",
        "passwd",
        "pwd",
        "credential",
        "key",
        "auth",
    ]
    .iter()
    .any(|needle| key.contains(needle))
}

/// Redact `scheme://userinfo@host...` userinfo by string surgery, for URLs that
/// `Url::parse` rejects (e.g. a missing host). Strings without userinfo are
/// returned unchanged.
fn mask_userinfo_fallback(url: &str) -> String {
    if let Some(scheme_end) = url.find("://") {
        let after = scheme_end + 3;
        let rest = &url[after..];
        let at = rest.find('@');
        let slash = rest.find('/');
        if let Some(at_idx) = at {
            if slash.is_none_or(|s| at_idx < s) {
                return format!("{}***@{}", &url[..after], &rest[at_idx + 1..]);
            }
        }
    }
    url.to_string()
}

/// A spec backend over HTTP(S), suitable for an Artifactory generic repository.
///
/// Reads are HTTP `GET`, writes HTTP `PUT`, deletes HTTP `DELETE`, against
/// `base` + the request path. Credentials come from an [`AuthStore`] entry for
/// the host and are sent as a bearer token (if set) or HTTP basic auth — never
/// embedded in the request URL. TLS uses the system trust store (rustls +
/// native roots), so an internal CA is honoured.
pub struct ArtifactoryFs {
    base: String,
    cred: Option<Credential>,
    client: reqwest::blocking::Client,
}

impl ArtifactoryFs {
    /// Build a backend rooted at `base` (e.g. `https://artifacts.example.com`).
    pub fn new(base: impl Into<String>, cred: Option<Credential>) -> Result<Self, FsError> {
        let client = reqwest::blocking::Client::builder()
            .user_agent(concat!("nepenthe/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| FsError::Other(format!("http client: {e}")))?;
        Ok(Self {
            base: base.into().trim_end_matches('/').to_string(),
            cred,
            client,
        })
    }

    fn url(&self, path: &str) -> String {
        if path.starts_with('/') {
            format!("{}{path}", self.base)
        } else {
            format!("{}/{path}", self.base)
        }
    }

    fn authed(&self, rb: reqwest::blocking::RequestBuilder) -> reqwest::blocking::RequestBuilder {
        match &self.cred {
            Some(Credential {
                token: Some(token), ..
            }) => rb.bearer_auth(token),
            Some(Credential {
                username: Some(user),
                password,
                ..
            }) => rb.basic_auth(user, password.as_ref()),
            _ => rb,
        }
    }
}

/// Map notable HTTP statuses to descriptive [`FsError`]s, masking the URL.
fn check_status(resp: &reqwest::blocking::Response, url: &str) -> FsResult<()> {
    match resp.status().as_u16() {
        404 => Err(FsError::NotFound(mask_url(url))),
        401 | 403 => Err(FsError::PermissionDenied(mask_url(url))),
        _ => Ok(()),
    }
}

/// Format an HTTP transport error without leaking the (possibly signed) request
/// URL: the URL is stripped from the `reqwest` error and a separately masked
/// URL is appended instead.
fn http_err(verb: &str, url: &str, e: reqwest::Error) -> FsError {
    FsError::Other(format!(
        "{verb} {} failed: {}",
        mask_url(url),
        e.without_url()
    ))
}

/// Whether `host` is a loopback address or `localhost`, the only hosts for
/// which cleartext http is permitted.
fn is_loopback_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.parse::<std::net::IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
}

impl FileSystem for ArtifactoryFs {
    fn protocol(&self) -> &[&str] {
        &["https", "http"]
    }

    fn cat_file(&self, path: &str, start: Option<i64>, end: Option<i64>) -> FsResult<Vec<u8>> {
        if start.is_some() || end.is_some() {
            return Err(FsError::NotSupported(
                "ranged reads are not supported by the http backend".into(),
            ));
        }
        let url = self.url(path);
        let resp = self
            .authed(self.client.get(&url))
            .send()
            .map_err(|e| http_err("GET", &url, e))?;
        check_status(&resp, &url)?;
        let resp = resp
            .error_for_status()
            .map_err(|e| http_err("GET", &url, e))?;
        resp.bytes()
            .map(|b| b.to_vec())
            .map_err(|e| http_err("GET body of", &url, e))
    }

    fn pipe_file(&self, path: &str, data: &[u8]) -> FsResult<()> {
        let url = self.url(path);
        let resp = self
            .authed(self.client.put(&url))
            .body(data.to_vec())
            .send()
            .map_err(|e| http_err("PUT", &url, e))?;
        check_status(&resp, &url)?;
        resp.error_for_status()
            .map_err(|e| http_err("PUT", &url, e))?;
        Ok(())
    }

    fn info(&self, path: &str) -> FsResult<FileInfo> {
        let url = self.url(path);
        let resp = self
            .authed(self.client.head(&url))
            .send()
            .map_err(|e| http_err("HEAD", &url, e))?;
        check_status(&resp, &url)?;
        let resp = resp
            .error_for_status()
            .map_err(|e| http_err("HEAD", &url, e))?;
        let size = resp
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        Ok(FileInfo::file(path, size))
    }

    fn rm_file(&self, path: &str) -> FsResult<()> {
        let url = self.url(path);
        let resp = self
            .authed(self.client.delete(&url))
            .send()
            .map_err(|e| http_err("DELETE", &url, e))?;
        check_status(&resp, &url)?;
        resp.error_for_status()
            .map_err(|e| http_err("DELETE", &url, e))?;
        Ok(())
    }

    fn cp_file(&self, _src: &str, _dst: &str) -> FsResult<()> {
        Err(FsError::NotSupported("cp_file".into()))
    }

    fn open(
        &self,
        _path: &str,
        _mode: OpenMode,
        _opts: Option<OpenOptions>,
    ) -> FsResult<Box<dyn FsFile>> {
        Err(FsError::NotSupported(
            "streaming open is not supported by the http backend; use cat_file/pipe_file".into(),
        ))
    }

    fn ls(&self, _path: &str, _detail: bool) -> FsResult<Vec<FileInfo>> {
        Err(FsError::NotSupported("ls".into()))
    }

    fn mkdir(&self, _path: &str, _create_parents: bool) -> FsResult<()> {
        Ok(())
    }

    fn rmdir(&self, _path: &str) -> FsResult<()> {
        Ok(())
    }
}

/// A concrete backend selected for one spec URL.
enum Backend {
    Local(LocalFs),
    // `S3Fs` embeds a tokio runtime, so box it to keep the variants balanced.
    S3(Box<S3Fs>),
    Artifactory(Box<ArtifactoryFs>),
}

impl Backend {
    fn cat(&self, path: &str) -> Result<Vec<u8>, FsError> {
        match self {
            Backend::Local(fs) => fs.cat_file(path, None, None),
            Backend::S3(fs) => fs.cat_file(path, None, None),
            Backend::Artifactory(fs) => fs.cat_file(path, None, None),
        }
    }

    fn pipe(&self, path: &str, data: &[u8]) -> Result<(), FsError> {
        match self {
            Backend::Local(fs) => fs.pipe_file(path, data),
            Backend::S3(fs) => fs.pipe_file(path, data),
            Backend::Artifactory(fs) => fs.pipe_file(path, data),
        }
    }
}

/// Reads manifests/overrides and writes locks over a `FileSystem` backend
/// chosen by URL scheme, taking credentials from an [`AuthStore`].
#[derive(Clone, Debug, Default)]
pub struct SpecStore {
    auth: AuthStore,
}

impl SpecStore {
    /// A store with no recorded credentials (the ambient environment still
    /// supplies AWS credentials for S3).
    pub fn new() -> Self {
        Self::default()
    }

    /// A store backed by the given [`AuthStore`].
    pub fn with_auth(auth: AuthStore) -> Self {
        Self { auth }
    }

    /// The credential store backing this spec store.
    pub fn auth(&self) -> &AuthStore {
        &self.auth
    }

    /// Pull the bytes of a spec (manifest, override layer, or lock) from `url`.
    pub fn get(&self, url: &str) -> Result<Vec<u8>, BackendError> {
        let (backend, path) = self.resolve(url)?;
        Ok(backend.cat(&path)?)
    }

    /// Publish the bytes of a spec (typically a lock) to `url`.
    pub fn put(&self, url: &str, data: &[u8]) -> Result<(), BackendError> {
        let (backend, path) = self.resolve(url)?;
        backend.pipe(&path, data)?;
        Ok(())
    }

    /// Select a backend and the in-backend path for `url`.
    fn resolve(&self, url: &str) -> Result<(Backend, String), BackendError> {
        let parsed = Url::parse(url)
            .map_err(|e| BackendError::InvalidUrl(format!("{}: {e}", mask_url(url))))?;
        match parsed.scheme() {
            "file" => {
                let path = parsed.to_file_path().map_err(|()| {
                    BackendError::InvalidUrl(format!("unsupported file url: {}", mask_url(url)))
                })?;
                // Auto-mkdir so publishing a spec creates its parent path,
                // matching the implicit-directory semantics of S3 and HTTP.
                Ok((
                    Backend::Local(LocalFs::with_auto_mkdir(true)),
                    path.to_string_lossy().into_owned(),
                ))
            }
            "s3" => {
                let bucket = parsed.host_str().ok_or_else(|| {
                    BackendError::InvalidUrl(format!("s3 url missing bucket: {}", mask_url(url)))
                })?;
                let key = parsed.path().trim_start_matches('/').to_string();
                let fs = S3Fs::new(self.auth.s3_config(bucket))?;
                Ok((Backend::S3(Box::new(fs)), key))
            }
            "http" | "https" => {
                let host = parsed.host_str().ok_or_else(|| {
                    BackendError::InvalidUrl(format!("http url missing host: {}", mask_url(url)))
                })?;
                let cred = self.auth.get(host).cloned();
                // Reject cleartext HTTP for non-loopback hosts: it would expose
                // credentials and let manifests/locks be tampered in transit.
                if parsed.scheme() == "http" && !is_loopback_host(host) {
                    return Err(BackendError::InsecureScheme(mask_url(url)));
                }
                let mut base = format!("{}://{host}", parsed.scheme());
                if let Some(port) = parsed.port() {
                    base.push_str(&format!(":{port}"));
                }
                let fs = ArtifactoryFs::new(base, cred)?;
                // Preserve any query string so signed/versioned URLs work.
                let mut path = parsed.path().to_string();
                if let Some(query) = parsed.query() {
                    path.push('?');
                    path.push_str(query);
                }
                Ok((Backend::Artifactory(Box::new(fs)), path))
            }
            other => Err(BackendError::UnsupportedScheme(other.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = "project:\n  name: demo\n  channels: [conda-forge]\n  platforms: [linux-64]\nenvironments:\n  app: []\n";

    fn temp_url(suffix: &str) -> (std::path::PathBuf, String) {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "nepenthe-backend-{}-{}",
            std::process::id(),
            suffix
        ));
        let url = format!("file://{}", path.to_str().expect("utf-8 temp path"));
        (path, url)
    }

    #[test]
    fn mask_url_redacts_userinfo() {
        assert_eq!(
            mask_url("https://alice:s3cr3t@host.example/specs/x.yaml"),
            "https://***:***@host.example/specs/x.yaml"
        );
        assert_eq!(
            mask_url("https://token@host.example/x"),
            "https://***@host.example/x"
        );
        // no userinfo and non-URL strings pass through untouched
        assert_eq!(
            mask_url("s3://team-bucket/envs/overrides.yaml"),
            "s3://team-bucket/envs/overrides.yaml"
        );
        assert_eq!(mask_url("not a url"), "not a url");
        // userinfo is redacted even when the URL does not fully parse
        assert_eq!(mask_url("s3://alice:hunter2@"), "s3://***@");
    }

    #[test]
    fn mask_url_redacts_sensitive_query_params() {
        let masked = mask_url(
            "https://host.example/o/x.tar?X-Amz-Signature=deadbeef&X-Amz-Credential=AKIA123&v=2",
        );
        assert!(!masked.contains("deadbeef"), "leaked signature: {masked}");
        assert!(!masked.contains("AKIA123"), "leaked credential: {masked}");
        // benign params are preserved
        assert!(masked.contains("v=2"), "dropped benign param: {masked}");
    }

    #[test]
    fn file_url_with_spaces_is_decoded() {
        let store = SpecStore::new();
        let dir = std::env::temp_dir().join(format!(
            "nepenthe-backend-{}-with space",
            std::process::id()
        ));
        let url = format!("file://{}/spec.yaml", dir.to_str().expect("utf-8")).replace(' ', "%20");
        store.put(&url, b"payload").expect("put");
        assert_eq!(store.get(&url).expect("get"), b"payload");
        // the on-disk directory has a real space, not a literal %20
        assert!(dir.join("spec.yaml").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn auth_store_set_get_and_s3_config() {
        let mut auth = AuthStore::new();
        auth.set(
            "team-bucket",
            Credential {
                username: Some("AKIAEXAMPLE".into()),
                password: Some("secret".into()),
                token: Some("session".into()),
                region: Some("us-west-2".into()),
            },
        );
        assert_eq!(
            auth.get("team-bucket").unwrap().username.as_deref(),
            Some("AKIAEXAMPLE")
        );
        assert!(auth.get("other").is_none());

        let cfg = auth.s3_config("team-bucket");
        assert_eq!(cfg.bucket, "team-bucket");
        assert_eq!(cfg.access_key_id.as_deref(), Some("AKIAEXAMPLE"));
        assert_eq!(cfg.secret_access_key.as_deref(), Some("secret"));
        assert_eq!(cfg.session_token.as_deref(), Some("session"));
        assert_eq!(cfg.region.as_deref(), Some("us-west-2"));
    }

    #[test]
    fn local_spec_store_round_trips_bytes() {
        let store = SpecStore::new();
        let (path, url) = temp_url("bytes.bin");
        let payload = b"\x00nepenthe spec\xff bytes";

        store.put(&url, payload).expect("put should succeed");
        let got = store.get(&url).expect("get should succeed");
        assert_eq!(got, payload);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn local_spec_store_round_trips_a_manifest() {
        let store = SpecStore::new();
        let (path, url) = temp_url("manifest.yaml");

        store
            .put(&url, MANIFEST.as_bytes())
            .expect("publishing a manifest should succeed");
        let got = store.get(&url).expect("pulling a manifest should succeed");

        let yaml = std::str::from_utf8(&got).expect("utf-8");
        let manifest = crate::manifest::Manifest::from_yaml_str(yaml).expect("parses");
        assert_eq!(manifest.project.name, "demo");

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn unsupported_scheme_is_reported() {
        let store = SpecStore::new();
        let err = store.get("ftp://host.example/x.yaml").unwrap_err();
        assert!(matches!(err, BackendError::UnsupportedScheme(s) if s == "ftp"));
    }

    #[test]
    fn invalid_url_error_masks_credentials() {
        let store = SpecStore::new();
        // `s3://user:pass@` with no bucket host is rejected; the error must not
        // leak the embedded password.
        let err = store.get("s3://alice:hunter2@").unwrap_err();
        let rendered = err.to_string();
        assert!(
            !rendered.contains("hunter2"),
            "error leaked a secret: {rendered}"
        );
    }

    #[test]
    fn credential_debug_is_redacted() {
        let c = Credential {
            username: Some("AKIAEXAMPLE".into()),
            password: Some("hunter2".into()),
            token: Some("session".into()),
            region: Some("us-west-2".into()),
        };
        let shown = format!("{c:?}");
        assert!(!shown.contains("hunter2"), "leaked password: {shown}");
        assert!(!shown.contains("AKIAEXAMPLE"), "leaked username: {shown}");
        assert!(!shown.contains("session"), "leaked token: {shown}");
        assert!(shown.contains("***"));
        // region is not a secret and may be shown
        assert!(shown.contains("us-west-2"));
    }

    #[test]
    fn cleartext_http_rejected_for_non_loopback() {
        let mut auth = AuthStore::new();
        auth.set("host.example", Credential::bearer("tok"));
        let store = SpecStore::with_auth(auth);
        // cleartext http to a non-loopback host is refused, with or without creds
        assert!(matches!(
            store.resolve("http://host.example/specs/x.yaml"),
            Err(BackendError::InsecureScheme(_))
        ));
        assert!(matches!(
            SpecStore::new().resolve("http://host.example/specs/x.yaml"),
            Err(BackendError::InsecureScheme(_))
        ));
        // https is fine, and cleartext http to loopback is allowed (dev/test)
        assert!(store.resolve("https://host.example/specs/x.yaml").is_ok());
        assert!(SpecStore::new().resolve("http://localhost/x.yaml").is_ok());
        assert!(SpecStore::new().resolve("http://127.0.0.1/x.yaml").is_ok());
    }

    #[test]
    fn http_query_string_is_preserved() {
        let store = SpecStore::new();
        let (_backend, path) = store
            .resolve("http://localhost/o/x.yaml?sig=abc&v=2")
            .expect("resolves");
        assert_eq!(path, "/o/x.yaml?sig=abc&v=2");
    }

    /// Round-trip a spec through a real S3 bucket. Ignored by default so CI
    /// stays offline; run with `NEPENTHE_TEST_S3_URL=s3://bucket/key cargo test
    /// -- --ignored`. AWS credentials come from the ambient environment.
    #[ignore = "requires network access and a writable S3 bucket"]
    #[test]
    fn real_s3_spec_store_round_trips() {
        let url = std::env::var("NEPENTHE_TEST_S3_URL")
            .expect("set NEPENTHE_TEST_S3_URL to an s3://bucket/key location");
        let store = SpecStore::new();

        store
            .put(&url, MANIFEST.as_bytes())
            .expect("publishing to S3 should succeed");
        let got = store.get(&url).expect("pulling from S3 should succeed");
        assert_eq!(got, MANIFEST.as_bytes());
    }

    /// Publish a spec to a real Artifactory generic repo and pull it back.
    /// Ignored by default so CI stays offline; run with the location and
    /// credentials supplied via env:
    /// `NEPENTHE_TEST_ARTIFACTORY_URL=https://host/artifactory/repo/x.yaml`,
    /// `NEPENTHE_TEST_ARTIFACTORY_USER`, `NEPENTHE_TEST_ARTIFACTORY_TOKEN`.
    #[ignore = "requires network access and Artifactory credentials"]
    #[test]
    fn real_artifactory_round_trips() {
        let url = std::env::var("NEPENTHE_TEST_ARTIFACTORY_URL")
            .expect("set NEPENTHE_TEST_ARTIFACTORY_URL to an https://.../file location");
        let mut auth = AuthStore::new();
        if let (Ok(user), Ok(token)) = (
            std::env::var("NEPENTHE_TEST_ARTIFACTORY_USER"),
            std::env::var("NEPENTHE_TEST_ARTIFACTORY_TOKEN"),
        ) {
            let host = Url::parse(&url)
                .ok()
                .and_then(|u| u.host_str().map(String::from))
                .expect("url has a host");
            auth.set(host, Credential::basic(user, token));
        }
        let store = SpecStore::with_auth(auth);

        store
            .put(&url, MANIFEST.as_bytes())
            .expect("publishing to Artifactory should succeed");
        let got = store
            .get(&url)
            .expect("pulling from Artifactory should succeed");
        assert_eq!(got, MANIFEST.as_bytes());
    }
}
