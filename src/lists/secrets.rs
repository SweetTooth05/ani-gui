//! Where account tokens live: the OS keychain (Secret Service on Linux,
//! the Keychain on macOS), never the database or a file. Without a working
//! keychain, connecting fails with an explanation instead of falling back
//! to plaintext.

use std::fmt;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

use super::Service;

/// The keychain "service" every ani-gui entry is filed under.
const KEYRING_SERVICE: &str = "ani-gui";

/// A service's tokens as stored (JSON in one keychain entry).
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Tokens {
    pub access: String,
    /// MAL only.
    #[serde(default)]
    pub refresh: Option<String>,
    /// Unix seconds the access token expires, when known.
    #[serde(default)]
    pub expires_at: Option<i64>,
}

/// Never prints the tokens themselves.
impl fmt::Debug for Tokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tokens")
            .field("access", &"<redacted>")
            .field("refresh", &self.refresh.as_ref().map(|_| "<redacted>"))
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

pub trait TokenStore: Send + Sync {
    /// None when nothing is stored for `service`.
    fn get(&self, service: Service) -> Result<Option<Tokens>>;
    fn set(&self, service: Service, tokens: &Tokens) -> Result<()>;
    /// Fine when nothing is stored.
    fn delete(&self, service: Service) -> Result<()>;
}

/// The OS keychain via the `keyring` crate.
pub struct Keyring;

fn entry(service: Service) -> Result<keyring::Entry> {
    keyring::Entry::new(KEYRING_SERVICE, service.as_str()).map_err(explain)
}

/// A keyring error a user can act on. Keyring errors never contain the
/// secret.
fn explain(err: keyring::Error) -> anyhow::Error {
    match err {
        keyring::Error::NoStorageAccess(inner) | keyring::Error::PlatformFailure(inner) => anyhow!(
            "Couldn't use your system keychain ({inner}). ani-gui keeps sign-ins only there, \
             so make sure a keyring such as GNOME Keyring or KWallet is running and unlocked, \
             then try again."
        ),
        other => anyhow!("Keychain error: {other}"),
    }
}

impl TokenStore for Keyring {
    fn get(&self, service: Service) -> Result<Option<Tokens>> {
        match entry(service)?.get_password() {
            Ok(json) => serde_json::from_str(&json)
                .map(Some)
                .map_err(|_| anyhow!("The saved {service} sign-in is unreadable")),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(err) => Err(explain(err)),
        }
    }

    fn set(&self, service: Service, tokens: &Tokens) -> Result<()> {
        let json = serde_json::to_string(tokens).context("encoding tokens")?;
        entry(service)?.set_password(&json).map_err(explain)
    }

    fn delete(&self, service: Service) -> Result<()> {
        match entry(service)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(err) => Err(explain(err)),
        }
    }
}

/// An in-memory store for tests; `broken` behaves like a missing keyring.
#[cfg(test)]
#[derive(Default)]
pub struct MemoryStore {
    pub tokens: std::sync::Mutex<std::collections::HashMap<Service, Tokens>>,
    pub broken: bool,
}

#[cfg(test)]
impl TokenStore for MemoryStore {
    fn get(&self, service: Service) -> Result<Option<Tokens>> {
        if self.broken {
            anyhow::bail!("no keyring");
        }
        Ok(self.tokens.lock().unwrap().get(&service).cloned())
    }

    fn set(&self, service: Service, tokens: &Tokens) -> Result<()> {
        if self.broken {
            anyhow::bail!("Couldn't use your system keychain (no keyring)");
        }
        self.tokens.lock().unwrap().insert(service, tokens.clone());
        Ok(())
    }

    fn delete(&self, service: Service) -> Result<()> {
        self.tokens.lock().unwrap().remove(&service);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_shows_tokens() {
        let t = Tokens {
            access: "SECRET-ACCESS".into(),
            refresh: Some("SECRET-REFRESH".into()),
            expires_at: Some(5),
        };
        let shown = format!("{t:?}");
        assert!(!shown.contains("SECRET"), "{shown}");
        assert!(shown.contains("expires_at: Some(5)"));
    }

    #[test]
    fn tokens_json_roundtrip_and_defaults() {
        let t = Tokens {
            access: "a".into(),
            refresh: None,
            expires_at: None,
        };
        let json = serde_json::to_string(&t).unwrap();
        assert_eq!(serde_json::from_str::<Tokens>(&json).unwrap(), t);
        assert_eq!(
            serde_json::from_str::<Tokens>(r#"{"access":"a"}"#).unwrap(),
            t
        );
    }
}
