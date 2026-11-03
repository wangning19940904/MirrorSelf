//! Checkup reports and lab sheets: PDFs and photos (architecture §7).
//!
//! Reading them is the job of the `report.ocr` and `report.extract`
//! processors; this module only recognizes the file types.

pub const SOURCE: &str = "report";
pub const KIND: &str = "checkup_report";

/// The MIME type of a supported report file, from its first bytes.
pub fn sniff(content: &[u8]) -> Option<&'static str> {
    let head = &content[..content.len().min(16)];
    if head.starts_with(b"%PDF-") {
        Some("application/pdf")
    } else if head.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if head.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if head.len() >= 12 && &head[4..8] == b"ftyp" && matches!(&head[8..12], b"heic" | b"heix" | b"mif1" | b"msf1") {
        Some("image/heic")
    } else if head.starts_with(b"II*\0") || head.starts_with(b"MM\0*") {
        Some("image/tiff")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_pdfs_and_photos() {
        assert_eq!(sniff(b"%PDF-1.7\n..."), Some("application/pdf"));
        assert_eq!(sniff(&[0xFF, 0xD8, 0xFF, 0xE0, 0, 0]), Some("image/jpeg"));
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\n...."), Some("image/png"));
        assert_eq!(sniff(b"\0\0\0\x18ftypheic...."), Some("image/heic"));
        assert_eq!(sniff(b"<?xml version"), None);
    }
}
