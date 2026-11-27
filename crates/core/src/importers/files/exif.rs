//! EXIF metadata from JPEG, PNG, HEIC/AVIF and WebP files: capture time and
//! its offset, GPS position and time, camera, and pixel size. Pixels are
//! never decoded.
//!
//! The TIFF structure is read with bounds checks on every offset; a damaged
//! block yields fewer fields, never a panic.

use std::collections::HashSet;

/// Fields read from an image's metadata. All optional.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ImageMeta {
    /// `DateTimeOriginal` as written (`YYYY:MM:DD HH:MM:SS`).
    pub datetime_original: Option<String>,
    /// `OffsetTimeOriginal`, e.g. `+08:00`.
    pub offset_original: Option<String>,
    /// `DateTime` (IFD0): when the file was last changed.
    pub modify_date: Option<String>,
    /// `OffsetTime`: the offset of `DateTime`.
    pub offset_modify: Option<String>,
    /// WGS-84 degrees, south and west negative.
    pub gps_lat: Option<f64>,
    pub gps_lon: Option<f64>,
    pub gps_alt_m: Option<f64>,
    /// `GPSDateStamp` (`YYYY:MM:DD`) and `GPSTimeStamp` (UTC h, m, s).
    pub gps_date: Option<String>,
    pub gps_time: Option<(f64, f64, f64)>,
    pub make: Option<String>,
    pub model: Option<String>,
    pub orientation: Option<u16>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

impl ImageMeta {
    pub fn is_empty(&self) -> bool {
        self == &ImageMeta { width: self.width, height: self.height, ..Default::default() }
    }
}

/// Reads whatever metadata `content` carries.
pub fn read(content: &[u8]) -> ImageMeta {
    let mut meta = find_tiff(content).map(parse_tiff).unwrap_or_default();
    if let Some((w, h)) = pixel_size(content) {
        meta.width = Some(w);
        meta.height = Some(h);
    }
    meta
}

/// The TIFF block holding EXIF: a JPEG APP1 segment, a PNG `eXIf` chunk, or
/// (HEIC, AVIF, WebP) the first `Exif\0\0` header followed by a TIFF header.
fn find_tiff(d: &[u8]) -> Option<&[u8]> {
    if d.starts_with(&[0xFF, 0xD8]) {
        let mut p = 2;
        while p + 4 <= d.len() && d[p] == 0xFF {
            let marker = d[p + 1];
            if marker == 0xD9 || marker == 0xDA {
                break; // end of image, or start of scan: no more metadata
            }
            if (0xD0..=0xD7).contains(&marker) || marker == 0x01 || marker == 0xFF {
                p += if marker == 0xFF { 1 } else { 2 };
                continue;
            }
            let len = usize::from(u16::from_be_bytes([d[p + 2], d[p + 3]]));
            let body = d.get(p + 4..p + 2 + len)?;
            if marker == 0xE1 && body.starts_with(b"Exif\0\0") {
                return Some(&body[6..]);
            }
            p += 2 + len;
        }
        return None;
    }
    if d.starts_with(&[0x89, b'P', b'N', b'G']) {
        let mut p = 8;
        while p + 12 <= d.len() {
            let len = u32::from_be_bytes(d[p..p + 4].try_into().ok()?) as usize;
            let kind = &d[p + 4..p + 8];
            let body = d.get(p + 8..p + 8 + len)?;
            if kind == b"eXIf" {
                return Some(body);
            }
            if kind == b"IDAT" || kind == b"IEND" {
                return None;
            }
            p += 12 + len;
        }
        return None;
    }
    // Container formats: look for the header within the first 4 MiB.
    let window = &d[..d.len().min(4 << 20)];
    let mut from = 0;
    while let Some(i) = window[from..].windows(6).position(|w| w == b"Exif\0\0") {
        let at = from + i + 6;
        let rest = &d[at..];
        if rest.starts_with(b"II*\0") || rest.starts_with(b"MM\0*") {
            return Some(rest);
        }
        from = at;
    }
    // WebP's EXIF chunk may hold the TIFF header directly.
    if d.len() >= 12 && &d[..4] == b"RIFF" && &d[8..12] == b"WEBP" {
        let mut p = 12;
        while p + 8 <= d.len() {
            let len = u32::from_le_bytes(d[p + 4..p + 8].try_into().ok()?) as usize;
            if &d[p..p + 4] == b"EXIF" {
                let body = d.get(p + 8..p + 8 + len)?;
                if body.starts_with(b"II*\0") || body.starts_with(b"MM\0*") {
                    return Some(body);
                }
            }
            p += 8 + len + (len & 1);
        }
    }
    None
}

struct Tiff<'a> {
    d: &'a [u8],
    le: bool,
}

#[derive(Clone, Copy)]
struct Entry {
    tag: u16,
    typ: u16,
    count: u32,
    /// Offset of the value bytes within the TIFF block.
    at: usize,
}

impl Tiff<'_> {
    fn u16(&self, p: usize) -> Option<u16> {
        let b: [u8; 2] = self.d.get(p..p + 2)?.try_into().ok()?;
        Some(if self.le { u16::from_le_bytes(b) } else { u16::from_be_bytes(b) })
    }

    fn u32(&self, p: usize) -> Option<u32> {
        let b: [u8; 4] = self.d.get(p..p + 4)?.try_into().ok()?;
        Some(if self.le { u32::from_le_bytes(b) } else { u32::from_be_bytes(b) })
    }

    fn entries(&self, ifd: usize) -> Vec<Entry> {
        let Some(n) = self.u16(ifd) else { return Vec::new() };
        let mut out = Vec::new();
        for i in 0..usize::from(n.min(512)) {
            let e = ifd + 2 + i * 12;
            let (Some(tag), Some(typ), Some(count)) = (self.u16(e), self.u16(e + 2), self.u32(e + 4)) else { break };
            let size = match typ {
                1 | 2 | 6 | 7 => 1,
                3 | 8 => 2,
                4 | 9 | 11 => 4,
                5 | 10 | 12 => 8,
                _ => continue,
            };
            let Some(total) = (count as usize).checked_mul(size) else { continue };
            let at = if total <= 4 {
                e + 8
            } else {
                match self.u32(e + 8) {
                    Some(o) => o as usize,
                    None => continue,
                }
            };
            if at.checked_add(total).is_none_or(|end| end > self.d.len()) {
                continue;
            }
            out.push(Entry { tag, typ, count, at });
        }
        out
    }

    fn ascii(&self, e: &Entry) -> Option<String> {
        if e.typ != 2 && e.typ != 7 {
            return None;
        }
        let raw = self.d.get(e.at..e.at + e.count as usize)?;
        let raw = raw.split(|&b| b == 0).next().unwrap_or_default();
        let s: String = String::from_utf8_lossy(raw).chars().filter(|c| !c.is_control()).collect();
        let s = s.trim().to_string();
        (!s.is_empty()).then_some(s)
    }

    fn uint(&self, e: &Entry) -> Option<u32> {
        match e.typ {
            3 => self.u16(e.at).map(u32::from),
            4 => self.u32(e.at),
            1 | 7 => self.d.get(e.at).map(|&b| u32::from(b)),
            _ => None,
        }
    }

    fn rational(&self, e: &Entry, i: usize) -> Option<f64> {
        if (e.typ != 5 && e.typ != 10) || i >= e.count as usize {
            return None;
        }
        let p = e.at + i * 8;
        let (n, d) = (self.u32(p)?, self.u32(p + 4)?);
        if d == 0 {
            return None;
        }
        Some(if e.typ == 10 { f64::from(n as i32) / f64::from(d as i32) } else { f64::from(n) / f64::from(d) })
    }
}

fn parse_tiff(d: &[u8]) -> ImageMeta {
    let mut m = ImageMeta::default();
    let le = match d.get(..4) {
        Some(b"II*\0") => true,
        Some(b"MM\0*") => false,
        _ => return m,
    };
    let t = Tiff { d, le };
    let Some(ifd0) = t.u32(4).map(|o| o as usize) else { return m };
    let mut seen = HashSet::new();
    let mut exif_ifd = None;
    let mut gps_ifd = None;
    if seen.insert(ifd0) {
        for e in t.entries(ifd0) {
            match e.tag {
                0x010F => m.make = t.ascii(&e),
                0x0110 => m.model = t.ascii(&e),
                0x0112 => m.orientation = t.uint(&e).and_then(|v| u16::try_from(v).ok()),
                0x0132 => m.modify_date = t.ascii(&e),
                0x8769 => exif_ifd = t.uint(&e).map(|o| o as usize),
                0x8825 => gps_ifd = t.uint(&e).map(|o| o as usize),
                _ => {}
            }
        }
    }
    if let Some(ifd) = exif_ifd.filter(|o| seen.insert(*o)) {
        for e in t.entries(ifd) {
            match e.tag {
                0x9003 => m.datetime_original = t.ascii(&e),
                0x9010 => m.offset_modify = t.ascii(&e),
                0x9011 => m.offset_original = t.ascii(&e),
                0xA002 => m.width = t.uint(&e),
                0xA003 => m.height = t.uint(&e),
                _ => {}
            }
        }
    }
    if let Some(ifd) = gps_ifd.filter(|o| seen.insert(*o)) {
        let mut lat_ref = None;
        let mut lon_ref = None;
        let mut alt_ref = 0;
        let mut lat = None;
        let mut lon = None;
        let mut alt = None;
        for e in t.entries(ifd) {
            match e.tag {
                1 => lat_ref = t.ascii(&e),
                2 => lat = dms(&t, &e),
                3 => lon_ref = t.ascii(&e),
                4 => lon = dms(&t, &e),
                5 => alt_ref = t.uint(&e).unwrap_or(0),
                6 => alt = t.rational(&e, 0),
                7 => {
                    if let (Some(h), Some(mi), Some(s)) = (t.rational(&e, 0), t.rational(&e, 1), t.rational(&e, 2)) {
                        m.gps_time = Some((h, mi, s));
                    }
                }
                29 => m.gps_date = t.ascii(&e),
                _ => {}
            }
        }
        let sign = |r: &Option<String>, neg: &str| if r.as_deref() == Some(neg) { -1.0 } else { 1.0 };
        if let (Some(la), Some(lo)) = (lat, lon) {
            let la = round7(la * sign(&lat_ref, "S"));
            let lo = round7(lo * sign(&lon_ref, "W"));
            let valid = (-90.0..=90.0).contains(&la) && (-180.0..=180.0).contains(&lo) && !(la == 0.0 && lo == 0.0);
            if valid {
                m.gps_lat = Some(la);
                m.gps_lon = Some(lo);
            }
        }
        m.gps_alt_m = alt.map(|a| round7(if alt_ref == 1 { -a } else { a }));
    }
    m
}

fn dms(t: &Tiff<'_>, e: &Entry) -> Option<f64> {
    let d = t.rational(e, 0)?;
    let m = t.rational(e, 1).unwrap_or(0.0);
    let s = t.rational(e, 2).unwrap_or(0.0);
    Some(d + m / 60.0 + s / 3600.0)
}

fn round7(x: f64) -> f64 {
    (x * 1e7).round() / 1e7
}

/// Pixel size from a JPEG frame header or a PNG `IHDR`.
fn pixel_size(d: &[u8]) -> Option<(u32, u32)> {
    if d.starts_with(&[0x89, b'P', b'N', b'G']) && d.get(12..16) == Some(b"IHDR") {
        let w = u32::from_be_bytes(d.get(16..20)?.try_into().ok()?);
        let h = u32::from_be_bytes(d.get(20..24)?.try_into().ok()?);
        return Some((w, h));
    }
    if d.starts_with(&[0xFF, 0xD8]) {
        let mut p = 2;
        while p + 4 <= d.len() && d[p] == 0xFF {
            let marker = d[p + 1];
            if (0xD0..=0xD7).contains(&marker) || marker == 0x01 || marker == 0xFF {
                p += if marker == 0xFF { 1 } else { 2 };
                continue;
            }
            let len = usize::from(u16::from_be_bytes([d[p + 2], d[p + 3]]));
            let is_sof = matches!(marker, 0xC0..=0xCF) && !matches!(marker, 0xC4 | 0xC8 | 0xCC);
            if is_sof {
                let h = u16::from_be_bytes([*d.get(p + 5)?, *d.get(p + 6)?]);
                let w = u16::from_be_bytes([*d.get(p + 7)?, *d.get(p + 8)?]);
                return Some((u32::from(w), u32::from(h)));
            }
            if marker == 0xDA || len < 2 {
                break;
            }
            p += 2 + len;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn damaged_blocks_yield_nothing() {
        assert!(read(b"").is_empty());
        assert!(read(&[0xFF, 0xD8, 0xFF, 0xE1, 0x00, 0x10, b'E', b'x', b'i', b'f', 0, 0, b'I', b'I']).is_empty());
        let mut tiff = b"II*\0\x08\0\0\0\xff\xff".to_vec();
        tiff.extend_from_slice(&[0xAA; 20]);
        assert!(parse_tiff(&tiff).is_empty());
    }

    #[test]
    fn finds_exif_in_containers() {
        let mut heic = b"\0\0\0\x18ftypheic\0\0\0\0meta....Exif\0\0".to_vec();
        heic.extend_from_slice(b"MM\0*\0\0\0\x08\0\x01\x01\x0f\0\x02\0\0\0\x04ACME\0\0\0\0");
        assert_eq!(read(&heic).make.as_deref(), Some("ACME"));
    }
}
