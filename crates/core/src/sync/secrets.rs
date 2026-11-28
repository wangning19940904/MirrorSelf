//! Credentials for sync recipes: bearer tokens, header values and
//! environment variables a server or program needs.
//!
//! They live in `<home>/sync-secrets.json`, outside `mirrorself.toml`, each
//! sealed with AES-256-GCM under a key derived from the vault key, with the
//! secret's name as associated data so a value cannot be moved to another
//! name. The file holds no plaintext. Values are never printed or logged;
//! [`Redactor`] scrubs them from error messages.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{anyhow, bail, Context, Result};
use hkdf::Hkdf;
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::config::Home;
use crate::crypto::{random_bytes, SubKeys};
use crate::error::{invalid, not_found};
use crate::sync::recipe::check_secret_name;

const FILE_NAME: &str = "sync-secrets.json";
const INFO: &[u8] = b"mirrorself/v1/sync-secrets";
const MAX_SECRET_BYTES: usize = 16 * 1024;

#[derive(Default, Serialize, Deserialize)]
struct SecretFile {
    version: u32,
    /// name → hex(nonce | ciphertext).
    secrets: BTreeMap<String, String>,
}

pub struct SecretStore {
    path: PathBuf,
    key: [u8; 32],
}

/// A secret value that does not print itself.
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(..)")
    }
}

impl SecretStore {
    pub fn path_for(home: &Home) -> PathBuf {
        home.root().join(FILE_NAME)
    }

    /// The key is derived from the index subkey with its own HKDF label, so
    /// it changes with the vault key and is never stored.
    pub fn open(home: &Home, keys: &SubKeys) -> Self {
        let ikm = hex::decode(keys.index_key_hex()).expect("index key is hex");
        let hk = Hkdf::<Sha256>::new(Some(INFO), &ikm);
        let mut key = [0u8; 32];
        hk.expand(INFO, &mut key).expect("32 bytes is a valid HKDF output length");
        Self { path: Self::path_for(home), key }
    }

    fn load(&self) -> Result<SecretFile> {
        match fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| format!("reading {}", self.path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(SecretFile { version: 1, ..Default::default() }),
            Err(e) => Err(e).with_context(|| format!("reading {}", self.path.display())),
        }
    }

    fn save(&self, file: &SecretFile) -> Result<()> {
        let tmp = self.path.with_extension("json.tmp");
        write_private(&tmp, &serde_json::to_vec_pretty(file)?)?;
        fs::rename(&tmp, &self.path).with_context(|| format!("writing {}", self.path.display()))
    }

    pub fn set(&self, name: &str, value: &str) -> Result<()> {
        check_secret_name(name).map_err(|e| invalid(e.to_string()))?;
        if value.is_empty() {
            return Err(invalid("the secret is empty"));
        }
        if value.len() > MAX_SECRET_BYTES || value.chars().any(|c| c.is_control()) {
            return Err(invalid("the secret must be one line of at most 16 KiB without control characters"));
        }
        let nonce: [u8; 12] = random_bytes();
        let cipher = Aes256Gcm::new_from_slice(&self.key).expect("32-byte key");
        let ct = cipher
            .encrypt(Nonce::from_slice(&nonce), Payload { msg: value.as_bytes(), aad: aad(name).as_bytes() })
            .map_err(|_| anyhow!("encrypting the secret failed"))?;
        let mut file = self.load()?;
        file.version = 1;
        let mut blob = nonce.to_vec();
        blob.extend_from_slice(&ct);
        file.secrets.insert(name.to_string(), hex::encode(blob));
        self.save(&file)
    }

    pub fn get(&self, name: &str) -> Result<Secret> {
        let file = self.load()?;
        let Some(hex_blob) = file.secrets.get(name) else {
            return Err(not_found(format!(
                "secret '{name}' is not set; run `mirrorself sync secret set {name}` and paste it on stdin"
            )));
        };
        let blob = hex::decode(hex_blob).context("secret file is corrupted")?;
        if blob.len() < 12 + 16 {
            bail!("secret '{name}' is corrupted");
        }
        let (nonce, ct) = blob.split_at(12);
        let cipher = Aes256Gcm::new_from_slice(&self.key).expect("32-byte key");
        let plain = cipher
            .decrypt(Nonce::from_slice(nonce), Payload { msg: ct, aad: aad(name).as_bytes() })
            .map_err(|_| anyhow!("secret '{name}' cannot be decrypted: wrong vault key or corrupted file"))?;
        Ok(Secret(String::from_utf8(plain).context("secret is not UTF-8")?))
    }

    pub fn remove(&self, name: &str) -> Result<bool> {
        let mut file = self.load()?;
        let removed = file.secrets.remove(name).is_some();
        if removed {
            self.save(&file)?;
        }
        Ok(removed)
    }

    pub fn names(&self) -> Result<Vec<String>> {
        Ok(self.load()?.secrets.into_keys().collect())
    }
}

fn aad(name: &str) -> String {
    format!("mirrorself/sync-secret/v1:{name}")
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path).with_context(|| format!("writing {}", path.display()))?;
    std::io::Write::write_all(&mut f, bytes)?;
    f.sync_all()?;
    Ok(())
}

/// Replaces `${secret:name}` references with stored values.
pub fn substitute(s: &str, secrets: &BTreeMap<String, Secret>) -> Result<String> {
    let mut out = s.to_string();
    for name in crate::sync::recipe::secret_refs(s) {
        let v = secrets.get(&name).ok_or_else(|| anyhow!("secret '{name}' was not loaded"))?;
        out = out.replace(&format!("${{secret:{name}}}"), v.expose());
    }
    Ok(out)
}

/// Scrubs known secret values from text before it is stored or shown.
#[derive(Default, Clone)]
pub struct Redactor(Vec<String>);

impl Redactor {
    pub fn new(secrets: &BTreeMap<String, Secret>) -> Self {
        Self(secrets.values().map(|s| s.0.clone()).filter(|s| s.len() >= 4).collect())
    }

    pub fn scrub(&self, text: &str) -> String {
        let mut out = text.to_string();
        for s in &self.0 {
            out = out.replace(s.as_str(), "[secret]");
        }
        out
    }
}
