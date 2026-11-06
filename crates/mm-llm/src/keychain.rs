//! Generic-password secrets in the macOS Keychain, shared by API keys and the HF token.
//!
//! The app was renamed from Meeting Master to Friston. Secrets saved under the old service
//! name move to the new one the first time they are read, so nobody has to re-enter a key.

/// Keychain service holding every Friston secret (account = what the secret is for).
pub const KEYCHAIN_SERVICE: &str = "Friston";

/// Service name used before the rename; read once for migration, then deleted.
pub const LEGACY_KEYCHAIN_SERVICE: &str = "Meeting Master";

/// Minimal secret store, so the migration logic can be tested without the real Keychain.
pub trait SecretStore {
    /// Reads `service`/`account`.
    ///
    /// Returns: the secret, or `keyring::Error::NoEntry`.
    fn get(&self, service: &str, account: &str) -> keyring::Result<String>;
    /// Writes `service`/`account`.
    ///
    /// Returns: `Ok(())` once stored.
    fn set(&self, service: &str, account: &str, secret: &str) -> keyring::Result<()>;
    /// Deletes `service`/`account`.
    ///
    /// Returns: `Ok(())`, or `keyring::Error::NoEntry` if absent.
    fn delete(&self, service: &str, account: &str) -> keyring::Result<()>;
}

/// The real macOS Keychain via `keyring`.
pub struct SystemKeychain;

impl SecretStore for SystemKeychain {
    fn get(&self, service: &str, account: &str) -> keyring::Result<String> {
        keyring::Entry::new(service, account)?.get_password()
    }

    fn set(&self, service: &str, account: &str, secret: &str) -> keyring::Result<()> {
        keyring::Entry::new(service, account)?.set_password(secret)
    }

    fn delete(&self, service: &str, account: &str) -> keyring::Result<()> {
        keyring::Entry::new(service, account)?.delete_credential()
    }
}

/// Treats a missing entry as success.
fn ignore_missing(r: keyring::Result<()>) -> keyring::Result<()> {
    match r {
        Err(keyring::Error::NoEntry) => Ok(()),
        other => other,
    }
}

/// Reads a secret, moving it from the legacy service if only that one has it.
///
/// Params: `store` – secret store; `account` – e.g. `openai` or `huggingface`.
/// Returns: the secret, or `None` when neither service has one.
pub fn read_secret(store: &impl SecretStore, account: &str) -> keyring::Result<Option<String>> {
    match store.get(KEYCHAIN_SERVICE, account) {
        Ok(s) => return Ok(Some(s)),
        Err(keyring::Error::NoEntry) => {}
        Err(e) => return Err(e),
    }
    match store.get(LEGACY_KEYCHAIN_SERVICE, account) {
        Ok(s) => {
            store.set(KEYCHAIN_SERVICE, account, &s)?;
            // Only drop the old copy once the new one is safely stored.
            ignore_missing(store.delete(LEGACY_KEYCHAIN_SERVICE, account))?;
            Ok(Some(s))
        }
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Stores a secret, or with `None` deletes it; any legacy copy is removed either way so it
/// cannot come back through [`read_secret`].
///
/// Params: `store`; `account`; `secret`.
/// Returns: `Ok(())` once the store accepted the change.
pub fn write_secret(
    store: &impl SecretStore,
    account: &str,
    secret: Option<&str>,
) -> keyring::Result<()> {
    match secret {
        Some(s) => store.set(KEYCHAIN_SERVICE, account, s)?,
        None => ignore_missing(store.delete(KEYCHAIN_SERVICE, account))?,
    }
    ignore_missing(store.delete(LEGACY_KEYCHAIN_SERVICE, account))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    #[derive(Default)]
    struct Fake(RefCell<HashMap<(String, String), String>>);

    impl SecretStore for Fake {
        fn get(&self, service: &str, account: &str) -> keyring::Result<String> {
            self.0
                .borrow()
                .get(&(service.into(), account.into()))
                .cloned()
                .ok_or(keyring::Error::NoEntry)
        }
        fn set(&self, service: &str, account: &str, secret: &str) -> keyring::Result<()> {
            self.0
                .borrow_mut()
                .insert((service.into(), account.into()), secret.into());
            Ok(())
        }
        fn delete(&self, service: &str, account: &str) -> keyring::Result<()> {
            self.0
                .borrow_mut()
                .remove(&(service.into(), account.into()))
                .map(|_| ())
                .ok_or(keyring::Error::NoEntry)
        }
    }

    #[test]
    fn legacy_secret_moves_to_the_new_service_on_first_read() {
        let store = Fake::default();
        store
            .set(LEGACY_KEYCHAIN_SERVICE, "openai", "sk-old")
            .unwrap();

        assert_eq!(
            read_secret(&store, "openai").unwrap().as_deref(),
            Some("sk-old")
        );
        assert_eq!(store.get(KEYCHAIN_SERVICE, "openai").unwrap(), "sk-old");
        assert!(matches!(
            store.get(LEGACY_KEYCHAIN_SERVICE, "openai"),
            Err(keyring::Error::NoEntry)
        ));
    }

    #[test]
    fn new_secret_wins_and_missing_reads_as_none() {
        let store = Fake::default();
        assert_eq!(read_secret(&store, "openai").unwrap(), None);
        store.set(KEYCHAIN_SERVICE, "openai", "sk-new").unwrap();
        store
            .set(LEGACY_KEYCHAIN_SERVICE, "openai", "sk-old")
            .unwrap();
        assert_eq!(
            read_secret(&store, "openai").unwrap().as_deref(),
            Some("sk-new")
        );
    }

    #[test]
    fn clearing_a_secret_also_drops_the_legacy_copy() {
        let store = Fake::default();
        store
            .set(LEGACY_KEYCHAIN_SERVICE, "huggingface", "hf_old")
            .unwrap();

        write_secret(&store, "huggingface", None).unwrap();

        assert_eq!(read_secret(&store, "huggingface").unwrap(), None);
    }
}
