//! Vault encryption (architecture §4).
//!
//! Every raw object gets its own AES-256-GCM data key, wrapped by a key
//! derived from the user's vault key (envelope encryption). Objects are
//! addressed by HMAC-SHA256 of their plaintext under another derived key, so
//! the vault deduplicates while a storage provider cannot confirm what a file
//! contains. The index database key is derived the same way.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{anyhow, bail, Context, Result};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

const MAGIC: &[u8; 4] = b"MSV1";
const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;
const WRAPPED_LEN: usize = 32 + TAG_LEN;
const HEADER_LEN: usize = MAGIC.len() + NONCE_LEN + WRAPPED_LEN + NONCE_LEN;

pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    getrandom::getrandom(&mut out).expect("the OS random source is available");
    out
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// The user's root secret. Lives in `vault.key` (mode B); never leaves the host.
#[derive(Clone)]
pub struct VaultKey([u8; 32]);

impl std::fmt::Debug for VaultKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("VaultKey(..)")
    }
}

impl VaultKey {
    pub fn generate() -> Self {
        Self(random_bytes())
    }

    pub fn from_hex(text: &str) -> Result<Self> {
        let bytes = hex::decode(text.trim()).context("vault key is not hex")?;
        let key: [u8; 32] = bytes
            .try_into()
            .map_err(|_| anyhow!("vault key must be 32 bytes"))?;
        Ok(Self(key))
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// A short, non-secret identifier stored with each raw object.
    pub fn id(&self) -> String {
        format!("vk1-{}", &hex::encode(self.derive(b"mirrorself/v1/key-id"))[..8])
    }

    pub fn subkeys(&self) -> SubKeys {
        SubKeys {
            address: self.derive(b"mirrorself/v1/content-address"),
            wrap: self.derive(b"mirrorself/v1/key-wrap"),
            index: self.derive(b"mirrorself/v1/index-db"),
        }
    }

    fn derive(&self, info: &[u8]) -> [u8; 32] {
        let hk = Hkdf::<Sha256>::new(None, &self.0);
        let mut out = [0u8; 32];
        hk.expand(info, &mut out)
            .expect("32 bytes is a valid HKDF-SHA256 output length");
        out
    }
}

#[derive(Clone)]
pub struct SubKeys {
    address: [u8; 32],
    wrap: [u8; 32],
    index: [u8; 32],
}

impl SubKeys {
    /// Keyed content address: lowercase hex HMAC-SHA256 of the plaintext.
    pub fn content_address(&self, data: &[u8]) -> String {
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&self.address)
            .expect("HMAC accepts any key length");
        mac.update(data);
        hex::encode(mac.finalize().into_bytes())
    }

    /// Raw SQLCipher key for the index database.
    pub fn index_key_hex(&self) -> String {
        hex::encode(self.index)
    }

    /// `MSV1 | wrap nonce | wrapped data key | data nonce | ciphertext`.
    pub fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>> {
        let data_key: [u8; 32] = random_bytes();
        let wrap_nonce: [u8; NONCE_LEN] = random_bytes();
        let data_nonce: [u8; NONCE_LEN] = random_bytes();

        let wrapper = Aes256Gcm::new_from_slice(&self.wrap).expect("32-byte key");
        let wrapped = wrapper
            .encrypt(
                Nonce::from_slice(&wrap_nonce),
                Payload {
                    msg: &data_key,
                    aad: MAGIC,
                },
            )
            .map_err(|_| anyhow!("wrapping the data key failed"))?;
        let cipher = Aes256Gcm::new_from_slice(&data_key).expect("32-byte key");
        let ciphertext = cipher
            .encrypt(
                Nonce::from_slice(&data_nonce),
                Payload {
                    msg: plaintext,
                    aad: MAGIC,
                },
            )
            .map_err(|_| anyhow!("encrypting the object failed"))?;

        let mut out = Vec::with_capacity(HEADER_LEN + ciphertext.len());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&wrap_nonce);
        out.extend_from_slice(&wrapped);
        out.extend_from_slice(&data_nonce);
        out.extend_from_slice(&ciphertext);
        Ok(out)
    }

    pub fn open(&self, blob: &[u8]) -> Result<Vec<u8>> {
        if blob.len() < HEADER_LEN + TAG_LEN || &blob[..MAGIC.len()] != MAGIC {
            bail!("not a MirrorSelf vault object");
        }
        let rest = &blob[MAGIC.len()..];
        let (wrap_nonce, rest) = rest.split_at(NONCE_LEN);
        let (wrapped, rest) = rest.split_at(WRAPPED_LEN);
        let (data_nonce, ciphertext) = rest.split_at(NONCE_LEN);

        let wrapper = Aes256Gcm::new_from_slice(&self.wrap).expect("32-byte key");
        let data_key = wrapper
            .decrypt(
                Nonce::from_slice(wrap_nonce),
                Payload {
                    msg: wrapped,
                    aad: MAGIC,
                },
            )
            .map_err(|_| anyhow!("cannot unwrap the data key: wrong vault key or corrupted object"))?;
        let cipher = Aes256Gcm::new_from_slice(&data_key)
            .map_err(|_| anyhow!("corrupted data key"))?;
        cipher
            .decrypt(
                Nonce::from_slice(data_nonce),
                Payload {
                    msg: ciphertext,
                    aad: MAGIC,
                },
            )
            .map_err(|_| anyhow!("object failed authentication: corrupted or tampered"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_open_roundtrip_and_tamper_detection() {
        let keys = VaultKey::generate().subkeys();
        let blob = keys.seal(b"hello vault").unwrap();
        assert_eq!(keys.open(&blob).unwrap(), b"hello vault");

        let mut tampered = blob.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 1;
        assert!(keys.open(&tampered).is_err());

        let other = VaultKey::generate().subkeys();
        assert!(other.open(&blob).is_err());
    }

    #[test]
    fn content_address_is_keyed() {
        let a = VaultKey::generate().subkeys();
        let b = VaultKey::generate().subkeys();
        assert_eq!(a.content_address(b"x"), a.content_address(b"x"));
        assert_ne!(a.content_address(b"x"), b.content_address(b"x"));
        assert_eq!(a.content_address(b"x").len(), 64);
    }
}
