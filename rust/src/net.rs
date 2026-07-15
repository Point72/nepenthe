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

/// Environment variable carrying channel credentials as a JSON map of host →
/// credential — the same shape as a `RATTLER_AUTH_FILE`, for setups that can
/// provide environment variables but not files (e.g. CI).
pub const CHANNEL_AUTH_ENV: &str = "NEPENTHE_CHANNEL_AUTH";

/// Build an HTTP client that authenticates conda-channel requests. Shared by
/// the repodata [gateway] and the package [installer] so both the solve and
/// install sides reach private channels.
///
/// [gateway]: rattler_repodata_gateway::Gateway
/// [installer]: rattler::install::Installer
pub fn authenticated_client() -> Result<ClientWithMiddleware, String> {
    let storage = auth_storage()?;
    let middleware = AuthenticationMiddleware::from_auth_storage(storage);
    Ok(ClientBuilder::new(reqwest::Client::new())
        .with(middleware)
        .build())
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
    use rattler_networking::authentication_storage::StorageBackend;
    use rattler_networking::Authentication;

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
