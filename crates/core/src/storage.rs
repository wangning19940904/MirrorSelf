//! Storage backends (architecture §4). Adapters store opaque, encrypted,
//! content-addressed blobs; they never see plaintext or metadata.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Serialize;

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Caps {
    /// A headless server can read this backend without the user present.
    pub server_readable: bool,
    /// Safe to read from background jobs (no short-lived user tokens).
    pub background_safe: bool,
}

#[derive(Debug, Default)]
pub struct Page {
    pub items: Vec<String>,
    pub next: Option<String>,
}

pub trait StorageAdapter: Send + Sync {
    fn uri(&self) -> String;
    /// Idempotent: storing an existing hash is a no-op.
    fn put(&self, hash: &str, ciphertext: &[u8]) -> Result<()>;
    fn get(&self, hash: &str) -> Result<Vec<u8>>;
    fn exists(&self, hash: &str) -> Result<bool>;
    fn list(&self, prefix: &str, cursor: Option<&str>) -> Result<Page>;
    /// Used by crypto-delete garbage collection.
    fn delete(&self, hash: &str) -> Result<()>;
    fn capabilities(&self) -> Caps;
}

pub fn open(uri: &str) -> Result<Box<dyn StorageAdapter>> {
    match uri.split_once(':') {
        Some(("local", path)) if !path.is_empty() => Ok(Box::new(LocalStorage::open(path)?)),
        Some((scheme @ ("gdrive" | "s3" | "webdav" | "icloud" | "cloud"), _)) => {
            bail!("storage backend '{scheme}' is planned but not in this build; use local:/path")
        }
        _ => bail!("unrecognised storage URI '{uri}' (expected local:/path)"),
    }
}

const PAGE_SIZE: usize = 1000;

fn is_object_name(name: &str) -> bool {
    name.len() == 64 && name.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Local disk or NAS: `<root>/objects/<first two hex>/<hash>`.
pub struct LocalStorage {
    base: PathBuf,
    objects: PathBuf,
}

impl LocalStorage {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let base = path.as_ref().to_path_buf();
        let objects = base.join("objects");
        fs::create_dir_all(&objects)
            .with_context(|| format!("creating vault directory {}", objects.display()))?;
        Ok(Self { base, objects })
    }

    fn path_for(&self, hash: &str) -> Result<PathBuf> {
        if !is_object_name(hash) {
            bail!("invalid object address '{hash}'");
        }
        Ok(self.objects.join(&hash[..2]).join(hash))
    }
}

impl StorageAdapter for LocalStorage {
    fn uri(&self) -> String {
        format!("local:{}", self.base.display())
    }

    fn put(&self, hash: &str, ciphertext: &[u8]) -> Result<()> {
        let path = self.path_for(hash)?;
        if path.exists() {
            return Ok(());
        }
        let dir = path.parent().expect("object paths have a shard directory");
        fs::create_dir_all(dir)?;
        let tmp = dir.join(format!("{hash}.tmp"));
        {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(ciphertext)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    fn get(&self, hash: &str) -> Result<Vec<u8>> {
        let path = self.path_for(hash)?;
        fs::read(&path).with_context(|| format!("vault object {hash} is missing"))
    }

    fn exists(&self, hash: &str) -> Result<bool> {
        Ok(self.path_for(hash)?.exists())
    }

    fn list(&self, prefix: &str, cursor: Option<&str>) -> Result<Page> {
        let mut names = Vec::new();
        for shard in fs::read_dir(&self.objects)? {
            let shard = shard?;
            if !shard.file_type()?.is_dir() {
                continue;
            }
            for entry in fs::read_dir(shard.path())? {
                let name = entry?.file_name().to_string_lossy().into_owned();
                if is_object_name(&name)
                    && name.starts_with(prefix)
                    && cursor.is_none_or(|c| name.as_str() > c)
                {
                    names.push(name);
                }
            }
        }
        names.sort();
        let next = if names.len() > PAGE_SIZE {
            names.truncate(PAGE_SIZE);
            names.last().cloned()
        } else {
            None
        };
        Ok(Page { items: names, next })
    }

    fn delete(&self, hash: &str) -> Result<()> {
        let path = self.path_for(hash)?;
        if path.exists() {
            fs::remove_file(path)?;
        }
        Ok(())
    }

    fn capabilities(&self) -> Caps {
        Caps {
            server_readable: true,
            background_safe: true,
        }
    }
}
