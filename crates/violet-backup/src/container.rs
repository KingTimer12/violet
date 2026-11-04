//! The `.dbv2bak` file: a fixed header, then the sealed payload.
//!
//! | Offset | Size | Field                                              |
//! |--------|------|----------------------------------------------------|
//! | 0      | 8    | `DBV2BAK` and the format version, `0x01`           |
//! | 8      | 12   | Argon2id `m_cost`, `t_cost`, `p_cost` (u32 LE)     |
//! | 20     | 16   | salt                                               |
//! | 36     | 24   | XChaCha20-Poly1305 nonce                           |
//! | 60     | n    | ciphertext, then the 16-byte Poly1305 tag          |
//!
//! The 60 header bytes are the AEAD's associated data. The plaintext is
//! the archive's size (u64 LE) and the archive compressed with zstd.
//!
//! Argon2id runs before the tag can be checked, so its settings are the
//! one thing an attacker controls before authentication: they are held to
//! [`KdfParams::M_COST`], [`KdfParams::T_COST`] and [`KdfParams::P_COST`]
//! before any memory is set aside for them.

use std::io::{Read as _, Write as _};
use std::ops::RangeInclusive;

use chacha20poly1305::aead::AeadInOut as _;
use chacha20poly1305::{Key, KeyInit as _, XChaCha20Poly1305, XNonce};
use violet_core::Secret;
use zeroize::Zeroizing;

use crate::{Error, Result};

/// The extension backups are saved with.
pub const EXTENSION: &str = "dbv2bak";

/// `DBV2BAK`, then the format version.
const MAGIC: &[u8; 7] = b"DBV2BAK";
const VERSION: u8 = 1;

pub const HEADER_LEN: usize = 60;
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 24;
const TAG_LEN: usize = 16;
const SIZE_LEN: usize = 8;

/// Largest archive a backup may hold, uncompressed.
pub const MAX_PAYLOAD: u64 = 256 * 1024 * 1024;

/// Largest backup file read: the largest payload, should it not compress
/// at all, with zstd's framing and the header and tag around it.
pub const MAX_FILE: u64 = MAX_PAYLOAD + MAX_PAYLOAD / 64 + 64 * 1024;

const ZSTD_LEVEL: i32 = 9;

/// Argon2id's settings: memory in KiB, passes, lanes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KdfParams {
    pub m_cost: u32,
    pub t_cost: u32,
    pub p_cost: u32,
}

impl KdfParams {
    /// What new backups use: 64 MiB, three passes, one lane.
    pub const DEFAULT: Self = Self {
        m_cost: 64 * 1024,
        t_cost: 3,
        p_cost: 1,
    };

    /// From OWASP's minimum (19 MiB) to 1 GiB.
    pub const M_COST: RangeInclusive<u32> = 19_456..=1_048_576;
    pub const T_COST: RangeInclusive<u32> = 2..=16;
    pub const P_COST: RangeInclusive<u32> = 1..=16;

    /// The settings, if all three are in bounds.
    pub fn check(self) -> Result<Self> {
        let within = |name: &str, value: u32, range: &RangeInclusive<u32>| {
            if range.contains(&value) {
                Ok(())
            } else {
                Err(Error::UnsafeKdf(format!(
                    "{name} {value} is outside {}..={}",
                    range.start(),
                    range.end()
                )))
            }
        };
        within("m_cost", self.m_cost, &Self::M_COST)?;
        within("t_cost", self.t_cost, &Self::T_COST)?;
        within("p_cost", self.p_cost, &Self::P_COST)?;
        Ok(self)
    }
}

/// The header's fields.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Header {
    kdf: KdfParams,
    salt: [u8; SALT_LEN],
    nonce: [u8; NONCE_LEN],
}

impl Header {
    fn to_bytes(&self) -> [u8; HEADER_LEN] {
        let mut bytes = [0; HEADER_LEN];
        bytes[..7].copy_from_slice(MAGIC);
        bytes[7] = VERSION;
        bytes[8..12].copy_from_slice(&self.kdf.m_cost.to_le_bytes());
        bytes[12..16].copy_from_slice(&self.kdf.t_cost.to_le_bytes());
        bytes[16..20].copy_from_slice(&self.kdf.p_cost.to_le_bytes());
        bytes[20..36].copy_from_slice(&self.salt);
        bytes[36..60].copy_from_slice(&self.nonce);
        bytes
    }

    /// Reads a header, refusing other files, later formats and unsafe key
    /// settings.
    fn parse(bytes: &[u8; HEADER_LEN]) -> Result<Self> {
        if &bytes[..7] != MAGIC {
            return Err(Error::NotABackup);
        }
        if bytes[7] != VERSION {
            return Err(Error::NewerFormat);
        }
        let word = |at: usize| {
            u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
        };
        let kdf = KdfParams {
            m_cost: word(8),
            t_cost: word(12),
            p_cost: word(16),
        }
        .check()?;
        let mut salt = [0; SALT_LEN];
        salt.copy_from_slice(&bytes[20..36]);
        let mut nonce = [0; NONCE_LEN];
        nonce.copy_from_slice(&bytes[36..60]);
        Ok(Self { kdf, salt, nonce })
    }
}

fn random<const N: usize>() -> Result<[u8; N]> {
    let mut bytes = [0; N];
    getrandom::fill(&mut bytes)
        .map_err(|error| Error::Io(format!("No random numbers from the system: {error}")))?;
    Ok(bytes)
}

/// The cipher for `password` with the header's settings and salt.
fn cipher(password: &Secret, header: &Header) -> Result<XChaCha20Poly1305> {
    let params = argon2::Params::new(
        header.kdf.m_cost,
        header.kdf.t_cost,
        header.kdf.p_cost,
        Some(32),
    )
    .map_err(|error| Error::UnsafeKdf(error.to_string()))?;
    let argon = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
    let mut key = Zeroizing::new([0u8; 32]);
    argon
        .hash_password_into(
            password.expose().as_bytes(),
            &header.salt,
            key.as_mut_slice(),
        )
        .map_err(|error| Error::Io(format!("Cannot derive the key: {error}")))?;
    let key: &Key = (&*key).into();
    Ok(XChaCha20Poly1305::new(key))
}

/// Compresses and seals `archive` with `password`.
pub fn seal(archive: &[u8], password: &Secret) -> Result<Vec<u8>> {
    seal_with(archive, password, KdfParams::DEFAULT)
}

pub(crate) fn seal_with(archive: &[u8], password: &Secret, kdf: KdfParams) -> Result<Vec<u8>> {
    if archive.len() as u64 > MAX_PAYLOAD {
        return Err(Error::TooLarge);
    }
    let header = Header {
        kdf: kdf.check()?,
        salt: random()?,
        nonce: random()?,
    };
    let head = header.to_bytes();

    // One buffer, sized for the worst case so it never grows: the header,
    // the size, the compressed archive (sealed in place), the tag.
    let bound = zstd::zstd_safe::compress_bound(archive.len());
    let mut out = Vec::with_capacity(HEADER_LEN + SIZE_LEN + bound + TAG_LEN);
    out.extend_from_slice(&head);
    out.extend_from_slice(&(archive.len() as u64).to_le_bytes());
    let mut encoder = zstd::stream::write::Encoder::new(&mut out, ZSTD_LEVEL)?;
    encoder.write_all(archive)?;
    encoder.finish()?;

    let cipher = cipher(password, &header)?;
    let (aad, body) = out.split_at_mut(HEADER_LEN);
    let tag = cipher
        .encrypt_inout_detached(&XNonce::from(header.nonce), aad, body.into())
        .map_err(|_| Error::Io("Cannot encrypt the backup".to_owned()))?;
    out.extend_from_slice(&tag);
    Ok(out)
}

/// The archive `file` holds, if `password` opens it.
pub fn open(file: &[u8], password: &Secret) -> Result<Zeroizing<Vec<u8>>> {
    let Some(head) = file.first_chunk::<HEADER_LEN>() else {
        return Err(if file.starts_with(MAGIC) {
            Error::Malformed("it is cut short".to_owned())
        } else {
            Error::NotABackup
        });
    };
    let header = Header::parse(head)?;
    if file.len() as u64 > MAX_FILE {
        return Err(Error::TooLarge);
    }
    let Some(sealed_len) = file.len().checked_sub(HEADER_LEN + TAG_LEN) else {
        return Err(Error::Malformed("it is cut short".to_owned()));
    };
    if sealed_len < SIZE_LEN {
        return Err(Error::Malformed("it is cut short".to_owned()));
    }
    let (sealed, tag) = file[HEADER_LEN..].split_at(sealed_len);

    let cipher = cipher(password, &header)?;
    let mut plain = Zeroizing::new(sealed.to_vec());
    let tag = chacha20poly1305::Tag::try_from(tag).map_err(|_| Error::Decrypt)?;
    cipher
        .decrypt_inout_detached(
            &XNonce::from(header.nonce),
            head,
            plain.as_mut_slice().into(),
            &tag,
        )
        .map_err(|_| Error::Decrypt)?;

    let (size, compressed) = plain.split_at(SIZE_LEN);
    let size = u64::from_le_bytes(size.try_into().map_err(|_| Error::Decrypt)?);
    if size > MAX_PAYLOAD {
        return Err(Error::TooLarge);
    }
    let size = usize::try_from(size).map_err(|_| Error::TooLarge)?;
    // Sized up front so it never moves (a move leaves a copy behind), one
    // byte more than announced to catch a stream that runs on.
    let mut archive = Zeroizing::new(Vec::with_capacity(size + 1));
    zstd::stream::read::Decoder::new(compressed)?
        .take(size as u64 + 1)
        .read_to_end(&mut archive)
        .map_err(|error| Error::Malformed(format!("cannot decompress it: {error}")))?;
    if archive.len() != size {
        return Err(Error::Malformed(
            "it does not hold the size it announces".to_owned(),
        ));
    }
    Ok(archive)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cheapest settings allowed, so tests run fast.
    const FAST: KdfParams = KdfParams {
        m_cost: 19_456,
        t_cost: 2,
        p_cost: 1,
    };

    fn password() -> Secret {
        Secret::new("correct horse battery staple")
    }

    fn sealed(archive: &[u8]) -> Vec<u8> {
        seal_with(archive, &password(), FAST).expect("seal")
    }

    #[test]
    fn a_sealed_archive_opens_with_its_password() {
        let archive = b"hello, backup".repeat(100);
        let file = sealed(&archive);
        assert_eq!(&file[..8], b"DBV2BAK\x01");
        assert_eq!(*open(&file, &password()).expect("open"), archive);
        // A fresh salt and nonce every time.
        assert_ne!(sealed(&archive), file);
    }

    #[test]
    fn the_wrong_password_is_refused() {
        let file = sealed(b"data");
        assert_eq!(
            open(&file, &Secret::new("guess")).map(|_| ()),
            Err(Error::Decrypt)
        );
    }

    #[test]
    fn flipping_any_bit_of_header_or_ciphertext_fails() {
        let file = sealed(&b"payload".repeat(10));
        // Salt, nonce and every byte after: the tag catches them all. The
        // magic and the key settings are refused before that, or caught
        // by the tag too.
        for at in [
            0,
            7,
            8,
            13,
            17,
            20,
            35,
            36,
            59,
            60,
            61,
            file.len() / 2,
            file.len() - 1,
        ] {
            let mut damaged = file.clone();
            damaged[at] ^= 0x01;
            assert!(open(&damaged, &password()).is_err(), "byte {at}");
        }
    }

    #[test]
    fn a_header_raising_the_key_cost_is_refused_before_any_work() {
        let mut file = sealed(b"data");
        // 64 GiB of memory for Argon2id.
        file[8..12].copy_from_slice(&(64 * 1024 * 1024u32).to_le_bytes());
        assert!(matches!(open(&file, &password()), Err(Error::UnsafeKdf(_))));
        let mut file = sealed(b"data");
        file[12..16].copy_from_slice(&1_000_000u32.to_le_bytes());
        assert!(matches!(open(&file, &password()), Err(Error::UnsafeKdf(_))));
        let mut file = sealed(b"data");
        file[16..20].copy_from_slice(&0u32.to_le_bytes());
        assert!(matches!(open(&file, &password()), Err(Error::UnsafeKdf(_))));
    }

    #[test]
    fn other_files_and_later_formats_are_told_apart() {
        assert_eq!(
            open(b"PK\x03\x04", &password()).map(|_| ()),
            Err(Error::NotABackup)
        );
        assert_eq!(
            open(&[0; 100], &password()).map(|_| ()),
            Err(Error::NotABackup)
        );
        let mut file = sealed(b"data");
        file[7] = 2;
        assert_eq!(
            open(&file, &password()).map(|_| ()),
            Err(Error::NewerFormat)
        );
        assert!(matches!(
            open(&file[..40], &password()),
            Err(Error::Malformed(_))
        ));
    }

    /// Seals `plain` as is, size prefix included, to forge payloads.
    fn forge(plain: &[u8]) -> Vec<u8> {
        let header = Header {
            kdf: FAST,
            salt: [7; SALT_LEN],
            nonce: [9; NONCE_LEN],
        };
        let head = header.to_bytes();
        let mut out = head.to_vec();
        out.extend_from_slice(plain);
        let cipher = cipher(&password(), &header).expect("cipher");
        let (aad, body) = out.split_at_mut(HEADER_LEN);
        let tag = cipher
            .encrypt_inout_detached(&XNonce::from(header.nonce), aad, body.into())
            .expect("encrypt");
        out.extend_from_slice(&tag);
        out
    }

    #[test]
    fn a_payload_lying_about_its_size_is_refused() {
        let archive = vec![0u8; 1024 * 1024];
        let compressed = zstd::bulk::compress(&archive, 3).expect("compress");

        // Claims less than the stream holds: a decompression bomb.
        let mut plain = 10u64.to_le_bytes().to_vec();
        plain.extend_from_slice(&compressed);
        assert!(matches!(
            open(&forge(&plain), &password()),
            Err(Error::Malformed(_))
        ));

        // Claims more than the stream holds.
        let mut plain = (2 * 1024 * 1024u64).to_le_bytes().to_vec();
        plain.extend_from_slice(&compressed);
        assert!(matches!(
            open(&forge(&plain), &password()),
            Err(Error::Malformed(_))
        ));

        // Claims more than any backup may hold: refused before allocating.
        let mut plain = (MAX_PAYLOAD + 1).to_le_bytes().to_vec();
        plain.extend_from_slice(&compressed);
        assert_eq!(
            open(&forge(&plain), &password()).map(|_| ()),
            Err(Error::TooLarge)
        );
    }

    #[test]
    fn default_settings_are_in_bounds() {
        assert_eq!(KdfParams::DEFAULT.check(), Ok(KdfParams::DEFAULT));
    }
}
