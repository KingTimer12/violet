//! Encrypted, portable backups of a workspace: its connections, their saved
//! passwords and its scripts, in one `.dbv2bak` file that opens on any
//! machine with the password it was made with.
//!
//! Three layers, each checked on the way in:
//!
//! - [`container`]: the file. A 60-byte header (magic, Argon2id settings,
//!   salt, nonce), then the payload sealed with XChaCha20-Poly1305 under a
//!   key derived from the password. The header is the AEAD's associated
//!   data, so changing any byte of it fails the tag. The payload is the
//!   archive's exact size and the archive compressed with zstd.
//! - [`archive`]: a tar of regular files only, extracted in memory, with
//!   links, special files and paths that climb out refused.
//! - [`bundle`]: what the archive holds: a manifest, the connections, the
//!   passwords and the scripts' files.
//!
//! [`workspace`] gathers a bundle from a workspace and writes its scripts
//! back. Nothing here touches the disk but that and the scripts folder:
//! the archive is built, compressed and sealed in memory, in buffers
//! sized up front and cleared when dropped.

pub mod archive;
pub mod bundle;
pub mod container;
pub mod workspace;

pub use bundle::{Bundle, BundleFile, Manifest};
pub use container::{EXTENSION, KdfParams};

/// Why a backup could not be made or read.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("This file is not a Violet backup")]
    NotABackup,
    #[error("This backup was made by a newer version of Violet")]
    NewerFormat,
    #[error("This backup asks for unsafe key settings ({0}); it was not made by Violet")]
    UnsafeKdf(String),
    #[error("Wrong password, or the backup is damaged")]
    Decrypt,
    #[error("The backup would be larger than {} MB", container::MAX_PAYLOAD / (1024 * 1024))]
    TooLarge,
    #[error("The backup is damaged: {0}")]
    Malformed(String),
    #[error("{0}")]
    Io(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

impl From<violet_core::DbError> for Error {
    fn from(error: violet_core::DbError) -> Self {
        Self::Io(error.message)
    }
}
