//! Rule-based extraction from files no source recognised (`file_pdf`,
//! `file_image`, `file_text`, `file_other`): text, a title, and the times
//! and place the file itself records. No model, no network.
//!
//! Everything read from a file is data. Nothing here interprets the text
//! beyond finding a heading; nothing is executed.

pub mod exif;
pub mod pdf;

use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, TimeZone};
use serde_json::{json, Map, Value};

use crate::sources::{KIND_FILE_IMAGE, KIND_FILE_PDF, KIND_FILE_TEXT};
use crate::time::utc_string;

/// Version of the rules below; recorded in `items.data.extractor`.
pub const EXTRACTOR: &str = "files.rule/1";
/// Longest text kept per item, in characters.
pub const MAX_TEXT_CHARS: usize = 1_000_000;
/// Longest title, in characters.
pub const MAX_TITLE_CHARS: usize = 200;
/// PDFs larger than this are stored but not parsed.
pub const MAX_PDF_BYTES: usize = 128 << 20;

/// When something happened: UTC instant (if the offset is known) and the
/// local date it counts toward.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EventTime {
    pub start_utc: Option<String>,
    pub tz_offset: Option<i32>,
    pub local_date: Option<String>,
}

impl EventTime {
    pub fn from_local(local: NaiveDateTime, offset: Option<i32>) -> Self {
        let local_date = Some(local.date().to_string());
        match offset.and_then(FixedOffset::east_opt) {
            Some(off) => match off.from_local_datetime(&local).single() {
                Some(dt) => Self { start_utc: Some(utc_string(&dt)), tz_offset: Some(off.local_minus_utc()), local_date },
                None => Self { local_date, ..Default::default() },
            },
            None => Self { local_date, ..Default::default() },
        }
    }

    pub fn from_fixed(dt: &DateTime<FixedOffset>) -> Self {
        Self {
            start_utc: Some(utc_string(dt)),
            tz_offset: Some(dt.offset().local_minus_utc()),
            local_date: Some(dt.date_naive().to_string()),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.start_utc.is_none() && self.local_date.is_none()
    }
}

/// What the rules could read from one file.
#[derive(Debug, Clone)]
pub struct Extracted {
    pub item_type: &'static str,
    pub subtype: Option<&'static str>,
    pub category: Option<&'static str>,
    pub title: Option<String>,
    pub text: Option<String>,
    pub event: EventTime,
    pub source_updated_at: Option<String>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    /// False when rules found nothing useful: the item needs review.
    pub readable: bool,
    pub data: Map<String, Value>,
}

/// Runs the rules for one raw object.
pub fn extract(kind: &str, mime: &str, name: Option<&str>, content: &[u8]) -> Extracted {
    let stem = name.map(file_stem).filter(|s| !s.is_empty());
    let mut x = Extracted {
        item_type: "file",
        subtype: None,
        category: None,
        title: stem.clone(),
        text: None,
        event: EventTime::default(),
        source_updated_at: None,
        lat: None,
        lon: None,
        readable: false,
        data: Map::new(),
    };
    x.data.insert("extractor".into(), json!(EXTRACTOR));
    x.data.insert("mime".into(), json!(mime));
    x.data.insert("bytes".into(), json!(content.len()));
    if stem.is_some() {
        x.data.insert("title_source".into(), json!("file_name"));
    }
    match kind {
        KIND_FILE_TEXT => text_file(&mut x, mime, content),
        KIND_FILE_PDF => pdf_file(&mut x, content),
        KIND_FILE_IMAGE => image_file(&mut x, mime, content),
        _ => {
            x.data.insert("reason".into(), json!("unsupported_format"));
        }
    }
    if !x.readable && !x.data.contains_key("reason") {
        x.data.insert("reason".into(), json!("nothing_extracted"));
    }
    x
}

fn set_title(x: &mut Extracted, title: &str, source: &str) -> bool {
    let t = clean_line(title, MAX_TITLE_CHARS);
    if t.is_empty() {
        return false;
    }
    x.title = Some(t);
    x.data.insert("title_source".into(), json!(source));
    true
}

fn set_text(x: &mut Extracted, text: String) {
    let (text, truncated) = truncate_chars(text, MAX_TEXT_CHARS);
    if truncated {
        x.data.insert("truncated".into(), json!(true));
    }
    x.readable = !text.trim().is_empty();
    x.text = (!text.trim().is_empty()).then_some(text);
}

fn text_file(x: &mut Extracted, mime: &str, content: &[u8]) {
    x.item_type = "document";
    x.category = Some("documents");
    x.subtype = match mime {
        "text/markdown" | "text/plain" => Some("note"),
        "text/csv" => Some("table"),
        "application/json" => Some("data"),
        "text/html" => Some("web_page"),
        _ => None,
    };
    let content = content.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(content);
    let mut text = String::from_utf8_lossy(content).replace("\r\n", "\n");
    if mime == "text/markdown" {
        if let Some((front, body)) = split_front_matter(&text) {
            let front = front.to_string();
            if let Some(t) = front_matter_value(&front, &["title"]) {
                set_title(x, &t, "front_matter");
            }
            if let Some(updated) = front_matter_value(&front, &["updated", "modified", "lastmod"]) {
                if let Ok(dt) = DateTime::parse_from_rfc3339(&updated) {
                    x.source_updated_at = Some(utc_string(&dt));
                }
            }
            x.data.insert("front_matter".into(), json!(true));
            text = body.to_string();
        }
        if x.data.get("title_source").and_then(Value::as_str) != Some("front_matter") {
            if let Some(h) = first_heading(&text) {
                set_title(x, &h, "heading");
            }
        }
    }
    set_text(x, text.trim_matches('\n').to_string());
}

/// `---\n…\n---\n` at the very start of a Markdown file.
fn split_front_matter(text: &str) -> Option<(&str, &str)> {
    let rest = text.strip_prefix("---\n")?;
    let end = rest.find("\n---\n").map(|i| (i, i + 5)).or_else(|| rest.strip_suffix("\n---").map(|r| (r.len(), rest.len())))?;
    Some((&rest[..end.0], &rest[end.1..]))
}

fn front_matter_value(front: &str, keys: &[&str]) -> Option<String> {
    for line in front.lines() {
        let Some((k, v)) = line.split_once(':') else { continue };
        if keys.contains(&k.trim()) {
            let v = v.trim().trim_matches(|c| c == '"' || c == '\'').trim();
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// The first ATX heading (`# …`) outside code fences.
fn first_heading(text: &str) -> Option<String> {
    let mut fenced = false;
    for line in text.lines() {
        let t = line.trim_start();
        if t.starts_with("```") || t.starts_with("~~~") {
            fenced = !fenced;
            continue;
        }
        if fenced || line.starts_with("    ") {
            continue;
        }
        let hashes = t.bytes().take_while(|&b| b == b'#').count();
        if (1..=6).contains(&hashes) {
            let rest = &t[hashes..];
            if rest.is_empty() || rest.starts_with(' ') || rest.starts_with('\t') {
                let h = rest.trim().trim_end_matches('#').trim();
                if !h.is_empty() {
                    return Some(h.to_string());
                }
            }
        }
    }
    None
}

fn pdf_file(x: &mut Extracted, content: &[u8]) {
    x.item_type = "document";
    x.category = Some("documents");
    if content.len() > MAX_PDF_BYTES {
        x.data.insert("reason".into(), json!("too_large"));
        return;
    }
    let doc = pdf::Doc::parse(content);
    let t = doc.text();
    x.data.insert("pages".into(), json!(t.pages));
    x.data.insert("page_images".into(), json!(t.page_images));
    let mut pdf_meta = Map::new();
    if let Some(p) = &t.producer {
        pdf_meta.insert("producer".into(), json!(clean_line(p, 200)));
    }
    if let Some(c) = t.creation_date.as_deref().and_then(pdf_date_utc) {
        pdf_meta.insert("created_at".into(), json!(c));
    }
    if !pdf_meta.is_empty() {
        x.data.insert("pdf".into(), Value::Object(pdf_meta));
    }
    if !t.unsupported_filters.is_empty() {
        x.data.insert("unsupported_filters".into(), json!(t.unsupported_filters));
    }
    x.source_updated_at = t.mod_date.as_deref().and_then(pdf_date_utc);
    if let Some(title) = &t.title {
        set_title(x, title, "metadata");
    }
    if t.encrypted {
        x.data.insert("encrypted".into(), json!(true));
        x.data.insert("reason".into(), json!("encrypted"));
        return;
    }
    set_text(x, t.text);
    if !x.readable {
        x.data.insert("reason".into(), json!(if t.page_images > 0 { "no_text_layer" } else { "nothing_extracted" }));
    }
}

/// A PDF date with an explicit offset, as UTC. Dates without one are
/// ambiguous and are left out.
fn pdf_date_utc(s: &str) -> Option<String> {
    let (local, offset) = pdf::parse_pdf_date(s)?;
    EventTime::from_local(local, Some(offset?)).start_utc
}

fn image_file(x: &mut Extracted, mime: &str, content: &[u8]) {
    x.item_type = "photo";
    x.category = Some("media");
    let m = exif::read(content);
    let mut e = Map::new();
    if let Some(v) = &m.make {
        e.insert("make".into(), json!(clean_line(v, 100)));
    }
    if let Some(v) = &m.model {
        e.insert("model".into(), json!(clean_line(v, 100)));
    }
    if let Some(v) = m.orientation {
        e.insert("orientation".into(), json!(v));
    }
    if let Some(v) = m.gps_alt_m {
        e.insert("altitude_m".into(), json!(v));
    }
    if let (Some(w), Some(h)) = (m.width, m.height) {
        x.data.insert("width".into(), json!(w));
        x.data.insert("height".into(), json!(h));
    }

    let capture = m.datetime_original.as_deref().and_then(exif_datetime);
    let gps_utc = gps_datetime(&m);
    let mut capture_offset = m.offset_original.as_deref().and_then(parse_offset);
    if capture_offset.is_some() {
        e.insert("offset_source".into(), json!("exif"));
    } else if let (Some(local), Some(utc)) = (capture, gps_utc) {
        // The camera clock is local; GPS time is UTC. Their difference,
        // rounded to a quarter hour, is the offset.
        let diff = (local - utc).num_seconds();
        let rounded = ((diff as f64 / 900.0).round() as i64 * 900) as i32;
        if rounded.abs() <= 14 * 3600 {
            capture_offset = Some(rounded);
            e.insert("offset_source".into(), json!("gps_time"));
        }
    }
    if let Some(local) = capture {
        x.event = EventTime::from_local(local, capture_offset);
        e.insert("captured_local".into(), json!(local.format("%Y-%m-%dT%H:%M:%S").to_string()));
    }
    if let Some(modified) = m.modify_date.as_deref().and_then(exif_datetime) {
        let offset = m.offset_modify.as_deref().and_then(parse_offset).or(capture_offset);
        x.source_updated_at = EventTime::from_local(modified, offset).start_utc;
    }
    x.lat = m.gps_lat;
    x.lon = m.gps_lon;
    if !e.is_empty() {
        x.data.insert("exif".into(), Value::Object(e));
    }
    x.readable = !x.event.is_empty() || x.lat.is_some();
    if !x.readable {
        x.data.insert("reason".into(), json!(if m.is_empty() { "no_metadata" } else { "no_time_or_place" }));
    }
    if matches!(mime, "image/heic" | "image/avif") {
        x.data.insert("pixels".into(), json!("not_decoded"));
    }
}

/// `YYYY:MM:DD HH:MM:SS` (EXIF), rejecting the all-zero placeholder.
fn exif_datetime(s: &str) -> Option<NaiveDateTime> {
    let dt = NaiveDateTime::parse_from_str(s.trim(), "%Y:%m:%d %H:%M:%S").ok()?;
    (1900..=2200).contains(&chrono::Datelike::year(&dt)).then_some(dt)
}

fn gps_datetime(m: &exif::ImageMeta) -> Option<NaiveDateTime> {
    let date = NaiveDate::parse_from_str(m.gps_date.as_deref()?, "%Y:%m:%d").ok()?;
    let (h, mi, s) = m.gps_time?;
    let secs = (h * 3600.0 + mi * 60.0 + s).round() as i64;
    if !(0..86_400 + 1).contains(&secs) {
        return None;
    }
    Some(date.and_hms_opt(0, 0, 0)? + chrono::Duration::seconds(secs))
}

/// `+08:00` / `-05:30` → seconds east of UTC.
fn parse_offset(s: &str) -> Option<i32> {
    let s = s.trim();
    let (sign, rest) = match s.as_bytes().first()? {
        b'+' => (1, &s[1..]),
        b'-' => (-1, &s[1..]),
        _ => return None,
    };
    let (h, m) = rest.split_once(':')?;
    let (h, m): (i32, i32) = (h.parse().ok()?, m.parse().ok()?);
    (h <= 14 && (0..60).contains(&m)).then_some(sign * (h * 3600 + m * 60))
}

/// The file name without directories and extension.
pub fn file_stem(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let stem = match base.rsplit_once('.') {
        Some((s, _)) if !s.is_empty() => s,
        _ => base,
    };
    clean_line(stem, MAX_TITLE_CHARS)
}

/// One line of display text: control characters removed, whitespace
/// collapsed, at most `max` characters.
pub fn clean_line(s: &str, max: usize) -> String {
    let collapsed = s
        .split(|c: char| c.is_whitespace() || c.is_control())
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    truncate_chars(collapsed, max).0
}

pub fn truncate_chars(s: String, max: usize) -> (String, bool) {
    match s.char_indices().nth(max) {
        Some((i, _)) => (s[..i].to_string(), true),
        None => (s, false),
    }
}

/// Images a vision model can read for one file: the photo itself (JPEG,
/// PNG, WebP, GIF) or the page images of a scanned PDF. HEIC and AVIF are
/// not sent: their pixels would need decoding first.
pub fn model_images(kind: &str, mime: &str, content: &[u8], max_images: usize, max_bytes: usize) -> Vec<pdf::PageImage> {
    match kind {
        KIND_FILE_IMAGE => match mime {
            "image/jpeg" | "image/png" | "image/webp" | "image/gif" if content.len() <= max_bytes && max_images > 0 => {
                let mime = match mime {
                    "image/jpeg" => "image/jpeg",
                    "image/png" => "image/png",
                    "image/webp" => "image/webp",
                    _ => "image/gif",
                };
                vec![pdf::PageImage { page: 1, mime, bytes: content.to_vec() }]
            }
            _ => Vec::new(),
        },
        KIND_FILE_PDF if content.len() <= MAX_PDF_BYTES => {
            let doc = pdf::Doc::parse(content);
            if doc.text().encrypted {
                return Vec::new();
            }
            doc.images(max_images, max_bytes)
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headings_and_front_matter() {
        let x = extract(KIND_FILE_TEXT, "text/markdown", Some("a/b/plan.md"), b"```\n# not this\n```\n## Real heading ##\nbody");
        assert_eq!(x.title.as_deref(), Some("Real heading"));
        let x = extract(
            KIND_FILE_TEXT,
            "text/markdown",
            Some("n.md"),
            b"---\ntitle: \"Front title\"\nupdated: 2026-10-01T10:00:00+08:00\n---\n# H\ntext",
        );
        assert_eq!(x.title.as_deref(), Some("Front title"));
        assert_eq!(x.source_updated_at.as_deref(), Some("2026-10-01T02:00:00Z"));
        assert_eq!(x.text.as_deref(), Some("# H\ntext"));
        let x = extract(KIND_FILE_TEXT, "text/plain", Some("empty.txt"), b"\n\n");
        assert!(!x.readable);
        assert_eq!(x.title.as_deref(), Some("empty"));
    }

    #[test]
    fn offsets_and_names() {
        assert_eq!(parse_offset("+08:00"), Some(28_800));
        assert_eq!(parse_offset("-05:30"), Some(-19_800));
        assert_eq!(parse_offset("08:00"), None);
        assert_eq!(file_stem("C:\\x\\IMG_0001.JPG"), "IMG_0001");
        assert_eq!(file_stem(".env"), ".env");
        assert_eq!(clean_line("a\u{0007}b\n  c", 10), "a b c");
        assert!(exif_datetime("0000:00:00 00:00:00").is_none());
    }
}
