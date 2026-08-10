//! Authenticated HTTP client construction for conda-channel access.
//!
//! Some channels (e.g. an internal Artifactory repository) require credentials.
//! Those credentials must never live in a manifest or a lock — they are
//! supplied out of band and applied per-host at request time as an
//! `Authorization` header, so package URLs stay bare.
//!
//! Credentials come from two sources, both keyed by host:
//!
//! - **A file**, via rattler's authentication storage: the path named by
//!   `RATTLER_AUTH_FILE`, else `~/.rattler/credentials.json`.
//! - **An environment variable**, [`CHANNEL_AUTH_ENV`] (`NEPENTHE_CHANNEL_AUTH`),
//!   holding the same JSON as that file. This is for environments such as CI
//!   that can set environment variables but not files. When both define the
//!   same host, the environment variable wins.
//!
//! Either source is a JSON map of host → credential, e.g.
//!
//! ```json
//! { "artifacts.example.com": { "BasicHTTP": { "username": "u", "password": "t" } } }
//! ```
//!
//! `BearerToken` and `CondaToken` credentials are also supported. When no
//! credentials are configured the client still works; requests go out
//! unauthenticated, which is correct for public channels.

use std::collections::BTreeMap;
use std::sync::Arc;

use rattler_networking::authentication_storage::backends::memory::MemoryStorage;
use rattler_networking::authentication_storage::StorageBackend;
use rattler_networking::{Authentication, AuthenticationMiddleware, AuthenticationStorage};
use reqwest_middleware::{reqwest, ClientBuilder, ClientWithMiddleware};
use reqwest_retry::policies::ExponentialBackoff;
use reqwest_retry::RetryTransientMiddleware;

/// Environment variable carrying channel credentials as a JSON map of host →
/// credential — the same shape as a `RATTLER_AUTH_FILE`, for setups that can
/// provide environment variables but not files (e.g. CI).
pub const CHANNEL_AUTH_ENV: &str = "NEPENTHE_CHANNEL_AUTH";

/// How many times a transient HTTP failure is retried before giving up.
const RETRY_ATTEMPTS: u32 = 3;

/// Build an HTTP client that authenticates conda-channel requests. Shared by
/// the repodata [gateway] and the package [installer] so both the solve and
/// install sides reach private channels.
///
/// Requests are retried on transient failures. rattler's own retry only covers
/// connection and timeout errors — an HTTP status is not retryable there (its
/// `should_retry` ends in a catch-all `false`), so a 429 or a 502 from a busy
/// channel host would otherwise fail a package permanently on the first
/// response. That shows up as a handful of unrelated packages failing across a
/// wide CI matrix while the same install succeeds when run on its own.
///
/// [gateway]: rattler_repodata_gateway::Gateway
/// [installer]: rattler::install::Installer
pub fn authenticated_client() -> Result<ClientWithMiddleware, String> {
    let storage = auth_storage()?;
    let middleware = AuthenticationMiddleware::from_auth_storage(storage);
    Ok(ClientBuilder::new(reqwest::Client::new())
        .with(middleware)
        .with(RetryTransientMiddleware::new_with_policy(retry_policy()))
        .build())
}

/// Exponential backoff for transient HTTP failures, applied by
/// [`authenticated_client`].
///
/// Retries `RETRY_ATTEMPTS` times. The default policy classifies 5xx, 429 and
/// connection errors as transient and leaves 4xx alone, so a genuine 401 on a
/// private channel still fails immediately rather than stalling behind
/// backoff.
fn retry_policy() -> ExponentialBackoff {
    ExponentialBackoff::builder().build_with_max_retries(RETRY_ATTEMPTS)
}

/// Assemble the authentication storage: rattler's file/keyring/netrc defaults,
/// with credentials from [`CHANNEL_AUTH_ENV`] layered on top at highest
/// priority when the variable is set to a non-empty value.
fn auth_storage() -> Result<AuthenticationStorage, String> {
    let mut storage = AuthenticationStorage::from_env_and_defaults()
        .map_err(|e| format!("failed to initialize authentication storage: {e}"))?;

    if let Ok(value) = std::env::var(CHANNEL_AUTH_ENV) {
        if let Some(memory) = load_env_credentials(&value)? {
            // Insert at the front so env-var credentials take precedence over
            // any file or keyring entry for the same host.
            let backend: Arc<dyn StorageBackend + Send + Sync> = Arc::new(memory);
            storage.backends.insert(0, backend);
        }
    }

    Ok(storage)
}

/// Parse the [`CHANNEL_AUTH_ENV`] value (a JSON map of host → credential) into
/// an in-memory backend. Returns `None` when the value is empty — an unset CI
/// secret expands to an empty string, so an absent credential is not an error.
///
/// On a parse failure the error is deliberately generic (no parser detail), so
/// the credential material in the value cannot leak into logs.
fn load_env_credentials(value: &str) -> Result<Option<MemoryStorage>, String> {
    if value.trim().is_empty() {
        return Ok(None);
    }
    let credentials: BTreeMap<String, Authentication> = serde_json::from_str(value)
        .map_err(|_| format!("{CHANNEL_AUTH_ENV} is not a valid JSON map of host to credential"))?;
    let memory = MemoryStorage::new();
    for (host, authentication) in &credentials {
        memory
            .store(host, authentication)
            .map_err(|e| format!("failed to load {CHANNEL_AUTH_ENV} credential for {host}: {e}"))?;
    }
    Ok(Some(memory))
}

#[cfg(test)]
mod tests {
    use super::load_env_credentials;
    use super::{authenticated_client, retry_policy, RETRY_ATTEMPTS};
    use rattler_networking::authentication_storage::StorageBackend;
    use rattler_networking::Authentication;
    use reqwest_retry::policies::ExponentialBackoff;
    use reqwest_retry::{RetryDecision, RetryPolicy};

    #[test]
    fn client_builds_with_the_retry_layer() {
        // Construction is the contract: a missing/!Send middleware or a
        // mismatched reqwest-middleware version fails to build the client.
        assert!(authenticated_client().is_ok());
    }

    #[test]
    fn retry_policy_gives_up_after_the_configured_attempts() {
        let policy: ExponentialBackoff = retry_policy();
        let start = std::time::SystemTime::now();
        // Attempts before the limit are retried...
        assert!(matches!(
            policy.should_retry(start, RETRY_ATTEMPTS - 1),
            RetryDecision::Retry { .. }
        ));
        // ...and the policy stops rather than retrying forever.
        assert!(matches!(
            policy.should_retry(start, RETRY_ATTEMPTS),
            RetryDecision::DoNotRetry
        ));
    }

    /// A 503 is retried until it succeeds. This is the case rattler's own retry
    /// does not cover — it treats any HTTP status as terminal — so without the
    /// middleware the first response would fail the request outright.
    #[tokio::test]
    async fn transient_http_status_is_retried() {
        use std::io::{Read as _, Write as _};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let served = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&served);

        std::thread::spawn(move || {
            for stream in listener.incoming().take(3) {
                let mut stream = stream.unwrap();
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                let n = counter.fetch_add(1, Ordering::SeqCst);
                // Fail twice, then succeed.
                let response = if n < 2 {
                    "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                } else {
                    "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok"
                };
                let _ = stream.write_all(response.as_bytes());
            }
        });

        let client = authenticated_client().unwrap();
        let response = client
            .get(format!("http://{addr}/pkg.conda"))
            .send()
            .await
            .expect("request should succeed after retries");

        assert_eq!(response.status(), 200);
        assert_eq!(served.load(Ordering::SeqCst), 3, "expected two retries");
    }

    #[test]
    fn channel_auth_env_loads_a_basic_credential() {
        let json = r#"{"artifacts.example.com":{"BasicHTTP":{"username":"u","password":"t"}}}"#;
        let memory = load_env_credentials(json)
            .unwrap()
            .expect("credentials present");
        assert_eq!(
            memory.get("artifacts.example.com").unwrap(),
            Some(Authentication::BasicHTTP {
                username: "u".to_string(),
                password: "t".to_string(),
            })
        );
        // a host with no entry yields nothing
        assert_eq!(memory.get("other.example.com").unwrap(), None);
    }

    #[test]
    fn channel_auth_env_loads_a_bearer_token() {
        let json = r#"{"h.example.com":{"BearerToken":"abc"}}"#;
        let memory = load_env_credentials(json).unwrap().unwrap();
        assert_eq!(
            memory.get("h.example.com").unwrap(),
            Some(Authentication::BearerToken("abc".to_string()))
        );
    }

    #[test]
    fn channel_auth_env_empty_is_ignored() {
        assert!(load_env_credentials("").unwrap().is_none());
        assert!(load_env_credentials("   \n").unwrap().is_none());
    }

    #[test]
    fn channel_auth_env_rejects_malformed_json() {
        assert!(load_env_credentials("not a map").is_err());
    }
}
