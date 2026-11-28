//! The raw vault: immutable, encrypted, content-addressed originals.
//!
//! Each object's plaintext is `u32 LE meta length | meta JSON | content`, so
//! the index can be rebuilt from the vault alone (`process --rebuild` after
//! deleting `index.db`). The address covers the content only, so importing
//! the same file twice stores it once.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::crypto::{SubKeys, VaultKey};
use crate::storage::StorageAdapter;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RawMeta {
    pub source: String,
    pub kind: String,
    pub mime: String,
    pub name: Option<String>,
    pub origin_ref: Option<String>,
    pub received_at: String,
    pub bytes: u64,
    /// Language hint for documents (`import report --lang`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lang: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PutResult {
    pub id: String,
    pub created: bool,
}

pub struct Vault {
    storage: Box<dyn StorageAdapter>,
    keys: SubKeys,
    key_id: String,
}

impl Vault {
    pub fn new(storage: Box<dyn StorageAdapter>, key: &VaultKey) -> Self {
        Self {
            storage,
            keys: key.subkeys(),
            key_id: key.id(),
        }
    }

    pub fn storage_uri(&self) -> String {
        self.storage.uri()
    }

    pub fn key_id(&self) -> &str {
        &self.key_id
    }

    pub fn subkeys(&self) -> &SubKeys {
        &self.keys
    }

    pub fn address(&self, content: &[u8]) -> String {
        self.keys.content_address(content)
    }

    pub fn put(&self, meta: &RawMeta, content: &[u8]) -> Result<PutResult> {
        let id = self.address(content);
        if self.storage.exists(&id)? {
            return Ok(PutResult { id, created: false });
        }
        let meta_json = serde_json::to_vec(meta)?;
        let meta_len = u32::try_from(meta_json.len()).context("metadata too large")?;
        let mut plain = Vec::with_capacity(4 + meta_json.len() + content.len());
        plain.extend_from_slice(&meta_len.to_le_bytes());
        plain.extend_from_slice(&meta_json);
        plain.extend_from_slice(content);
        let blob = self.keys.seal(&plain)?;
        self.storage.put(&id, &blob)?;
        Ok(PutResult { id, created: true })
    }

    /// Decrypts an object and checks it still matches its address.
    pub fn get(&self, id: &str) -> Result<(RawMeta, Vec<u8>)> {
        let blob = self.storage.get(id)?;
        let mut plain = self
            .keys
            .open(&blob)
            .with_context(|| format!("opening vault object {id}"))?;
        if plain.len() < 4 {
            bail!("vault object {id} is truncated");
        }
        let meta_len = u32::from_le_bytes(plain[..4].try_into().expect("4 bytes")) as usize;
        if plain.len() < 4 + meta_len {
            bail!("vault object {id} is truncated");
        }
        let meta: RawMeta = serde_json::from_slice(&plain[4..4 + meta_len])
            .with_context(|| format!("vault object {id} has unreadable metadata"))?;
        let content = plain.split_off(4 + meta_len);
        if self.address(&content) != id {
            bail!("vault object {id} does not match its address");
        }
        Ok((meta, content))
    }

    pub fn list_ids(&self) -> Result<Vec<String>> {
        let mut ids = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = self.storage.list("", cursor.as_deref())?;
            ids.extend(page.items);
            match page.next {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        Ok(ids)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::LocalStorage;

    fn meta() -> RawMeta {
        RawMeta {
            source: "test".into(),
            kind: "blob".into(),
            mime: "text/plain".into(),
            name: Some("a.txt".into()),
            origin_ref: None,
            received_at: "2026-11-01T00:00:00Z".into(),
            bytes: 5,
            lang: None,
        }
    }

    #[test]
    fn put_is_idempotent_and_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::new(
            Box::new(LocalStorage::open(dir.path()).unwrap()),
            &VaultKey::generate(),
        );
        let first = vault.put(&meta(), b"hello").unwrap();
        let second = vault.put(&meta(), b"hello").unwrap();
        assert!(first.created);
        assert!(!second.created);
        assert_eq!(first.id, second.id);
        assert_eq!(vault.list_ids().unwrap(), vec![first.id.clone()]);

        let (m, content) = vault.get(&first.id).unwrap();
        assert_eq!(m, meta());
        assert_eq!(content, b"hello");

        // The stored bytes are ciphertext, not the original.
        let shard = dir.path().join("objects").join(&first.id[..2]).join(&first.id);
        let stored = std::fs::read(shard).unwrap();
        assert!(!stored.windows(5).any(|w| w == b"hello"));
    }
}
