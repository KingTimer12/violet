//! Saved passwords in one file, sealed with a key of their own.
//!
//! `vault.json` holds each connection's password sealed with
//! XChaCha20-Poly1305 under the vault key (the DEK), with the connection's
//! id and target as associated data: a sealed password copied to another
//! connection, or left behind when its connection moves to another server,
//! does not open.
//!
//! The vault key itself is kept one of two ways:
//!
//! - in the platform keychain, as one entry (one keychain access per run,
//!   however many connections there are); or
//! - wrapped by a key derived from a master password with Argon2id. The
//!   vault then starts locked and opens with the password.
//!
//! While open, the key sits in a page locked into memory (`mlock`,
//! `VirtualLock`) so it is never swapped to disk, and is cleared when the
//! vault is dropped.
//!
//! Passwords saved before the vault, one keychain entry per connection,
//! move into it the first time they are read.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use chacha20poly1305::aead::AeadInOut as _;
use chacha20poly1305::{Key, KeyInit as _, XChaCha20Poly1305, XNonce};
use serde::{Deserialize, Serialize};
use violet_core::ports::{SavedSecret, SecretStore};
use violet_core::{ConnectionId, DbError, ErrorKind, Result, Secret, SecretTarget};
use zeroize::{Zeroize as _, Zeroizing};

use crate::json::{JsonFile, storage_error};

const VERSION: u32 = 1;
const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 24;
const SALT_LEN: usize = 16;
const TAG_LEN: usize = 16;

/// The keychain entry holding the vault key, in keychain mode.
const KEY_SERVICE: &str = "dev.violet.vault";
const KEY_USER: &str = "key";

/// Associated data of the wrapped vault key.
const WRAP_AAD: &[u8] = b"violet-vault-key:v1";

/// Argon2id settings for the master password, and the bounds a vault file
/// is held to (it could have been edited to stall Violet at start).
const KDF: (u32, u32, u32) = (64 * 1024, 3, 1);
const M_COST: std::ops::RangeInclusive<u32> = 19_456..=1_048_576;
const T_COST: std::ops::RangeInclusive<u32> = 2..=16;
const P_COST: std::ops::RangeInclusive<u32> = 1..=16;

/// The shortest master password accepted.
pub const MIN_MASTER_PASSWORD: usize = 12;

/// Where the vault key is kept.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "kebab-case")]
enum KeySource {
    Keychain,
    Password {
        m_cost: u32,
        t_cost: u32,
        p_cost: u32,
        salt: String,
        nonce: String,
        /// The vault key sealed with the password's key, then its tag.
        wrapped: String,
    },
}

/// A sealed password and the target it was saved for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Sealed {
    target: SecretTarget,
    nonce: String,
    /// The password sealed, then its tag.
    sealed: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct VaultFile {
    v: u32,
    key: KeySource,
    #[serde(default)]
    entries: BTreeMap<ConnectionId, Sealed>,
}

/// The vault key, in a page kept out of swap while it lives.
struct VaultKey {
    bytes: Box<[u8; KEY_LEN]>,
    /// Unlocks the page when dropped, after [`Drop`] cleared the key.
    _lock: Option<region::LockGuard>,
}

impl VaultKey {
    /// A key made by `fill`, written straight into its locked page so no
    /// copy is left elsewhere.
    fn new(fill: impl FnOnce(&mut [u8; KEY_LEN]) -> Result<()>) -> Result<Self> {
        let mut bytes = Box::new([0u8; KEY_LEN]);
        let lock = match region::lock(bytes.as_ptr(), KEY_LEN) {
            Ok(guard) => Some(guard),
            Err(error) => {
                // Over the process's lock limit, say: the key still works,
                // it may only be swapped out.
                log::warn!("the vault key could not be locked in memory: {error}");
                None
            }
        };
        fill(&mut bytes)?;
        Ok(Self { bytes, _lock: lock })
    }

    fn generate() -> Result<Self> {
        Self::new(|bytes| random(bytes))
    }

    fn cipher(&self) -> XChaCha20Poly1305 {
        let key: &Key = (&*self.bytes).into();
        XChaCha20Poly1305::new(key)
    }
}

impl Drop for VaultKey {
    fn drop(&mut self) {
        self.bytes.zeroize();
    }
}

/// Where keychain mode keeps the vault key. Tests use memory.
pub enum KeySlot {
    Keychain,
    Memory(Mutex<Option<Zeroizing<String>>>),
}

impl KeySlot {
    fn keychain() -> Result<keyring::Entry> {
        keyring::Entry::new(KEY_SERVICE, KEY_USER).map_err(|error| keychain_error(&error))
    }

    fn get(&self) -> Result<Option<Zeroizing<String>>> {
        match self {
            Self::Keychain => match Self::keychain()?.get_password() {
                Ok(text) => Ok(Some(Zeroizing::new(text))),
                Err(keyring::Error::NoEntry) => Ok(None),
                Err(error) => Err(keychain_error(&error)),
            },
            Self::Memory(slot) => Ok(lock(slot).clone()),
        }
    }

    fn set(&self, key: &VaultKey) -> Result<()> {
        let text = Zeroizing::new(hex(&key.bytes[..]));
        match self {
            Self::Keychain => Self::keychain()?
                .set_password(&text)
                .map_err(|error| keychain_error(&error)),
            Self::Memory(slot) => {
                *lock(slot) = Some(text);
                Ok(())
            }
        }
    }

    fn delete(&self) -> Result<()> {
        match self {
            Self::Keychain => match Self::keychain()?.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(error) => Err(keychain_error(&error)),
            },
            Self::Memory(slot) => {
                *lock(slot) = None;
                Ok(())
            }
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn keychain_error(error: &keyring::Error) -> DbError {
    DbError::new(ErrorKind::Storage, format!("Keychain: {error}"))
}

fn locked_error() -> DbError {
    DbError::new(
        ErrorKind::Storage,
        "The saved passwords are locked: enter the master password first",
    )
}

fn damaged(what: &str) -> DbError {
    storage_error(format!("The password vault is damaged: {what}"))
}

enum State {
    Locked,
    Open(VaultKey),
}

/// Saved passwords in `vault.json`; see the module's documentation.
pub struct VaultSecretStore {
    file: JsonFile<VaultFile>,
    slot: KeySlot,
    /// Passwords saved before the vault.
    legacy: Box<dyn SecretStore>,
    inner: Mutex<(State, VaultFile)>,
}

impl std::fmt::Debug for VaultSecretStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VaultSecretStore")
            .field("path", &self.file.path())
            .field("locked", &self.is_locked())
            .finish_non_exhaustive()
    }
}

impl VaultSecretStore {
    /// The vault at `path`, its key in the keychain and passwords from
    /// before it in the keychain too.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        Self::with(path, KeySlot::Keychain, Box::new(crate::KeyringSecretStore))
    }

    /// The vault at `path`, made with a new key when there is none. A vault
    /// with a master password starts locked.
    pub fn with(
        path: impl Into<PathBuf>,
        slot: KeySlot,
        legacy: Box<dyn SecretStore>,
    ) -> Result<Self> {
        let file = JsonFile::<VaultFile>::new(path);
        let (state, vault) = match file.read()? {
            None => {
                let key = VaultKey::generate()?;
                let vault = VaultFile {
                    v: VERSION,
                    key: KeySource::Keychain,
                    entries: BTreeMap::new(),
                };
                slot.set(&key)?;
                file.write(&vault)?;
                (State::Open(key), vault)
            }
            Some(vault) if vault.v != VERSION => {
                return Err(storage_error(
                    "The password vault was written by a newer version of Violet",
                ));
            }
            Some(vault) => match &vault.key {
                KeySource::Keychain => {
                    let text = slot.get()?.ok_or_else(|| {
                        storage_error(
                            "The password vault's key is missing from the keychain, so the \
                             saved passwords cannot be read",
                        )
                    })?;
                    let key = VaultKey::new(|bytes| unhex_into(&text, bytes))?;
                    (State::Open(key), vault)
                }
                KeySource::Password { .. } => (State::Locked, vault),
            },
        };
        Ok(Self {
            file,
            slot,
            legacy,
            inner: Mutex::new((state, vault)),
        })
    }

    /// Whether the vault waits for its master password.
    #[must_use]
    pub fn is_locked(&self) -> bool {
        matches!(lock(&self.inner).0, State::Locked)
    }

    /// Whether a master password guards the vault key.
    #[must_use]
    pub fn has_master_password(&self) -> bool {
        matches!(lock(&self.inner).1.key, KeySource::Password { .. })
    }

    /// Opens a locked vault with its master password.
    pub fn unlock(&self, password: &Secret) -> Result<()> {
        let mut inner = lock(&self.inner);
        if matches!(inner.0, State::Open(_)) {
            return Ok(());
        }
        let key = unwrap_key(&inner.1.key, password)?;
        inner.0 = State::Open(key);
        Ok(())
    }

    /// Sets, changes or removes the master password. `current` must be the
    /// master password when there is one; `new` is `None` to keep the
    /// vault key in the keychain instead.
    pub fn set_master_password(
        &self,
        current: Option<&Secret>,
        new: Option<&Secret>,
    ) -> Result<()> {
        let mut inner = lock(&self.inner);
        let (state, vault) = &mut *inner;
        let State::Open(key) = state else {
            return Err(locked_error());
        };
        if matches!(vault.key, KeySource::Password { .. }) {
            let current = current.ok_or_else(wrong_password)?;
            // Proves the password; the key it opens is the one held.
            drop(unwrap_key(&vault.key, current)?);
        }
        let mut next = vault.clone();
        match new {
            Some(password) => {
                if password.expose().chars().count() < MIN_MASTER_PASSWORD {
                    return Err(DbError::new(
                        ErrorKind::Other,
                        format!("Use at least {MIN_MASTER_PASSWORD} characters"),
                    ));
                }
                next.key = wrap_key(key, password)?;
                // The file first: should removing the keychain entry fail,
                // the key is only in one more place than it should be.
                self.file.write(&next)?;
                if let Err(error) = self.slot.delete() {
                    log::warn!("could not remove the vault key from the keychain: {error}");
                }
            }
            None => {
                next.key = KeySource::Keychain;
                // The keychain first: the file never points at a key that
                // is not there.
                self.slot.set(key)?;
                self.file.write(&next)?;
            }
        }
        *vault = next;
        Ok(())
    }

    /// Saves `vault` and adopts it.
    fn store(&self, vault: &mut VaultFile, next: VaultFile) -> Result<()> {
        self.file.write(&next)?;
        *vault = next;
        Ok(())
    }
}

impl SecretStore for VaultSecretStore {
    fn get(&self, id: ConnectionId, target: &SecretTarget) -> Result<SavedSecret> {
        let mut inner = lock(&self.inner);
        let (state, vault) = &mut *inner;
        let State::Open(key) = state else {
            return Err(locked_error());
        };
        if let Some(sealed) = vault.entries.get(&id) {
            if sealed.target != *target {
                return Ok(SavedSecret::Elsewhere(sealed.target.clone()));
            }
            return open_entry(key, id, sealed).map(SavedSecret::Found);
        }
        // Saved before the vault: move it in, then out of the keychain.
        let saved = self.legacy.get(id, target)?;
        if let SavedSecret::Found(secret) = &saved {
            let mut next = vault.clone();
            next.entries
                .insert(id, seal_entry(key, id, target, secret)?);
            self.store(vault, next)?;
            if let Err(error) = self.legacy.delete(id) {
                log::warn!("could not remove a password moved into the vault: {error}");
            }
        }
        Ok(saved)
    }

    fn set(&self, id: ConnectionId, target: &SecretTarget, secret: &Secret) -> Result<()> {
        let mut inner = lock(&self.inner);
        let (state, vault) = &mut *inner;
        let State::Open(key) = state else {
            return Err(locked_error());
        };
        let mut next = vault.clone();
        next.entries
            .insert(id, seal_entry(key, id, target, secret)?);
        self.store(vault, next)?;
        if let Err(error) = self.legacy.delete(id) {
            log::warn!("could not remove a password from the keychain: {error}");
        }
        Ok(())
    }

    fn delete(&self, id: ConnectionId) -> Result<()> {
        // Works locked too: removing needs no key.
        let mut inner = lock(&self.inner);
        let (_, vault) = &mut *inner;
        if vault.entries.contains_key(&id) {
            let mut next = vault.clone();
            next.entries.remove(&id);
            self.store(vault, next)?;
        }
        self.legacy.delete(id)
    }
}

/// What a sealed password is bound to: its connection, the field, and the
/// target it was saved for.
fn entry_aad(id: ConnectionId, target: &SecretTarget) -> Result<Vec<u8>> {
    let target = serde_json::to_string(target).map_err(|error| damaged(&error.to_string()))?;
    Ok(format!("{id}:password\n{target}").into_bytes())
}

fn seal_entry(
    key: &VaultKey,
    id: ConnectionId,
    target: &SecretTarget,
    secret: &Secret,
) -> Result<Sealed> {
    let mut nonce = [0u8; NONCE_LEN];
    random(&mut nonce)?;
    let aad = entry_aad(id, target)?;
    let plain = secret.expose().as_bytes();
    let mut buffer = Zeroizing::new(Vec::with_capacity(plain.len() + TAG_LEN));
    buffer.extend_from_slice(plain);
    let tag = key
        .cipher()
        .encrypt_inout_detached(&XNonce::from(nonce), &aad, buffer.as_mut_slice().into())
        .map_err(|_| storage_error("Cannot seal the password"))?;
    buffer.extend_from_slice(&tag);
    Ok(Sealed {
        target: target.clone(),
        nonce: hex(&nonce),
        sealed: hex(&buffer),
    })
}

fn open_entry(key: &VaultKey, id: ConnectionId, sealed: &Sealed) -> Result<Secret> {
    let nonce: [u8; NONCE_LEN] = unhex(&sealed.nonce)?
        .as_slice()
        .try_into()
        .map_err(|_| damaged("a nonce has the wrong length"))?;
    let mut buffer = Zeroizing::new(unhex(&sealed.sealed)?);
    let Some(split) = buffer.len().checked_sub(TAG_LEN) else {
        return Err(damaged("a password is cut short"));
    };
    let tag = chacha20poly1305::Tag::try_from(&buffer[split..])
        .map_err(|_| damaged("a password is cut short"))?;
    let aad = entry_aad(id, &sealed.target)?;
    key.cipher()
        .decrypt_inout_detached(
            &XNonce::from(nonce),
            &aad,
            (&mut buffer[..split]).into(),
            &tag,
        )
        .map_err(|_| damaged("a password does not open with the vault key"))?;
    let text =
        std::str::from_utf8(&buffer[..split]).map_err(|_| damaged("a password is not text"))?;
    Ok(Secret::new(text))
}

/// The key a master password derives with `params` and `salt`.
fn password_key(password: &Secret, params: (u32, u32, u32), salt: &[u8]) -> Result<VaultKey> {
    let (m, t, p) = params;
    if !M_COST.contains(&m) || !T_COST.contains(&t) || !P_COST.contains(&p) {
        return Err(damaged("its key settings are out of bounds"));
    }
    let params =
        argon2::Params::new(m, t, p, Some(KEY_LEN)).map_err(|error| damaged(&error.to_string()))?;
    let argon = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
    VaultKey::new(|bytes| {
        argon
            .hash_password_into(password.expose().as_bytes(), salt, bytes)
            .map_err(|error| storage_error(format!("Cannot derive the key: {error}")))
    })
}

fn wrap_key(key: &VaultKey, password: &Secret) -> Result<KeySource> {
    let mut salt = [0u8; SALT_LEN];
    random(&mut salt)?;
    let mut nonce = [0u8; NONCE_LEN];
    random(&mut nonce)?;
    let wrapping = password_key(password, KDF, &salt)?;
    let mut buffer = Zeroizing::new(Vec::with_capacity(KEY_LEN + TAG_LEN));
    buffer.extend_from_slice(&key.bytes[..]);
    let tag = wrapping
        .cipher()
        .encrypt_inout_detached(&XNonce::from(nonce), WRAP_AAD, buffer.as_mut_slice().into())
        .map_err(|_| storage_error("Cannot wrap the vault key"))?;
    buffer.extend_from_slice(&tag);
    Ok(KeySource::Password {
        m_cost: KDF.0,
        t_cost: KDF.1,
        p_cost: KDF.2,
        salt: hex(&salt),
        nonce: hex(&nonce),
        wrapped: hex(&buffer),
    })
}

fn wrong_password() -> DbError {
    DbError::new(ErrorKind::Authentication, "Wrong master password")
}

fn unwrap_key(source: &KeySource, password: &Secret) -> Result<VaultKey> {
    let KeySource::Password {
        m_cost,
        t_cost,
        p_cost,
        salt,
        nonce,
        wrapped,
    } = source
    else {
        return Err(storage_error("The vault has no master password"));
    };
    let salt = unhex(salt)?;
    let nonce: [u8; NONCE_LEN] = unhex(nonce)?
        .as_slice()
        .try_into()
        .map_err(|_| damaged("its nonce has the wrong length"))?;
    let mut buffer = Zeroizing::new(unhex(wrapped)?);
    if buffer.len() != KEY_LEN + TAG_LEN {
        return Err(damaged("its key has the wrong length"));
    }
    let tag = chacha20poly1305::Tag::try_from(&buffer[KEY_LEN..])
        .map_err(|_| damaged("its key has the wrong length"))?;
    let wrapping = password_key(password, (*m_cost, *t_cost, *p_cost), &salt)?;
    wrapping
        .cipher()
        .decrypt_inout_detached(
            &XNonce::from(nonce),
            WRAP_AAD,
            (&mut buffer[..KEY_LEN]).into(),
            &tag,
        )
        .map_err(|_| wrong_password())?;
    VaultKey::new(|bytes| {
        bytes.copy_from_slice(&buffer[..KEY_LEN]);
        Ok(())
    })
}

fn random(bytes: &mut [u8]) -> Result<()> {
    getrandom::fill(bytes)
        .map_err(|error| storage_error(format!("No random numbers from the system: {error}")))
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    out
}

fn nibble(digit: u8) -> Result<u8> {
    match digit {
        b'0'..=b'9' => Ok(digit - b'0'),
        b'a'..=b'f' => Ok(digit - b'a' + 10),
        _ => Err(damaged("it holds something other than hex")),
    }
}

fn unhex(text: &str) -> Result<Vec<u8>> {
    let mut out = vec![0; text.len() / 2];
    unhex_into(text, &mut out)?;
    Ok(out)
}

/// Decodes `text` into `out`, which it must fill exactly.
fn unhex_into(text: &str, out: &mut [u8]) -> Result<()> {
    let digits = text.as_bytes();
    if digits.len() != out.len() * 2 {
        return Err(damaged("a value has the wrong length"));
    }
    for (byte, pair) in out.iter_mut().zip(digits.as_chunks::<2>().0) {
        *byte = (nibble(pair[0])? << 4) | nibble(pair[1])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::MemorySecretStore;

    fn target(host: &str) -> SecretTarget {
        SecretTarget {
            driver: "postgres".to_owned(),
            host: host.to_owned(),
            port: 5432,
            user: "app".to_owned(),
            ssl: violet_core::SslMode::Require,
        }
    }

    /// A legacy store shared with the test, to look into.
    struct Shared(Arc<MemorySecretStore>);

    impl SecretStore for Shared {
        fn get(&self, id: ConnectionId, target: &SecretTarget) -> Result<SavedSecret> {
            self.0.get(id, target)
        }
        fn set(&self, id: ConnectionId, target: &SecretTarget, secret: &Secret) -> Result<()> {
            self.0.set(id, target, secret)
        }
        fn delete(&self, id: ConnectionId) -> Result<()> {
            self.0.delete(id)
        }
    }

    struct Fixture {
        dir: tempfile::TempDir,
        key: Arc<Mutex<Option<Zeroizing<String>>>>,
        legacy: Arc<MemorySecretStore>,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                dir: tempfile::tempdir().expect("temp dir"),
                key: Arc::default(),
                legacy: Arc::default(),
            }
        }

        fn path(&self) -> PathBuf {
            self.dir.path().join("vault.json")
        }

        /// The vault, as Violet would open it on start.
        fn open(&self) -> Result<VaultSecretStore> {
            let slot = KeySlot::Memory(Mutex::new(lock(&self.key).clone()));
            let vault = VaultSecretStore::with(
                self.path(),
                slot,
                Box::new(Shared(Arc::clone(&self.legacy))),
            )?;
            // Keep what the vault did to its key slot for the next open.
            if let KeySlot::Memory(slot) = &vault.slot {
                *lock(&self.key) = lock(slot).clone();
            }
            Ok(vault)
        }

        fn sync(&self, vault: &VaultSecretStore) {
            if let KeySlot::Memory(slot) = &vault.slot {
                *lock(&self.key) = lock(slot).clone();
            }
        }

        fn text(&self) -> String {
            std::fs::read_to_string(self.path()).expect("read")
        }
    }

    #[test]
    fn passwords_survive_a_restart_and_never_reach_the_file_in_clear() {
        let fixture = Fixture::new();
        let id = ConnectionId::new();
        let vault = fixture.open().expect("open");
        vault
            .set(id, &target("prod"), &Secret::new("hunter2-secret"))
            .expect("set");
        assert!(!fixture.text().contains("hunter2"));
        drop(vault);

        let vault = fixture.open().expect("reopen");
        assert_eq!(
            vault.get(id, &target("prod")).expect("get"),
            SavedSecret::Found(Secret::new("hunter2-secret"))
        );
        vault.delete(id).expect("delete");
        assert_eq!(
            vault.get(id, &target("prod")).expect("get"),
            SavedSecret::None
        );
    }

    #[test]
    fn a_password_moved_to_another_target_or_connection_does_not_open() {
        let fixture = Fixture::new();
        let (prod, other) = (ConnectionId::new(), ConnectionId::new());
        let vault = fixture.open().expect("open");
        vault
            .set(prod, &target("prod"), &Secret::new("pw"))
            .expect("set");
        vault
            .set(other, &target("test"), &Secret::new("other"))
            .expect("set");
        drop(vault);

        // Rewrite the target beside the sealed password: the tag fails.
        let mut file: VaultFile = serde_json::from_str(&fixture.text()).expect("parse");
        file.entries.get_mut(&prod).expect("entry").target = target("evil");
        // And paste prod's sealed password onto the other connection.
        let stolen = file.entries[&prod].clone();
        file.entries.insert(
            other,
            Sealed {
                target: target("test"),
                ..stolen
            },
        );
        std::fs::write(fixture.path(), serde_json::to_string(&file).expect("json")).expect("write");

        let vault = fixture.open().expect("reopen");
        assert!(vault.get(prod, &target("evil")).is_err());
        assert_eq!(
            vault.get(prod, &target("prod")).expect("get"),
            SavedSecret::Elsewhere(target("evil"))
        );
        assert!(vault.get(other, &target("test")).is_err());
    }

    #[test]
    fn a_master_password_locks_the_vault_and_takes_the_key_out_of_the_keychain() {
        let fixture = Fixture::new();
        let id = ConnectionId::new();
        let vault = fixture.open().expect("open");
        vault
            .set(id, &target("prod"), &Secret::new("pw"))
            .expect("set");
        assert!(
            vault
                .set_master_password(None, Some(&Secret::new("short")))
                .is_err()
        );
        vault
            .set_master_password(None, Some(&Secret::new("a long master password")))
            .expect("set master");
        fixture.sync(&vault);
        assert!(lock(&fixture.key).is_none());
        drop(vault);

        let vault = fixture.open().expect("reopen");
        assert!(vault.is_locked());
        assert!(vault.has_master_password());
        assert!(vault.get(id, &target("prod")).is_err());
        assert_eq!(
            vault
                .unlock(&Secret::new("wrong password!"))
                .map_err(|e| e.kind),
            Err(ErrorKind::Authentication)
        );
        vault
            .unlock(&Secret::new("a long master password"))
            .expect("unlock");
        assert_eq!(
            vault.get(id, &target("prod")).expect("get"),
            SavedSecret::Found(Secret::new("pw"))
        );

        // Removing it needs it, and puts the key back in the keychain.
        assert!(vault.set_master_password(None, None).is_err());
        vault
            .set_master_password(Some(&Secret::new("a long master password")), None)
            .expect("remove");
        fixture.sync(&vault);
        drop(vault);
        let vault = fixture.open().expect("reopen");
        assert!(!vault.is_locked());
        assert_eq!(
            vault.get(id, &target("prod")).expect("get"),
            SavedSecret::Found(Secret::new("pw"))
        );
    }

    #[test]
    fn passwords_from_before_the_vault_move_in_when_read() {
        let fixture = Fixture::new();
        let id = ConnectionId::new();
        fixture
            .legacy
            .set(id, &target("prod"), &Secret::new("old"))
            .expect("legacy");
        let vault = fixture.open().expect("open");
        assert_eq!(
            vault.get(id, &target("prod")).expect("get"),
            SavedSecret::Found(Secret::new("old"))
        );
        assert_eq!(
            fixture.legacy.get(id, &target("prod")).expect("legacy"),
            SavedSecret::None
        );
        drop(vault);
        let vault = fixture.open().expect("reopen");
        assert_eq!(
            vault.get(id, &target("prod")).expect("get"),
            SavedSecret::Found(Secret::new("old"))
        );
    }

    #[test]
    fn a_vault_whose_key_is_gone_or_whose_settings_are_absurd_is_refused() {
        let fixture = Fixture::new();
        drop(fixture.open().expect("open"));
        *lock(&fixture.key) = None;
        assert!(fixture.open().is_err());

        let fixture = Fixture::new();
        let vault = fixture.open().expect("open");
        vault
            .set_master_password(None, Some(&Secret::new("a long master password")))
            .expect("set master");
        drop(vault);
        let mut file: VaultFile = serde_json::from_str(&fixture.text()).expect("parse");
        if let KeySource::Password { m_cost, .. } = &mut file.key {
            *m_cost = u32::MAX;
        }
        std::fs::write(fixture.path(), serde_json::to_string(&file).expect("json")).expect("write");
        let vault = fixture.open().expect("reopen");
        assert!(
            vault
                .unlock(&Secret::new("a long master password"))
                .is_err()
        );
    }

    #[test]
    fn hex_round_trips() {
        let bytes = [0u8, 1, 0x7f, 0x80, 0xff];
        assert_eq!(hex(&bytes), "00017f80ff");
        assert_eq!(unhex("00017f80ff").expect("unhex"), bytes);
        assert!(unhex("0g").is_err());
        assert!(unhex("abc").is_err());
    }
}
