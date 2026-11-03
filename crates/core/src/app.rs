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
use crate::importers::{apple_health, report};
use crate::models::{self, DocumentInput, ModelProvider, OcrDocument};
use crate::ops::{self, Edit};
use crate::pipeline::{self, ProcessReport};
use crate::query::QueryService;
use crate::storage;
use crate::time::now_utc_precise;
use crate::vault::{RawMeta, Vault};
use crate::db;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportKind {
    AppleHealth,
    /// A checkup report or lab sheet (PDF or photo), with a language hint.
    Report(Option<Lang>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Zh,
    En,
}

impl Lang {
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "zh" | "zh-Hans" | "zh-CN" => Ok(Lang::Zh),
            "en" | "en-US" | "en-GB" => Ok(Lang::En),
            other => Err(invalid(format!("language '{other}' must be zh or en"))),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Lang::Zh => "zh",
            Lang::En => "en",
        }
    }
}

/// A review decision on one extracted lab value.
#[derive(Debug, Clone)]
pub enum Decision {
    Confirm,
    Edit(Edit),
    Reject { reason: Option<String> },
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
    models: Box<dyn ModelProvider>,
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
        let mut config = Config::load(&home)?;
        if config.node_id.is_none() {
            config.node_id = Some(hex::encode(random_bytes::<4>()));
            config.save(&home)?;
        }
        let models = models::from_config(&config.models)?;
        let key_hex = fs::read_to_string(home.key_path())
            .with_context(|| format!("reading {}", home.key_path().display()))?;
        let key = VaultKey::from_hex(&key_hex)?;
        let vault = Vault::new(storage::open(&config.storage)?, &key);
        let conn = db::open(&home.index_path(), vault.subkeys())?;
        Ok(Self { home, config, vault, conn, models })
    }

    /// Replaces the OCR and extraction provider chosen from the config.
    pub fn set_model_provider(&mut self, models: Box<dyn ModelProvider>) {
        self.models = models;
    }

    pub fn model_provider(&self) -> &dyn ModelProvider {
        self.models.as_ref()
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
                    lang: None,
                }
            }
            ImportKind::Report(lang) => {
                let mime = report::sniff(content).ok_or_else(|| {
                    invalid("not a supported report file (expected a PDF, JPEG, PNG, HEIC or TIFF)")
                })?;
                RawMeta {
                    source: report::SOURCE.into(),
                    kind: report::KIND.into(),
                    mime: mime.into(),
                    name: name.clone(),
                    origin_ref: name,
                    received_at: now_utc_precise(),
                    bytes: content.len() as u64,
                    lang: lang.map(|l| l.as_str().to_string()),
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
        pipeline::process(&self.conn, &self.vault, &self.config.pipeline, self.models.as_ref(), rebuild)
    }

    /// Runs the configured provider's OCR on a file without importing it
    /// (`mirrorself dev ocr`, used to record fixtures).
    pub fn ocr_bytes(&self, content: &[u8], lang: Option<Lang>) -> Result<OcrDocument> {
        let mime = report::sniff(content).ok_or_else(|| invalid("not a supported report file"))?;
        self.models.ocr(&DocumentInput { mime, bytes: content, lang: lang.map(Lang::as_str) })
    }

    /// Records a review decision as a user op (in the vault and the op log)
    /// and applies it. Returns the updated review item.
    pub fn review(&self, id: &str, decision: Decision, actor: &str) -> Result<Value> {
        let row: Option<(String, Option<i64>, Option<String>, String)> = self
            .conn
            .query_row(
                "SELECT raw_id, page, label, status FROM observations
                 WHERE id = ?1 AND code_system IN ('loinc', 'local')",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let Some((raw_id, page, label, status)) = row else {
            return Err(not_found(format!("no extracted lab value '{id}' (see `mirrorself review list`)")));
        };
        if matches!(status.as_str(), "superseded" | "duplicate") {
            return Err(conflict(format!("lab value '{id}' is {status} and cannot be reviewed")));
        }
        let matcher = json!({ "raw_id": raw_id, "page": page, "label": label });
        let (op, payload) = match decision {
            Decision::Confirm => (ops::CONFIRM, json!({ "match": matcher })),
            Decision::Edit(edit) => {
                ops::check_edit(&edit).map_err(invalid)?;
                (ops::EDIT, json!({ "match": matcher, "edit": edit }))
            }
            Decision::Reject { reason } => (ops::REJECT, json!({ "match": matcher, "reason": reason })),
        };
        let node = self.config.node_id.as_deref().unwrap_or("local");
        let (op_id, _) = ops::append(&self.conn, &self.vault, node, actor, op, id, payload)?;
        let mut item = self.query().review_item(id)?;
        item["op_id"] = json!(op_id);
        Ok(item)
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
