//! The source registry: everything MirrorSelf knows how to ingest.
//!
//! A source module describes itself with one [`Source`] value: how its files
//! are recognised on import, which processors turn its raw objects into
//! records, which whole-index stages it adds, and which tables it owns. The
//! pipeline, the importer and the CLI all read [`SOURCES`], so adding a
//! source means writing its module and nothing else.
//!
//! See `docs/demo-plan.md` for the contracts between sources.

use anyhow::Result;
use rusqlite::{params, Connection};
use serde_json::Value;

use crate::config::Config;
use crate::ids::stable_id;
use crate::pipeline::Processor;
use crate::vault::{RawMeta, Vault};

/// How one kind of file is recognised and labelled when it enters the vault.
pub struct ImportSpec {
    /// Name used on the command line and in the API, e.g. `apple-health`.
    pub name: &'static str,
    /// `raw_objects.source`.
    pub source: &'static str,
    /// `raw_objects.kind`; selects the processors that run on the object.
    pub kind: &'static str,
    /// What the user should supply, completing "expected …" in error messages.
    pub expects: &'static str,
    /// Returns the MIME type when `content` is this source's format. Must be
    /// strict enough for auto-detection: a wrong match files the object
    /// under the wrong kind for good, since raw objects are immutable.
    pub detect: fn(name: Option<&str>, content: &[u8]) -> Option<&'static str>,
}

/// What a per-object processor is given besides the bytes.
pub struct RawCtx<'a> {
    pub config: &'a Config,
    /// For processors that read or store other raw objects (for example a
    /// cached model response). Derived tables go through the connection.
    pub vault: &'a Vault,
    pub raw_id: &'a str,
    pub meta: &'a RawMeta,
    pub run_id: &'a str,
}

/// Runs once per raw object of the listed kinds and per processor version.
pub struct RawProcessor {
    pub processor: Processor,
    pub kinds: &'static [&'static str],
    /// Writes derived records for one raw object and returns counters for
    /// `processor_runs.outputs`. Runs inside a transaction that is rolled
    /// back if it fails. Must be deterministic for the same bytes.
    pub load: fn(conn: &Connection, ctx: &RawCtx<'_>, content: &[u8]) -> Result<Value>,
    /// Deletes everything earlier runs of this processor wrote, before its
    /// objects are reprocessed by a new version. [`clear_standard`] covers
    /// `observations`, `events` and `items`; processors with their own
    /// tables call it and then clear those.
    pub clear: fn(conn: &Connection, processor: &str) -> Result<()>,
}

/// What a whole-index stage is given.
pub struct GlobalCtx<'a> {
    pub config: &'a Config,
    pub vault: &'a Vault,
    pub run_id: &'a str,
}

/// Runs over the whole index after any per-object output changed. A stage
/// must leave the same rows however many times it runs.
pub struct GlobalStage {
    pub processor: Processor,
    pub run: fn(conn: &Connection, ctx: &GlobalCtx<'_>) -> Result<Value>,
}

pub struct Source {
    pub name: &'static str,
    pub imports: &'static [ImportSpec],
    pub raw: &'static [RawProcessor],
    /// After cross-device de-duplication, before daily metrics. For stages
    /// that add or adjust `observations`.
    pub pre_derive: &'static [GlobalStage],
    /// After the built-in link rules have rebuilt `entities` and rule links,
    /// before the search index. For stages that add entities and links.
    pub post_link: &'static [GlobalStage],
    /// After the built-in search index was rebuilt. For stages that add
    /// their own rows to `search_fts`.
    pub post_index: &'static [GlobalStage],
    /// Tables this source owns, emptied by `process --rebuild`.
    pub derived_tables: &'static [&'static str],
}

impl Source {
    /// A source that registers nothing; fill in fields with struct update
    /// syntax: `Source { raw: &[…], ..Source::empty("name") }`.
    pub const fn empty(name: &'static str) -> Self {
        Self {
            name,
            imports: &[],
            raw: &[],
            pre_derive: &[],
            post_link: &[],
            post_index: &[],
            derived_tables: &[],
        }
    }
}

/// Every source, in the order their processors run.
pub static SOURCES: &[&Source] = &[
    &crate::pipeline::apple_health::SOURCE,
    &crate::pipeline::alipay::SOURCE,
    &crate::pipeline::feishu_calendar::SOURCE,
    &crate::pipeline::files::SOURCE,
    &crate::sync::SOURCE,
];

pub fn import_specs() -> impl Iterator<Item = &'static ImportSpec> {
    SOURCES.iter().flat_map(|s| s.imports.iter())
}

pub fn import_spec(name: &str) -> Option<&'static ImportSpec> {
    import_specs().find(|s| s.name == name)
}

/// Raw kinds for files no source recognised.
pub const KIND_FILE_PDF: &str = "file_pdf";
pub const KIND_FILE_IMAGE: &str = "file_image";
pub const KIND_FILE_TEXT: &str = "file_text";
pub const KIND_FILE_OTHER: &str = "file_other";
/// `raw_objects.source` for those files.
pub const SOURCE_FILE: &str = "file";

/// Classifies a file by its leading bytes, falling back to its extension for
/// text formats. Returns `(kind, mime)`.
pub fn generic_file_kind(name: Option<&str>, content: &[u8]) -> (&'static str, &'static str) {
    if content.starts_with(b"%PDF-") {
        return (KIND_FILE_PDF, "application/pdf");
    }
    if content.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return (KIND_FILE_IMAGE, "image/jpeg");
    }
    if content.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        return (KIND_FILE_IMAGE, "image/png");
    }
    if content.starts_with(b"GIF87a") || content.starts_with(b"GIF89a") {
        return (KIND_FILE_IMAGE, "image/gif");
    }
    if content.len() >= 12 && &content[..4] == b"RIFF" && &content[8..12] == b"WEBP" {
        return (KIND_FILE_IMAGE, "image/webp");
    }
    if content.len() >= 12 && &content[4..8] == b"ftyp" {
        match &content[8..12] {
            b"heic" | b"heix" | b"hevc" | b"hevx" | b"mif1" | b"msf1" => {
                return (KIND_FILE_IMAGE, "image/heic")
            }
            b"avif" => return (KIND_FILE_IMAGE, "image/avif"),
            _ => {}
        }
    }
    if content.starts_with(b"PK\x03\x04") {
        // Archives and office documents; nothing unpacks them yet.
        return (KIND_FILE_OTHER, "application/zip");
    }
    let head = &content[..content.len().min(8192)];
    // Text has no control bytes apart from whitespace and escape.
    let has_control = head
        .iter()
        .any(|&b| b < 0x20 && !matches!(b, b'\t' | b'\n' | b'\r' | 0x0C | 0x1B));
    let looks_textual = !has_control
        && match std::str::from_utf8(head) {
            Ok(_) => true,
            // A multi-byte character cut off by the 8 KiB window is still text.
            Err(e) => e.error_len().is_none(),
        };
    if looks_textual && !content.is_empty() {
        let ext = name
            .and_then(|n| n.rsplit_once('.'))
            .map(|(_, e)| e.to_ascii_lowercase());
        let mime = match ext.as_deref() {
            Some("md") | Some("markdown") => "text/markdown",
            Some("csv") => "text/csv",
            Some("json") => "application/json",
            Some("html") | Some("htm") => "text/html",
            _ => "text/plain",
        };
        return (KIND_FILE_TEXT, mime);
    }
    (KIND_FILE_OTHER, "application/octet-stream")
}

/// Deletes the rows `processor`'s runs wrote to the shared tables.
pub fn clear_standard(conn: &Connection, processor: &str) -> Result<()> {
    for sql in [
        "DELETE FROM observations WHERE run_id IN (SELECT id FROM processor_runs WHERE processor = ?1)",
        "DELETE FROM events WHERE derived = 0 AND run_id IN (SELECT id FROM processor_runs WHERE processor = ?1)",
        "DELETE FROM items WHERE run_id IN (SELECT id FROM processor_runs WHERE processor = ?1)",
    ] {
        conn.execute(sql, [processor])?;
    }
    Ok(())
}

/// Adds an entity if it is new and returns its id. Ids depend only on kind
/// and name, so a rebuild reproduces them.
pub fn upsert_entity(conn: &Connection, kind: &str, name: &str) -> Result<String> {
    let id = stable_id("ent", &[kind, name]);
    conn.prepare_cached("INSERT OR IGNORE INTO entities (id, kind, name) VALUES (?1, ?2, ?3)")?
        .execute(params![id, kind, name])?;
    Ok(id)
}

/// Adds a rule-made link from a record to an entity. Returns whether it was new.
pub fn add_rule_link(
    conn: &Connection,
    from_type: &str,
    from_id: &str,
    entity_id: &str,
    relation: &str,
) -> Result<bool> {
    let n = conn
        .prepare_cached(
            "INSERT OR IGNORE INTO links (from_type, from_id, to_type, to_id, relation, confidence, method)
             VALUES (?1, ?2, 'entity', ?3, ?4, 1.0, 'rule')",
        )?
        .execute(params![from_type, from_id, entity_id, relation])?;
    Ok(n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generic_kinds_follow_magic_bytes() {
        assert_eq!(generic_file_kind(None, b"%PDF-1.7\n").0, KIND_FILE_PDF);
        assert_eq!(generic_file_kind(None, &[0xFF, 0xD8, 0xFF, 0xE1]), (KIND_FILE_IMAGE, "image/jpeg"));
        assert_eq!(
            generic_file_kind(None, b"\x00\x00\x00\x18ftypheic\x00\x00\x00\x00"),
            (KIND_FILE_IMAGE, "image/heic")
        );
        assert_eq!(generic_file_kind(Some("notes.md"), "# 标题\n".as_bytes()), (KIND_FILE_TEXT, "text/markdown"));
        assert_eq!(generic_file_kind(Some("a.bin"), &[0, 1, 2, 3]).0, KIND_FILE_OTHER);
        assert_eq!(
            generic_file_kind(Some("a.docx"), b"PK\x03\x04 text-like bytes"),
            (KIND_FILE_OTHER, "application/zip")
        );
        assert_eq!(generic_file_kind(None, b"").0, KIND_FILE_OTHER);
    }

    #[test]
    fn import_names_and_kinds_are_unique() {
        let specs: Vec<_> = import_specs().collect();
        for (i, a) in specs.iter().enumerate() {
            for b in &specs[i + 1..] {
                assert_ne!(a.name, b.name);
                assert_ne!(a.kind, b.kind);
            }
        }
    }
}
