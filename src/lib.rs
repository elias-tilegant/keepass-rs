#![doc = include_str!("../README.md")]
#![recursion_limit = "1024"]

mod compression;
pub mod config;
pub(crate) mod crypt;
pub mod db;
pub mod error;
pub(crate) mod format;

mod key;

pub use self::db::Database;

#[cfg(feature = "challenge_response")]
pub use self::key::ChallengeResponseKey;
pub use self::key::DatabaseKey;

/// Diagnostics: decrypt a KDBX 4 file to its inner cleartext XML, no
/// further parsing. Useful for debugging interop with other clients —
/// inspect the literal XML our writer produces vs. what the reader expects.
pub use self::format::kdbx4::debug_decrypt_to_xml;
