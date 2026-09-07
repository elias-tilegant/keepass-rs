use std::io::Read;

use base64::{engine::general_purpose as base64_engine, Engine as _};
use quick_xml::{encoding::EncodingError, events::Event, reader::Reader};
use thiserror::Error;
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::crypt::calculate_sha256;

pub type KeyElement = Vec<u8>;
pub type KeyElements = Vec<KeyElement>;

#[cfg(feature = "challenge_response")]
mod yubikey;

#[cfg(feature = "challenge_response")]
pub use yubikey::{ChallengeResponseKey, ChallengeResponseKeyError};

fn parse_xml_keyfile(xml: &[u8]) -> Result<KeyElement, ParseXmlKeyFileError> {
    let mut tag_stack = Vec::new();

    let mut key_version: Option<String> = None;
    let mut key_value: Option<String> = None;

    let mut reader = Reader::from_reader(xml);
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Eof => break,

            Event::Start(e) => {
                tag_stack.push(String::from_utf8_lossy(e.name().as_ref()).to_string());
            }

            Event::End(_) => {
                tag_stack.pop();
            }

            Event::Text(e) => {
                let s = e.decode()?.into_owned();

                if tag_stack == ["KeyFile", "Meta", "Version"] {
                    key_version = Some(s);
                    continue;
                }

                if tag_stack == ["KeyFile", "Key", "Data"] {
                    key_value = Some(s);
                    continue;
                }
            }

            _ => (),
        }
    }

    let key_value = key_value.ok_or(ParseXmlKeyFileError::EmptyKey)?;

    let key_bytes = key_value.as_bytes().to_vec();

    if key_version == Some("2.0".to_string()) {
        // TODO we should also validate the integrity of a v2 keyfile using the hash value

        let trimmed_key = key_value
            .trim()
            .replace(" ", "")
            .replace("\n", "")
            .replace("\t", "")
            .replace("\r", "");

        return if let Ok(key) = hex::decode(&trimmed_key) {
            Ok(key)
        } else {
            Ok(key_bytes)
        };
    }

    // Check if the key is base64-encoded. If yes, return decoded bytes
    if let Ok(key) = base64_engine::STANDARD.decode(&key_bytes) {
        Ok(key)
    } else {
        Ok(key_bytes)
    }
}

#[derive(Debug, Error)]
pub enum ParseXmlKeyFileError {
    #[error("The XML keyfile is missing a key data element")]
    EmptyKey,

    #[error(transparent)]
    Encoding(#[from] EncodingError),

    #[error(transparent)]
    Xml(#[from] quick_xml::Error),
}

fn parse_keyfile(buffer: &[u8]) -> Result<KeyElement, DatabaseKeyError> {
    // try to parse the buffer as XML, if successful, use that data instead of full file
    if let Ok(v) = parse_xml_keyfile(buffer) {
        Ok(v)
    } else if buffer.len() == 32 {
        // legacy binary key format
        Ok(buffer.to_vec())
    } else {
        Ok(calculate_sha256(&[buffer]).as_slice().to_vec())
    }
}

/// A KeePass key, which might consist of a password and/or a keyfile
///
/// The password is not kept. KDBX only ever uses its SHA-256 as one element
/// of the composite key, so that is what is stored: a caller who unlocks a
/// database no longer holds a reusable credential for the life of the
/// session, and a memory dump yields a value that has to be brute-forced
/// rather than the plaintext the user probably typed somewhere else too.
#[derive(Clone, Default, PartialEq, Zeroize, ZeroizeOnDrop)]
pub struct DatabaseKey {
    /// SHA-256 of the password, computed once by [`Self::with_password`].
    password_hash: Option<KeyElement>,
    keyfile: Option<Vec<u8>>,
    #[cfg(feature = "challenge_response")]
    challenge_response_key: Option<ChallengeResponseKey>,
    #[cfg(feature = "challenge_response")]
    challenge_response_result: Option<KeyElement>,
}

/// Redacted by hand. Both fields are key material, and the derived `Debug`
/// printed them into any log line or panic message that formatted a key.
impl std::fmt::Debug for DatabaseKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DatabaseKey")
            .field("has_password", &self.password_hash.is_some())
            .field("has_keyfile", &self.keyfile.is_some())
            .finish_non_exhaustive()
    }
}

impl DatabaseKey {
    pub fn with_password(mut self, password: &str) -> Self {
        self.password_hash = Some(calculate_sha256(&[password.as_bytes()]).to_vec());
        self
    }

    #[cfg(feature = "utilities")]
    pub fn with_password_from_prompt(self, prompt_message: &str) -> Result<Self, std::io::Error> {
        let password = rpassword::prompt_password(prompt_message)?;
        Ok(self.with_password(&password))
    }

    #[cfg(all(feature = "challenge_response", feature = "utilities"))]
    pub fn with_hmac_sha1_secret_from_prompt(mut self, prompt_message: &str) -> Result<Self, std::io::Error> {
        self.challenge_response_key = Some(ChallengeResponseKey::LocalChallenge(rpassword::prompt_password(
            prompt_message,
        )?));
        Ok(self)
    }
    /// Creates a database key with a `keyfile`
    ///
    /// # Errors
    ///
    /// Fails if the `keyfile` cannot be read
    pub fn with_keyfile(mut self, keyfile: &mut dyn Read) -> Result<Self, std::io::Error> {
        let mut buf = Vec::new();
        keyfile.read_to_end(&mut buf)?;

        self.keyfile = Some(buf);

        Ok(self)
    }

    #[cfg(feature = "challenge_response")]
    pub fn with_challenge_response_key(mut self, challenge_response_key: ChallengeResponseKey) -> Self {
        self.challenge_response_key = Some(challenge_response_key);
        self
    }

    #[cfg(feature = "challenge_response")]
    pub fn perform_challenge(mut self, kdf_seed: &[u8]) -> Result<Self, DatabaseKeyError> {
        if let Some(challenge_response_key) = &self.challenge_response_key {
            let response = challenge_response_key.perform_challenge(kdf_seed)?;
            self.challenge_response_result = Some(response);
        }

        Ok(self)
    }

    pub fn new() -> Self {
        Default::default()
    }

    pub(crate) fn get_key_elements(&self) -> Result<KeyElements, DatabaseKeyError> {
        let mut out = Vec::new();

        if let Some(password_hash) = &self.password_hash {
            out.push(password_hash.clone());
        }

        if let Some(ref f) = self.keyfile {
            out.push(parse_keyfile(f)?);
        }

        if out.is_empty() {
            return Err(DatabaseKeyError::EmptyKey);
        }

        #[cfg(feature = "challenge_response")]
        if let Some(result) = &self.challenge_response_result {
            out.push(calculate_sha256(&[result]).as_slice().to_vec());
        } else if self.challenge_response_key.is_some() {
            return Err(DatabaseKeyError::ChallengeResponse(
                crate::key::yubikey::ChallengeResponseKeyError::NotPerformed,
            ));
        }

        Ok(out)
    }

    /// Returns true if the database key is not associated with any key component.
    pub fn is_empty(&self) -> bool {
        if self.password_hash.is_some() || self.keyfile.is_some() {
            return false;
        }
        #[cfg(feature = "challenge_response")]
        if self.challenge_response_key.is_some() {
            return false;
        }
        true
    }
}

#[derive(Debug, Error)]
pub enum DatabaseKeyError {
    #[error("The key contains no components")]
    EmptyKey,

    #[error("Incorrect key")]
    IncorrectKey,

    #[error("I/O error reading keyfile: {0}")]
    Io(#[from] std::io::Error),

    #[error("XML error reading keyfile: {0}")]
    Xml(#[from] quick_xml::Error),

    #[error("Invalid keyfile format")]
    InvalidKeyFile,

    #[cfg(feature = "challenge_response")]
    #[error("Challenge-response key error: {0}")]
    ChallengeResponse(#[from] crate::key::yubikey::ChallengeResponseKeyError),
}

#[cfg(test)]
mod key_tests {

    use super::{DatabaseKey, DatabaseKeyError};

    #[test]
    fn test_key() -> Result<(), DatabaseKeyError> {
        let ke = DatabaseKey::new().with_password("asdf").get_key_elements()?;
        assert_eq!(ke.len(), 1);

        let ke = DatabaseKey::new()
            .with_keyfile(&mut "bare-key-file".as_bytes())?
            .get_key_elements()?;
        assert_eq!(ke.len(), 1);

        let ke = DatabaseKey::new()
            .with_keyfile(&mut "0123456789ABCDEF0123456789ABCDEF".as_bytes())?
            .get_key_elements()?;
        assert_eq!(ke.len(), 1);

        let ke = DatabaseKey::new()
            .with_password("asdf")
            .with_keyfile(&mut "bare-key-file".as_bytes())?
            .get_key_elements()?;
        assert_eq!(ke.len(), 2);

        let ke = DatabaseKey::new()
            .with_keyfile(
                &mut "<KeyFile><Key><Data>0!23456789ABCDEF0123456789ABCDEF</Data></Key></KeyFile>".as_bytes(),
            )?
            .get_key_elements()?;
        assert_eq!(ke.len(), 1);

        let ke = DatabaseKey::new()
            .with_keyfile(
                &mut "<KeyFile><Key><Data>NXyYiJMHg3ls+eBmjbAjWec9lcOToJiofbhNiFMTJMw=</Data></Key></KeyFile>"
                    .as_bytes(),
            )?
            .get_key_elements()?;
        assert_eq!(ke.len(), 1);

        let xml_keyfile_v2 = r###"
            <?xml version="1.0" encoding="utf-8"?>
            <KeyFile>
                <Meta>
                    <Version>2.0</Version>
                </Meta>
                <Key>
                    <Data Hash="A65F0C2D">
                        36057B1C 35037FD9 62257893 C0A22403
                        EE3F8FBB 504D9981 08B821CB 00D28F89
                    </Data>
                </Key>
            </KeyFile>
        "###;
        let ke = DatabaseKey::new()
            .with_keyfile(&mut xml_keyfile_v2.trim().as_bytes())?
            .get_key_elements()?;
        assert_eq!(ke.len(), 1);

        // other XML files will just be hashed as a "bare" keyfile
        let ke = DatabaseKey::new()
            .with_keyfile(&mut "<Not><A><KeyFile></KeyFile></A></Not>".as_bytes())?
            .get_key_elements()?;

        assert_eq!(ke.len(), 1);

        assert!(DatabaseKey {
            password_hash: None,
            keyfile: None,
            #[cfg(feature = "challenge_response")]
            challenge_response_key: None,
            #[cfg(feature = "challenge_response")]
            challenge_response_result: None,
        }
        .get_key_elements()
        .is_err());

        Ok(())
    }

    /// The password is a credential the user probably reuses; the hash is
    /// not. Keeping the plaintext for the life of an unlocked database made
    /// every memory dump a password disclosure rather than a brute-force
    /// problem, and nothing needed it: KDBX only ever uses the hash.
    #[test]
    fn a_password_is_reduced_to_its_hash_and_never_kept() {
        let key = DatabaseKey::new().with_password("correct horse battery staple");

        let rendered = format!("{key:?}");
        assert!(!rendered.contains("correct horse battery staple"));
        assert!(rendered.contains("has_password: true"));

        let elements = key.get_key_elements().expect("one element");
        assert_eq!(elements.len(), 1);
        assert_eq!(
            elements[0],
            crate::crypt::calculate_sha256(&[b"correct horse battery staple"]).to_vec(),
            "the element is exactly what the format asks for"
        );
    }
}
