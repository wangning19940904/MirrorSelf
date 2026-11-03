//! `App`: one opened MirrorSelf home (config, vault, index).

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};
use serde::Serialize;
use serde_json::{json, Value};

use crate::config::{Config, Home};
use crate::crypto::{random_bytes, VaultKey};
use crate::error::{conflict, invalid, not_found, UserError};
use crate::importers::apple_health;
use crate::pipeline::{self, ProcessReport};
use crate::query::QueryService;
use crate::storage;
use crate::time::now_utc_precise;
use crate::vault::{RawMeta, Vault};
use crate::db;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportKind {
    AppleHealth,
}

#[derive(Debug, Serialize)]
pub struct InitReport {
    pub home: String,
    pub storage: String,
    pub key_file: String,
    pub token_file: String,
    pub key_id: String,
}

#[derive(Debug, Serialize)]
pub struct ImportReport {
    pub raw_id: String,
    /// False when the vault already held identical bytes.
    pub created: bool,
    pub bytes: u64,
    pub process: Option<ProcessReport>,
}

pub struct App {
    home: Home,
    config: Config,
    vault: Vault,
    conn: Connection,
}

impl App {
    /// Creates a home: vault key, API token, config, vault directory, index.
    pub fn init(home: &Home, mode: &str, storage_uri: Option<&str>) -> Result<InitReport> {
        if home.is_initialized() {
            return Err(conflict(format!("{} is already initialized", home.root().display())));
        }
        if mode != "server" {
            return Err(invalid(
                "only --mode server is available in this build (device and cloud modes come later)",
            ));
        }
        create_private_dir(home.root())?;
        let storage_uri = match storage_uri {
            Some(s) => s.to_string(),
            None => format!("local:{}", home.default_vault_dir().display()),
        };
        let adapter = storage::open(&storage_uri)?;
        let key = VaultKey::generate();
        write_secret(&home.key_path(), &key.to_hex())?;
        write_secret(&home.token_path(), &hex::encode(random_bytes::<32>()))?;
        Config::new(mode, &storage_uri).save(home)?;
        let vault = Vault::new(adapter, &key);
        db::open(&home.index_path(), vault.subkeys())?;
        Ok(InitReport {
            home: home.root().display().to_string(),
            storage: storage_uri,
            key_file: home.key_path().display().to_string(),
            token_file: home.token_path().display().to_string(),
            key_id: key.id(),
        })
    }

    pub fn open(home: Home) -> Result<Self> {
        if !home.is_initialized() {
            return Err(UserError::NotInitialized(home.root().to_path_buf()).into());
        }
        let config = Config::load(&home)?;
        let key_hex = fs::read_to_string(home.key_path())
            .with_context(|| format!("reading {}", home.key_path().display()))?;
        let key = VaultKey::from_hex(&key_hex)?;
        let vault = Vault::new(storage::open(&config.storage)?, &key);
        let conn = db::open(&home.index_path(), vault.subkeys())?;
        Ok(Self { home, config, vault, conn })
    }

    pub fn home(&self) -> &Home {
        &self.home
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn vault(&self) -> &Vault {
        &self.vault
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    pub fn query(&self) -> QueryService<'_> {
        QueryService::new(&self.conn)
    }

    pub fn api_token(&self) -> Result<String> {
        Ok(fs::read_to_string(self.home.token_path())
            .context("reading the API token")?
            .trim()
            .to_string())
    }

    pub fn import_file(&self, kind: ImportKind, path: &Path, process: bool) -> Result<ImportReport> {
        let content = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
        self.import_bytes(kind, name, &content, process)
    }

    /// Stores the bytes in the vault (once), registers them, and optionally
    /// runs the pipeline.
    pub fn import_bytes(
        &self,
        kind: ImportKind,
        name: Option<String>,
        content: &[u8],
        process: bool,
    ) -> Result<ImportReport> {
        let meta = match kind {
            ImportKind::AppleHealth => {
                if !apple_health::looks_like_export(content) {
                    return Err(invalid(
                        "not an Apple Health export (expected export.zip or export.xml from the Health app)",
                    ));
                }
                let zipped = content.starts_with(b"PK\x03\x04");
                RawMeta {
                    source: apple_health::SOURCE.into(),
                    kind: apple_health::KIND.into(),
                    mime: if zipped { "application/zip" } else { "application/xml" }.into(),
                    name: name.clone(),
                    origin_ref: name,
                    received_at: now_utc_precise(),
                    bytes: content.len() as u64,
                }
            }
        };
        let put = self.vault.put(&meta, content)?;
        if put.created {
            pipeline::insert_raw(&self.conn, &self.vault, &put.id, &meta)?;
        }
        let process = if process { Some(self.process(false)?) } else { None };
        Ok(ImportReport {
            raw_id: put.id,
            created: put.created,
            bytes: meta.bytes,
            process,
        })
    }

    pub fn process(&self, rebuild: bool) -> Result<ProcessReport> {
        pipeline::process(&self.conn, &self.vault, &self.config.pipeline, rebuild)
    }

    pub fn raw_list(&self) -> Result<Value> {
        let rows = self
            .conn
            .prepare(
                "SELECT r.id, r.source, r.kind, r.mime, r.name, r.received_at, r.bytes,
                        (SELECT status FROM processor_runs p WHERE p.raw_id = r.id ORDER BY finished_at DESC LIMIT 1)
                 FROM raw_objects r ORDER BY r.received_at, r.id",
            )?
            .query_map([], |r| {
                Ok(json!({
                    "id": r.get::<_, String>(0)?, "source": r.get::<_, String>(1)?,
                    "kind": r.get::<_, String>(2)?, "mime": r.get::<_, String>(3)?,
                    "name": r.get::<_, Option<String>>(4)?, "received_at": r.get::<_, String>(5)?,
                    "bytes": r.get::<_, i64>(6)?, "last_run": r.get::<_, Option<String>>(7)?,
                }))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(json!({ "count": rows.len(), "items": rows }))
    }

    /// The original bytes of a raw object, decrypted and verified.
    pub fn raw_original(&self, id: &str) -> Result<(RawMeta, Vec<u8>)> {
        let known: Option<String> = self
            .conn
            .query_row("SELECT id FROM raw_objects WHERE id = ?1", [id], |r| r.get(0))
            .optional()?;
        if known.is_none() {
            return Err(not_found(format!("no raw object '{id}'")));
        }
        self.vault.get(id)
    }
}

fn create_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("creating {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn write_secret(path: &Path, contents: &str) -> Result<()> {
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    std::io::Write::write_all(&mut f, format!("{contents}\n").as_bytes())?;
    Ok(())
}
