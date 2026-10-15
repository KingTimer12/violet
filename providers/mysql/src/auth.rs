//! Authentication plugins: what the client answers to the server's
//! challenge (its nonce) for each one Violet speaks.

use rsa::pkcs1::DecodeRsaPublicKey as _;
use rsa::pkcs8::DecodePublicKey as _;
use rsa::{Oaep, RsaPublicKey};
use sha1::Sha1;
use sha2::{Digest as _, Sha256};
use violet_sdk::{DbError, Result};

pub(crate) const NATIVE: &str = "mysql_native_password";
pub(crate) const CACHING_SHA2: &str = "caching_sha2_password";
pub(crate) const SHA256: &str = "sha256_password";
pub(crate) const CLEAR: &str = "mysql_clear_password";

/// Asks the server for its RSA public key (`sha256_password`'s first
/// answer, `caching_sha2_password`'s after a full authentication request).
pub(crate) const REQUEST_SHA256_KEY: u8 = 0x01;
pub(crate) const REQUEST_CACHING_SHA2_KEY: u8 = 0x02;

/// `caching_sha2_password`'s verdicts after the scramble.
pub(crate) const FAST_AUTH_OK: u8 = 0x03;
pub(crate) const FULL_AUTH: u8 = 0x04;

/// The first answer to `plugin`'s challenge. `secure` is true over TLS or
/// a local socket, where the password may travel as is.
pub(crate) fn answer(
    plugin: &str,
    password: Option<&str>,
    nonce: &[u8],
    secure: bool,
) -> Result<Vec<u8>> {
    let password = password.unwrap_or_default();
    Ok(match plugin {
        // An empty password is answered with nothing, whatever the plugin.
        _ if password.is_empty() => Vec::new(),
        NATIVE => native(password.as_bytes(), nonce).to_vec(),
        CACHING_SHA2 => caching_sha2(password.as_bytes(), nonce).to_vec(),
        SHA256 if secure => cleartext(password),
        SHA256 => vec![REQUEST_SHA256_KEY],
        CLEAR if secure => cleartext(password),
        CLEAR => {
            return Err(DbError::tls(
                "The server asks for the password in plain text; connect with TLS",
            ));
        }
        other => {
            return Err(DbError::unsupported(format!(
                "The server asks for the {other} authentication method, \
                 which Violet does not support"
            )));
        }
    })
}

/// The password and a NUL, as `sha256_password` and a full
/// `caching_sha2_password` authentication take it over a secure channel.
pub(crate) fn cleartext(password: &str) -> Vec<u8> {
    let mut bytes = password.as_bytes().to_vec();
    bytes.push(0);
    bytes
}

/// `SHA1(password) XOR SHA1(nonce + SHA1(SHA1(password)))`.
pub(crate) fn native(password: &[u8], nonce: &[u8]) -> [u8; 20] {
    let stage1: [u8; 20] = Sha1::digest(password).into();
    let stage2: [u8; 20] = Sha1::digest(stage1).into();
    let mut hasher = Sha1::new();
    hasher.update(nonce);
    hasher.update(stage2);
    let mix: [u8; 20] = hasher.finalize().into();
    xor(stage1, &mix)
}

/// `SHA256(password) XOR SHA256(SHA256(SHA256(password)) + nonce)`.
pub(crate) fn caching_sha2(password: &[u8], nonce: &[u8]) -> [u8; 32] {
    let stage1: [u8; 32] = Sha256::digest(password).into();
    let stage2: [u8; 32] = Sha256::digest(stage1).into();
    let mut hasher = Sha256::new();
    hasher.update(stage2);
    hasher.update(nonce);
    let mix: [u8; 32] = hasher.finalize().into();
    xor(stage1, &mix)
}

fn xor<const N: usize>(mut a: [u8; N], b: &[u8; N]) -> [u8; N] {
    for (x, y) in a.iter_mut().zip(b) {
        *x ^= y;
    }
    a
}

/// The password, NUL-terminated and mixed with the nonce, encrypted with
/// the server's public key (RSA-OAEP), for plain connections.
pub(crate) fn encrypt(password: &str, nonce: &[u8], pem: &[u8]) -> Result<Vec<u8>> {
    let pem = std::str::from_utf8(pem)
        .map_err(|_| DbError::authentication("The server sent an unreadable public key"))?
        .trim_matches(char::from(0))
        .trim();
    let key = RsaPublicKey::from_public_key_pem(pem)
        .or_else(|_| RsaPublicKey::from_pkcs1_pem(pem))
        .map_err(|error| {
            DbError::authentication(format!("The server sent an unusable public key: {error}"))
        })?;
    let mut message = cleartext(password);
    if !nonce.is_empty() {
        for (i, byte) in message.iter_mut().enumerate() {
            *byte ^= nonce[i % nonce.len()];
        }
    }
    key.encrypt(&mut SystemRandom, Oaep::new::<Sha1>(), &message)
        .map_err(|error| {
            DbError::authentication(format!("Could not encrypt the password: {error}"))
        })
}

/// The host's randomness, for OAEP's padding.
struct SystemRandom;

impl rand_core::RngCore for SystemRandom {
    fn next_u32(&mut self) -> u32 {
        let mut bytes = [0; 4];
        self.fill_bytes(&mut bytes);
        u32::from_le_bytes(bytes)
    }

    fn next_u64(&mut self) -> u64 {
        let mut bytes = [0; 8];
        self.fill_bytes(&mut bytes);
        u64::from_le_bytes(bytes)
    }

    fn fill_bytes(&mut self, dest: &mut [u8]) {
        // Without randomness there is nothing safe to do.
        #[allow(clippy::expect_used)]
        getrandom::fill(dest).expect("the host provides randomness");
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core::Error> {
        getrandom::fill(dest).map_err(|error| {
            rand_core::Error::from(
                std::num::NonZeroU32::new(error.raw_os_error().unwrap_or(1).unsigned_abs())
                    .unwrap_or(std::num::NonZeroU32::MIN),
            )
        })
    }
}

impl rand_core::CryptoRng for SystemRandom {}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// The scrambles undo to the password's hash given the server's stored
    /// hash, as the server checks them.
    #[test]
    fn scrambles_are_checked_like_the_server_does() {
        let nonce = b"abcdefghijklmnopqrst";
        let password = b"secret";

        let answer = native(password, nonce);
        let stage2: [u8; 20] = Sha1::digest(Sha1::digest(password)).into();
        let mut hasher = Sha1::new();
        hasher.update(nonce);
        hasher.update(stage2);
        let mix: [u8; 20] = hasher.finalize().into();
        let stage1 = xor(answer, &mix);
        assert_eq!(hex(&Sha1::digest(stage1)), hex(&stage2));

        let answer = caching_sha2(password, nonce);
        let stored: [u8; 32] = Sha256::digest(Sha256::digest(password)).into();
        let mut hasher = Sha256::new();
        hasher.update(stored);
        hasher.update(nonce);
        let mix: [u8; 32] = hasher.finalize().into();
        let stage1 = xor(answer, &mix);
        assert_eq!(hex(&Sha256::digest(stage1)), hex(&stored));
    }

    #[test]
    fn passwords_are_never_sent_in_the_clear_without_tls() {
        assert_eq!(
            answer(SHA256, Some("pw"), b"n", false).expect("sha256"),
            [1]
        );
        assert_eq!(
            answer(SHA256, Some("pw"), b"n", true).expect("tls"),
            b"pw\0"
        );
        assert!(answer(CLEAR, Some("pw"), b"n", false).is_err());
        assert!(answer(NATIVE, None, b"n", false).expect("empty").is_empty());
        assert!(answer("client_ed25519", Some("pw"), b"n", false).is_err());
    }
}
