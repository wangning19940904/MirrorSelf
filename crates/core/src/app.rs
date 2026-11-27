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
use crate::pipeline::{self, ProcessReport};
use crate::query::QueryService;
use crate::sources::{self, ImportSpec};
use crate::storage;
use crate::time::now_utc_precise;
use crate::vault::{RawMeta, Vault};
use crate::db;

/// Source name that asks [`App::import_bytes`] to detect the format.
pub const AUTO_SOURCE: &str = "auto";

/// How a raw object is labelled in the vault. The label is permanent.
#[derive(Debug, Clone)]
pub struct RawInput {
    /// `raw_objects.source`, e.g. `apple_health`.
    pub source: String,
    /// `raw_objects.kind`; selects the processors that run on the object.
    pub kind: String,
    pub mime: String,
    /// File name, if it came from a file.
    pub name: Option<String>,
    /// Where it came from: a file name, or a sync recipe and its window.
    pub origin_ref: Option<String>,
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
    /// How the object was labelled (for bytes already in the vault, how this
    /// call would have labelled them).
    pub source: String,
    pub kind: String,
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

    /// Imports a file as `source` (an import name from the source registry,
    /// e.g. `apple-health`, or `auto`).
    pub fn import_file(&self, source: &str, path: &Path, process: bool) -> Result<ImportReport> {
        let content = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
        self.import_bytes(source, name, &content, process)
    }

    /// Checks that the bytes are what `source` expects, stores them in the
    /// vault (once), registers them, and optionally runs the pipeline.
    pub fn import_bytes(
        &self,
        source: &str,
        name: Option<String>,
        content: &[u8],
        process: bool,
    ) -> Result<ImportReport> {
        if source == AUTO_SOURCE {
            return self.import_auto(name, content, process);
        }
        let Some(spec) = sources::import_spec(source) else {
            let known: Vec<&str> = sources::import_specs().map(|s| s.name).collect();
            return Err(invalid(format!(
                "unknown source '{source}'. Known sources: {}, {AUTO_SOURCE}",
                known.join(", ")
            )));
        };
        let Some(mime) = (spec.detect)(name.as_deref(), content) else {
            return Err(invalid(format!("this is not {}", spec.expects)));
        };
        self.ingest(labelled(spec, mime, name), content, process)
    }

    /// Imports bytes of unknown origin, as an upload does: the first source
    /// that recognises them wins, and anything else is kept as a generic
    /// file (`file_pdf`, `file_image`, `file_text` or `file_other`).
    pub fn import_auto(&self, name: Option<String>, content: &[u8], process: bool) -> Result<ImportReport> {
        let recognised = sources::import_specs()
            .find_map(|spec| (spec.detect)(name.as_deref(), content).map(|mime| (spec, mime)));
        let input = match recognised {
            Some((spec, mime)) => labelled(spec, mime, name),
            None => {
                let (kind, mime) = sources::generic_file_kind(name.as_deref(), content);
                RawInput {
                    source: sources::SOURCE_FILE.into(),
                    kind: kind.into(),
                    mime: mime.into(),
                    name: name.clone(),
                    origin_ref: name,
                }
            }
        };
        self.ingest(input, content, process)
    }

    /// Stores already-labelled bytes: the entry point for anything that is
    /// not a user-supplied file, such as a sync response. Nothing checks
    /// that the bytes match the label.
    pub fn ingest(&self, input: RawInput, content: &[u8], process: bool) -> Result<ImportReport> {
        let meta = RawMeta {
            source: input.source,
            kind: input.kind,
            mime: input.mime,
            name: input.name,
            origin_ref: input.origin_ref,
            received_at: now_utc_precise(),
            bytes: content.len() as u64,
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
            source: meta.source,
            kind: meta.kind,
            process,
        })
    }

    pub fn process(&self, rebuild: bool) -> Result<ProcessReport> {
        pipeline::process(&self.conn, &self.vault, &self.config, rebuild)
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

fn labelled(spec: &ImportSpec, mime: &str, name: Option<String>) -> RawInput {
    RawInput {
        source: spec.source.into(),
        kind: spec.kind.into(),
        mime: mime.into(),
        name: name.clone(),
        origin_ref: name,
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
