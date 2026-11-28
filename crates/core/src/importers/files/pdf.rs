//! A small, bounded PDF reader: enough for the text layer of ordinary PDFs
//! (including CJK text through ToUnicode CMaps or the predefined UCS-2
//! CMaps), the document information dictionary, and the page images a
//! scanned PDF consists of.
//!
//! It is deliberately not a full PDF implementation. Objects are found by
//! scanning the file (so broken cross-reference tables do not matter), and
//! object streams are unpacked. Not supported: encryption, LZW streams,
//! CID fonts without a Unicode mapping, and rendering. Anything it cannot
//! read yields less text, never an error that stops the import, and the
//! file is then left for review.
//!
//! Every loop is bounded: decoded bytes per stream and per document, pages,
//! nesting depth, content operators and output size. File contents are only
//! ever parsed as data.

use std::cell::Cell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Read;
use std::rc::Rc;

use flate2::read::{DeflateDecoder, ZlibDecoder};

const MAX_STREAM_BYTES: usize = 32 << 20;
const MAX_DOC_DECODED: usize = 256 << 20;
const MAX_PAGES: usize = 5_000;
const MAX_VALUE_DEPTH: usize = 64;
const MAX_OPS_PER_DOC: usize = 5_000_000;
const MAX_FORM_DEPTH: usize = 8;
const MAX_CMAP_ENTRIES: usize = 200_000;

pub type Dict = BTreeMap<Vec<u8>, Obj>;

#[derive(Debug, Clone, PartialEq)]
pub enum Obj {
    Null,
    Bool(bool),
    Int(i64),
    Real(f64),
    Str(Vec<u8>),
    Name(Vec<u8>),
    Array(Vec<Obj>),
    Dict(Dict),
    Ref(u32),
    /// A stream: its dictionary and the byte range of its data in the file.
    Stream(Dict, usize, usize),
    /// A bare keyword (content operators, `R`, `obj`, …).
    Op(Vec<u8>),
}

static NULL: Obj = Obj::Null;

impl Obj {
    fn as_dict(&self) -> Option<&Dict> {
        match self {
            Obj::Dict(d) | Obj::Stream(d, _, _) => Some(d),
            _ => None,
        }
    }

    fn as_name(&self) -> Option<&[u8]> {
        match self {
            Obj::Name(n) => Some(n),
            _ => None,
        }
    }

    fn as_num(&self) -> Option<f64> {
        match self {
            Obj::Int(i) => Some(*i as f64),
            Obj::Real(r) => Some(*r),
            _ => None,
        }
    }

    fn as_int(&self) -> Option<i64> {
        match self {
            Obj::Int(i) => Some(*i),
            Obj::Real(r) if r.fract() == 0.0 => Some(*r as i64),
            _ => None,
        }
    }
}

fn is_ws(b: u8) -> bool {
    matches!(b, 0 | b'\t' | b'\n' | 0x0C | b'\r' | b' ')
}

fn is_delim(b: u8) -> bool {
    matches!(b, b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%')
}

fn is_regular(b: u8) -> bool {
    !is_ws(b) && !is_delim(b)
}

struct Lexer<'a> {
    d: &'a [u8],
    pos: usize,
}

impl<'a> Lexer<'a> {
    fn new(d: &'a [u8], pos: usize) -> Self {
        Self { d, pos }
    }

    fn at_end(&self) -> bool {
        self.pos >= self.d.len()
    }

    fn skip_ws(&mut self) {
        while self.pos < self.d.len() {
            let b = self.d[self.pos];
            if is_ws(b) {
                self.pos += 1;
            } else if b == b'%' {
                while self.pos < self.d.len() && !matches!(self.d[self.pos], b'\n' | b'\r') {
                    self.pos += 1;
                }
            } else {
                break;
            }
        }
    }

    fn starts_with(&self, s: &[u8]) -> bool {
        self.d[self.pos.min(self.d.len())..].starts_with(s)
    }

    /// The next value, or `None` at the end of input. Keywords come back as
    /// [`Obj::Op`]; `n g R` becomes a reference.
    fn value(&mut self, depth: usize) -> Option<Obj> {
        self.skip_ws();
        if self.at_end() || depth > MAX_VALUE_DEPTH {
            return None;
        }
        let b = self.d[self.pos];
        match b {
            b'/' => {
                self.pos += 1;
                let start = self.pos;
                while self.pos < self.d.len() && is_regular(self.d[self.pos]) {
                    self.pos += 1;
                }
                Some(Obj::Name(unescape_name(&self.d[start..self.pos])))
            }
            b'(' => Some(Obj::Str(self.literal_string())),
            b'<' if self.d.get(self.pos + 1) == Some(&b'<') => {
                self.pos += 2;
                let mut dict = Dict::new();
                loop {
                    self.skip_ws();
                    if self.at_end() {
                        break;
                    }
                    if self.starts_with(b">>") {
                        self.pos += 2;
                        break;
                    }
                    match self.value(depth + 1)? {
                        Obj::Name(k) => {
                            let v = self.value(depth + 1).unwrap_or(Obj::Null);
                            dict.insert(k, v);
                        }
                        // A stray token: skip it rather than give up.
                        _ => continue,
                    }
                }
                Some(Obj::Dict(dict))
            }
            b'<' => Some(Obj::Str(self.hex_string())),
            b'[' => {
                self.pos += 1;
                let mut items = Vec::new();
                loop {
                    self.skip_ws();
                    if self.at_end() {
                        break;
                    }
                    if self.d[self.pos] == b']' {
                        self.pos += 1;
                        break;
                    }
                    match self.value(depth + 1) {
                        Some(v) => items.push(v),
                        None => break,
                    }
                }
                Some(Obj::Array(items))
            }
            b']' | b'>' | b')' | b'{' | b'}' => {
                self.pos += 1;
                Some(Obj::Op(vec![b]))
            }
            b'0'..=b'9' | b'+' | b'-' | b'.' => {
                let num = self.number();
                if let Obj::Int(n) = num {
                    if n >= 0 {
                        if let Some(r) = self.try_ref(n) {
                            return Some(r);
                        }
                    }
                }
                Some(num)
            }
            _ => {
                let start = self.pos;
                while self.pos < self.d.len() && is_regular(self.d[self.pos]) {
                    self.pos += 1;
                }
                if self.pos == start {
                    self.pos += 1;
                }
                let word = &self.d[start..self.pos];
                Some(match word {
                    b"true" => Obj::Bool(true),
                    b"false" => Obj::Bool(false),
                    b"null" => Obj::Null,
                    _ => Obj::Op(word.to_vec()),
                })
            }
        }
    }

    fn number(&mut self) -> Obj {
        let start = self.pos;
        while self.pos < self.d.len() && matches!(self.d[self.pos], b'0'..=b'9' | b'+' | b'-' | b'.') {
            self.pos += 1;
        }
        let s = std::str::from_utf8(&self.d[start..self.pos]).unwrap_or("0");
        if !s.contains('.') {
            if let Ok(i) = s.parse::<i64>() {
                return Obj::Int(i);
            }
        }
        Obj::Real(s.parse::<f64>().unwrap_or(0.0))
    }

    /// After an integer: `gen R` makes it a reference.
    fn try_ref(&mut self, num: i64) -> Option<Obj> {
        let save = self.pos;
        let mut p = self.pos;
        let d = self.d;
        let skip = |p: &mut usize| {
            while *p < d.len() && is_ws(d[*p]) {
                *p += 1;
            }
        };
        skip(&mut p);
        let gs = p;
        while p < d.len() && d[p].is_ascii_digit() {
            p += 1;
        }
        if p == gs || p - gs > 5 {
            return None;
        }
        skip(&mut p);
        if p < d.len() && d[p] == b'R' && (p + 1 == d.len() || !is_regular(d[p + 1])) {
            self.pos = p + 1;
            return u32::try_from(num).ok().map(Obj::Ref);
        }
        self.pos = save;
        None
    }

    fn literal_string(&mut self) -> Vec<u8> {
        self.pos += 1; // (
        let mut out = Vec::new();
        let mut depth = 1usize;
        while self.pos < self.d.len() {
            let b = self.d[self.pos];
            self.pos += 1;
            match b {
                b'\\' => {
                    let Some(&e) = self.d.get(self.pos) else { break };
                    self.pos += 1;
                    match e {
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'b' => out.push(8),
                        b'f' => out.push(12),
                        b'0'..=b'7' => {
                            let mut v = u32::from(e - b'0');
                            for _ in 0..2 {
                                match self.d.get(self.pos) {
                                    Some(&c @ b'0'..=b'7') => {
                                        v = v * 8 + u32::from(c - b'0');
                                        self.pos += 1;
                                    }
                                    _ => break,
                                }
                            }
                            out.push((v & 0xFF) as u8);
                        }
                        b'\r' => {
                            if self.d.get(self.pos) == Some(&b'\n') {
                                self.pos += 1;
                            }
                        }
                        b'\n' => {}
                        other => out.push(other),
                    }
                }
                b'(' => {
                    depth += 1;
                    out.push(b);
                }
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                    out.push(b);
                }
                _ => out.push(b),
            }
        }
        out
    }

    fn hex_string(&mut self) -> Vec<u8> {
        self.pos += 1; // <
        let mut out = Vec::new();
        let mut hi: Option<u8> = None;
        while self.pos < self.d.len() {
            let b = self.d[self.pos];
            self.pos += 1;
            if b == b'>' {
                break;
            }
            let Some(v) = hex_val(b) else { continue };
            match hi.take() {
                Some(h) => out.push(h << 4 | v),
                None => hi = Some(v),
            }
        }
        if let Some(h) = hi {
            out.push(h << 4);
        }
        out
    }
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn unescape_name(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'#' && i + 2 < raw.len() {
            if let (Some(h), Some(l)) = (hex_val(raw[i + 1]), hex_val(raw[i + 2])) {
                out.push(h << 4 | l);
                i += 3;
                continue;
            }
        }
        out.push(raw[i]);
        i += 1;
    }
    out
}

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if from >= hay.len() || needle.is_empty() {
        return None;
    }
    hay[from..].windows(needle.len()).position(|w| w == needle).map(|p| p + from)
}

/// What [`read`] found in a PDF.
#[derive(Debug, Default)]
pub struct PdfText {
    pub pages: usize,
    /// Page texts joined by a blank line; empty when there is no text layer.
    pub text: String,
    pub title: Option<String>,
    pub creation_date: Option<String>,
    pub mod_date: Option<String>,
    pub producer: Option<String>,
    pub encrypted: bool,
    /// Image XObjects drawn on the pages, in page order.
    pub page_images: usize,
    /// Streams that used a filter this reader does not decode.
    pub unsupported_filters: Vec<String>,
}

/// An image taken from a page, ready to send to a model.
#[derive(Debug, Clone)]
pub struct PageImage {
    pub page: usize,
    pub mime: &'static str,
    pub bytes: Vec<u8>,
}

pub struct Doc<'a> {
    data: &'a [u8],
    objects: HashMap<u32, Obj>,
    trailer: Dict,
    budget: Cell<usize>,
    unsupported: std::cell::RefCell<Vec<String>>,
}

impl<'a> Doc<'a> {
    pub fn parse(data: &'a [u8]) -> Self {
        let mut doc = Doc {
            data,
            objects: HashMap::new(),
            trailer: Dict::new(),
            budget: Cell::new(MAX_DOC_DECODED),
            unsupported: Default::default(),
        };
        doc.scan();
        doc
    }

    fn scan(&mut self) {
        let d = self.data;
        let mut pos = 0;
        let mut trailers: Vec<Dict> = Vec::new();
        while let Some(i) = find(d, b"obj", pos) {
            pos = i + 3;
            if i + 3 < d.len() && is_regular(d[i + 3]) {
                continue;
            }
            let Some(num) = object_number_before(d, i) else { continue };
            let mut lx = Lexer::new(d, i + 3);
            let Some(value) = lx.value(0) else { break };
            let mut end = lx.pos;
            let value = match value {
                Obj::Dict(dict) => {
                    let mut l2 = Lexer::new(d, lx.pos);
                    l2.skip_ws();
                    if l2.starts_with(b"stream") {
                        let mut s = l2.pos + 6;
                        if d.get(s) == Some(&b'\r') {
                            s += 1;
                        }
                        if d.get(s) == Some(&b'\n') {
                            s += 1;
                        }
                        let direct_len = dict
                            .get(b"Length".as_slice())
                            .and_then(Obj::as_int)
                            .and_then(|n| usize::try_from(n).ok())
                            .filter(|&n| {
                                s.checked_add(n).is_some_and(|e| e <= d.len()) && {
                                    let mut l3 = Lexer::new(d, s + n);
                                    l3.skip_ws();
                                    l3.starts_with(b"endstream")
                                }
                            });
                        let (e, after) = match direct_len {
                            Some(n) => {
                                let after = find(d, b"endstream", s + n).map_or(d.len(), |p| p + 9);
                                (s + n, after)
                            }
                            None => match find(d, b"endstream", s) {
                                Some(p) => {
                                    let mut e = p;
                                    if e > s && d[e - 1] == b'\n' {
                                        e -= 1;
                                    }
                                    if e > s && d[e - 1] == b'\r' {
                                        e -= 1;
                                    }
                                    (e, p + 9)
                                }
                                None => (d.len(), d.len()),
                            },
                        };
                        end = after;
                        if dict.get(b"Type".as_slice()).and_then(Obj::as_name) == Some(b"XRef") {
                            trailers.push(dict.clone());
                        }
                        Obj::Stream(dict, s, e)
                    } else {
                        Obj::Dict(dict)
                    }
                }
                other => other,
            };
            self.objects.insert(num, value);
            pos = pos.max(end);
        }
        // Classic trailers.
        let mut tpos = 0;
        while let Some(i) = find(d, b"trailer", tpos) {
            tpos = i + 7;
            let mut lx = Lexer::new(d, i + 7);
            if let Some(Obj::Dict(t)) = lx.value(0) {
                trailers.push(t);
            }
        }
        // Later trailers win, but keep keys only earlier ones had (/Info in
        // the first revision, /Root repeated, …).
        for t in trailers {
            for (k, v) in t {
                self.trailer.insert(k, v);
            }
        }
        self.unpack_object_streams();
    }

    fn unpack_object_streams(&mut self) {
        let streams: Vec<(Dict, usize, usize)> = self
            .objects
            .values()
            .filter_map(|o| match o {
                Obj::Stream(d, s, e) if d.get(b"Type".as_slice()).and_then(Obj::as_name) == Some(b"ObjStm") => {
                    Some((d.clone(), *s, *e))
                }
                _ => None,
            })
            .collect();
        for (dict, s, e) in streams {
            let Ok(data) = self.decode(&dict, s, e) else { continue };
            let n = dict.get(b"N".as_slice()).and_then(Obj::as_int).unwrap_or(0).clamp(0, 1_000_000) as usize;
            let first = dict.get(b"First".as_slice()).and_then(Obj::as_int).unwrap_or(0).max(0) as usize;
            let mut lx = Lexer::new(&data, 0);
            let mut pairs = Vec::with_capacity(n.min(10_000));
            for _ in 0..n {
                match (lx.value(0), lx.value(0)) {
                    (Some(Obj::Int(num)), Some(Obj::Int(off))) => pairs.push((num, off)),
                    _ => break,
                }
            }
            for (num, off) in pairs {
                let (Ok(num), Ok(off)) = (u32::try_from(num), usize::try_from(off)) else { continue };
                let Some(at) = first.checked_add(off).filter(|&a| a < data.len()) else { continue };
                if self.objects.contains_key(&num) {
                    continue;
                }
                let mut l = Lexer::new(&data, at);
                if let Some(v) = l.value(0) {
                    // Objects inside object streams cannot be streams, so
                    // the byte ranges in `v` never point into `data`.
                    self.objects.insert(num, v);
                }
            }
        }
    }

    fn resolve<'b>(&'b self, mut o: &'b Obj) -> &'b Obj {
        for _ in 0..32 {
            match o {
                Obj::Ref(n) => o = self.objects.get(n).unwrap_or(&NULL),
                _ => return o,
            }
        }
        &NULL
    }

    fn get<'b>(&'b self, dict: &'b Dict, key: &[u8]) -> &'b Obj {
        dict.get(key).map_or(&NULL, |o| self.resolve(o))
    }

    fn note_unsupported(&self, name: &[u8]) {
        let name = String::from_utf8_lossy(name).into_owned();
        let mut u = self.unsupported.borrow_mut();
        if !u.contains(&name) && u.len() < 16 {
            u.push(name);
        }
    }

    /// Decodes a stream's data through its filters. Image codecs (DCT, JPX,
    /// CCITT, JBIG2) stop the chain and are reported as unsupported.
    fn decode(&self, dict: &Dict, s: usize, e: usize) -> Result<Vec<u8>, Vec<u8>> {
        let raw = &self.data[s.min(self.data.len())..e.min(self.data.len())];
        let (filters, params) = self.filters(dict);
        let mut data = raw.to_vec();
        for (i, f) in filters.iter().enumerate() {
            let p = params.get(i).cloned().flatten();
            data = match f.as_slice() {
                b"FlateDecode" | b"Fl" => {
                    let out = inflate(&data, self.remaining())?;
                    self.spend(out.len());
                    match p {
                        Some(p) => png_unpredict(&out, &p, self),
                        None => out,
                    }
                }
                b"ASCIIHexDecode" | b"AHx" => ascii_hex(&data),
                b"ASCII85Decode" | b"A85" => ascii85(&data),
                other => {
                    self.note_unsupported(other);
                    return Err(other.to_vec());
                }
            };
        }
        Ok(data)
    }

    fn filters(&self, dict: &Dict) -> (Vec<Vec<u8>>, Vec<Option<Dict>>) {
        let filters: Vec<Vec<u8>> = match self.get(dict, b"Filter") {
            Obj::Name(n) => vec![n.clone()],
            Obj::Array(a) => a.iter().filter_map(|o| self.resolve(o).as_name().map(<[u8]>::to_vec)).collect(),
            _ => Vec::new(),
        };
        let params: Vec<Option<Dict>> = match self.get(dict, b"DecodeParms") {
            Obj::Dict(d) => vec![Some(d.clone())],
            Obj::Array(a) => a.iter().map(|o| self.resolve(o).as_dict().cloned()).collect(),
            _ => Vec::new(),
        };
        (filters, params)
    }

    fn remaining(&self) -> usize {
        self.budget.get().min(MAX_STREAM_BYTES)
    }

    fn spend(&self, n: usize) {
        self.budget.set(self.budget.get().saturating_sub(n));
    }

    fn stream_data(&self, o: &Obj) -> Option<Vec<u8>> {
        match self.resolve(o) {
            Obj::Stream(d, s, e) => self.decode(d, *s, *e).ok(),
            _ => None,
        }
    }

    fn info_string(&self, key: &[u8]) -> Option<String> {
        let info = self.get(&self.trailer, b"Info").as_dict()?;
        match self.get(info, key) {
            Obj::Str(s) => {
                let t = decode_text_string(s);
                let t = t.trim();
                (!t.is_empty()).then(|| t.to_string())
            }
            _ => None,
        }
    }

    /// Page dictionaries in document order, with inherited resources.
    fn pages(&self) -> Vec<(Dict, Dict)> {
        let mut out = Vec::new();
        let root = self.get(&self.trailer, b"Root");
        let catalog = match root.as_dict() {
            Some(c) => Some(c),
            None => self
                .objects
                .values()
                .find(|o| o.as_dict().and_then(|d| d.get(b"Type".as_slice())).and_then(Obj::as_name) == Some(b"Catalog"))
                .and_then(Obj::as_dict),
        };
        if let Some(catalog) = catalog {
            let mut seen = HashSet::new();
            if let Some(Obj::Ref(r)) = catalog.get(b"Pages".as_slice()) {
                seen.insert(*r);
            }
            self.walk_pages(self.get(catalog, b"Pages"), &Dict::new(), 0, &mut seen, &mut out);
        }
        if out.is_empty() {
            // No usable page tree: every page object in number order.
            let mut nums: Vec<&u32> = self.objects.keys().collect();
            nums.sort();
            for n in nums {
                if let Some(d) = self.objects[n].as_dict() {
                    if d.get(b"Type".as_slice()).and_then(Obj::as_name) == Some(b"Page") {
                        let res = self.get(d, b"Resources").as_dict().cloned().unwrap_or_default();
                        out.push((d.clone(), res));
                        if out.len() >= MAX_PAGES {
                            break;
                        }
                    }
                }
            }
        }
        out
    }

    fn walk_pages(&self, node: &Obj, inherited: &Dict, depth: usize, seen: &mut HashSet<u32>, out: &mut Vec<(Dict, Dict)>) {
        if depth > 32 || out.len() >= MAX_PAGES {
            return;
        }
        let Some(d) = node.as_dict() else { return };
        let res = match self.get(d, b"Resources").as_dict() {
            Some(r) => r.clone(),
            None => inherited.clone(),
        };
        let is_tree = d.get(b"Type".as_slice()).and_then(Obj::as_name) == Some(b"Pages")
            || (d.contains_key(b"Kids".as_slice()) && !d.contains_key(b"Contents".as_slice()));
        if !is_tree {
            out.push((d.clone(), res));
            return;
        }
        if let Obj::Array(kids) = self.get(d, b"Kids") {
            for kid in kids {
                if let Obj::Ref(r) = kid {
                    if !seen.insert(*r) {
                        continue;
                    }
                }
                self.walk_pages(self.resolve(kid), &res, depth + 1, seen, out);
            }
        }
    }

    fn page_contents(&self, page: &Dict) -> Vec<u8> {
        let mut out = Vec::new();
        let parts: Vec<&Obj> = match page.get(b"Contents".as_slice()).map(|o| (o, self.resolve(o))) {
            Some((_, Obj::Array(a))) => a.iter().collect(),
            Some((o, _)) => vec![o],
            None => Vec::new(),
        };
        for p in parts {
            if let Some(data) = self.stream_data(p) {
                out.extend_from_slice(&data);
                out.push(b'\n');
            }
        }
        out
    }

    /// Reads the text layer and metadata.
    pub fn text(&self) -> PdfText {
        let mut out = PdfText {
            encrypted: self.trailer.contains_key(b"Encrypt".as_slice()),
            title: self.info_string(b"Title"),
            producer: self.info_string(b"Producer"),
            creation_date: self.info_string(b"CreationDate"),
            mod_date: self.info_string(b"ModDate"),
            ..Default::default()
        };
        let pages = self.pages();
        out.pages = pages.len();
        if out.encrypted {
            // Strings are encrypted; reading them would produce noise.
            return out;
        }
        let mut fonts = FontCache::default();
        let mut ops = 0usize;
        let mut texts = Vec::new();
        for (page, res) in &pages {
            let content = self.page_contents(page);
            let mut w = TextWriter::default();
            let mut images = Vec::new();
            self.run_content(&content, res, &mut fonts, &mut w, &mut images, 0, &mut ops);
            out.page_images += images.len();
            let t = normalize(&w.s);
            if !t.is_empty() {
                texts.push(t);
            }
            if ops > MAX_OPS_PER_DOC {
                break;
            }
        }
        out.text = texts.join("\n\n");
        out.unsupported_filters = self.unsupported.borrow().clone();
        out
    }

    /// Page images that a vision model can read: JPEG data as stored, and
    /// 8-bit grey or RGB (and 1-bit grey) images re-encoded as PNG.
    pub fn images(&self, max_images: usize, max_bytes: usize) -> Vec<PageImage> {
        let mut out = Vec::new();
        let mut total = 0usize;
        let mut fonts = FontCache::default();
        let mut ops = 0usize;
        for (i, (page, res)) in self.pages().iter().enumerate() {
            let content = self.page_contents(page);
            let mut w = TextWriter::default();
            let mut images = Vec::new();
            self.run_content(&content, res, &mut fonts, &mut w, &mut images, 0, &mut ops);
            for img in images {
                if out.len() >= max_images {
                    return out;
                }
                if let Some((mime, bytes)) = self.image_bytes(&img) {
                    if total + bytes.len() > max_bytes {
                        continue;
                    }
                    total += bytes.len();
                    out.push(PageImage { page: i + 1, mime, bytes });
                }
            }
        }
        out
    }

    fn image_bytes(&self, img: &Obj) -> Option<(&'static str, Vec<u8>)> {
        let Obj::Stream(dict, s, e) = self.resolve(img) else { return None };
        let w = self.get(dict, b"Width").as_int()?;
        let h = self.get(dict, b"Height").as_int()?;
        if !(1..=20_000).contains(&w) || !(1..=20_000).contains(&h) || w * h < 1024 {
            return None; // icons and rules, not pages
        }
        let (filters, _) = self.filters(dict);
        if filters.last().map(Vec::as_slice) == Some(b"DCTDecode") || filters.last().map(Vec::as_slice) == Some(b"DCT") {
            let mut d2 = dict.clone();
            // Decode everything before the JPEG codec, keep the JPEG.
            let pre: Vec<Obj> = filters[..filters.len() - 1].iter().map(|f| Obj::Name(f.clone())).collect();
            d2.insert(b"Filter".to_vec(), Obj::Array(pre));
            d2.remove(b"DecodeParms".as_slice());
            let jpeg = self.decode(&d2, *s, *e).ok()?;
            return jpeg.starts_with(&[0xFF, 0xD8]).then_some(("image/jpeg", jpeg));
        }
        let pixels = self.decode(dict, *s, *e).ok()?;
        let image_mask = matches!(self.get(dict, b"ImageMask"), Obj::Bool(true));
        let bpc = if image_mask { 1 } else { self.get(dict, b"BitsPerComponent").as_int().unwrap_or(8) };
        let colors = if image_mask {
            1
        } else {
            match self.get(dict, b"ColorSpace") {
                Obj::Name(n) if n == b"DeviceGray" || n == b"G" || n == b"CalGray" => 1,
                Obj::Name(n) if n == b"DeviceRGB" || n == b"RGB" || n == b"CalRGB" => 3,
                Obj::Array(a) if a.first().and_then(|o| self.resolve(o).as_name()) == Some(b"ICCBased") => {
                    match a.get(1).map(|o| self.resolve(o)).and_then(Obj::as_dict).map(|d| self.get(d, b"N")) {
                        Some(Obj::Int(1)) => 1,
                        Some(Obj::Int(3)) => 3,
                        _ => return None,
                    }
                }
                _ => return None,
            }
        };
        if !(bpc == 8 || (bpc == 1 && colors == 1)) {
            return None;
        }
        let row = (w as usize * colors * bpc as usize).div_ceil(8);
        if pixels.len() < row * h as usize {
            return None;
        }
        let mut pixels = pixels;
        let invert = match self.get(dict, b"Decode") {
            Obj::Array(a) => a.first().and_then(Obj::as_num) == Some(1.0),
            _ => false,
        };
        if invert {
            for b in pixels.iter_mut() {
                *b = !*b;
            }
        }
        let png = encode_png(w as u32, h as u32, bpc as u8, colors == 3, &pixels[..row * h as usize]);
        Some(("image/png", png))
    }

    #[allow(clippy::too_many_arguments)]
    fn run_content(
        &self,
        content: &[u8],
        res: &Dict,
        fonts: &mut FontCache,
        w: &mut TextWriter,
        images: &mut Vec<Obj>,
        depth: usize,
        ops: &mut usize,
    ) {
        let font_dict = self.get(res, b"Font").as_dict();
        let xobjects = self.get(res, b"XObject").as_dict();
        let mut font: Option<Rc<Font>> = None;
        let mut stack: Vec<Obj> = Vec::new();
        let mut lx = Lexer::new(content, 0);
        while let Some(tok) = lx.value(0) {
            let Obj::Op(op) = tok else {
                if stack.len() < 64 {
                    stack.push(tok);
                }
                continue;
            };
            *ops += 1;
            if *ops > MAX_OPS_PER_DOC {
                return;
            }
            match op.as_slice() {
                b"BT" => w.begin_text(),
                b"Tf" => {
                    if let Some(Obj::Name(name)) = stack.first() {
                        font = font_dict
                            .and_then(|fd| fd.get(name.as_slice()))
                            .map(|r| fonts.get(self, r));
                    }
                }
                b"Td" | b"TD" => {
                    if let [.., tx, ty] = stack.as_slice() {
                        let (tx, ty) = (tx.as_num().unwrap_or(0.0), ty.as_num().unwrap_or(0.0));
                        if op == b"TD" {
                            w.leading = -ty;
                        }
                        w.move_by(tx, ty);
                    }
                }
                b"TL" => {
                    if let Some(l) = stack.last().and_then(Obj::as_num) {
                        w.leading = l;
                    }
                }
                b"Tm" => {
                    if let [.., _, _, _, _, _, f] = stack.as_slice() {
                        w.set_y(f.as_num().unwrap_or(0.0));
                    }
                }
                b"T*" => w.next_line(),
                b"Tj" => {
                    if let Some(Obj::Str(s)) = stack.last() {
                        w.show(&decode_with(font.as_deref(), s));
                    }
                }
                b"'" | b"\"" => {
                    w.next_line();
                    if let Some(Obj::Str(s)) = stack.last() {
                        w.show(&decode_with(font.as_deref(), s));
                    }
                }
                b"TJ" => {
                    if let Some(Obj::Array(parts)) = stack.last() {
                        let mut t = String::new();
                        for p in parts {
                            match p {
                                Obj::Str(s) => t.push_str(&decode_with(font.as_deref(), s)),
                                n if n.as_num().is_some_and(|v| v < -250.0) && !t.is_empty() && !t.ends_with(char::is_whitespace) => {
                                    t.push(' ');
                                }
                                _ => {}
                            }
                        }
                        w.show(&t);
                    }
                }
                b"Do" => {
                    if let (Some(Obj::Name(name)), Some(xo)) = (stack.last(), xobjects) {
                        if let Some(r) = xo.get(name.as_slice()) {
                            self.do_xobject(r, res, fonts, w, images, depth, ops);
                        }
                    }
                }
                b"ID" => {
                    // Inline image data runs until whitespace + EI.
                    let d = content;
                    let mut p = lx.pos + 1;
                    while p + 2 < d.len() {
                        if is_ws(d[p]) && d[p + 1] == b'E' && d[p + 2] == b'I' && (p + 3 == d.len() || !is_regular(d[p + 3])) {
                            break;
                        }
                        p += 1;
                    }
                    lx.pos = (p + 3).min(d.len());
                }
                _ => {}
            }
            stack.clear();
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn do_xobject(
        &self,
        r: &Obj,
        res: &Dict,
        fonts: &mut FontCache,
        w: &mut TextWriter,
        images: &mut Vec<Obj>,
        depth: usize,
        ops: &mut usize,
    ) {
        let Obj::Stream(dict, s, e) = self.resolve(r) else { return };
        match self.get(dict, b"Subtype").as_name() {
            Some(b"Image") => {
                if images.len() < 256 {
                    images.push(r.clone());
                }
            }
            Some(b"Form") if depth < MAX_FORM_DEPTH => {
                let Ok(content) = self.decode(dict, *s, *e) else { return };
                let form_res = self.get(dict, b"Resources").as_dict().cloned().unwrap_or_else(|| res.clone());
                self.run_content(&content, &form_res, fonts, w, images, depth + 1, ops);
            }
            _ => {}
        }
    }
}

/// `num gen obj` ending just before `i` (the position of `obj`).
fn object_number_before(d: &[u8], i: usize) -> Option<u32> {
    let mut j = i;
    let ws_end = j;
    while j > 0 && is_ws(d[j - 1]) {
        j -= 1;
    }
    if j == ws_end {
        return None;
    }
    let gen_end = j;
    while j > 0 && d[j - 1].is_ascii_digit() {
        j -= 1;
    }
    if j == gen_end || gen_end - j > 5 {
        return None;
    }
    let ws2 = j;
    while j > 0 && is_ws(d[j - 1]) {
        j -= 1;
    }
    if j == ws2 {
        return None;
    }
    let num_end = j;
    while j > 0 && d[j - 1].is_ascii_digit() {
        j -= 1;
    }
    if j == num_end || num_end - j > 10 {
        return None;
    }
    if j > 0 && is_regular(d[j - 1]) {
        return None;
    }
    std::str::from_utf8(&d[j..num_end]).ok()?.parse().ok()
}

fn inflate(data: &[u8], limit: usize) -> Result<Vec<u8>, Vec<u8>> {
    fn read_all(mut r: impl Read, limit: usize) -> (Vec<u8>, bool) {
        let mut out = Vec::new();
        let mut buf = [0u8; 16 * 1024];
        loop {
            match r.read(&mut buf) {
                Ok(0) => return (out, true),
                Ok(n) => {
                    if out.len() + n > limit {
                        let take = limit - out.len();
                        out.extend_from_slice(&buf[..take]);
                        return (out, true);
                    }
                    out.extend_from_slice(&buf[..n]);
                }
                // Keep what decoded before a corrupt tail.
                Err(_) => return (out, false),
            }
        }
    }
    let (out, ok) = read_all(ZlibDecoder::new(data), limit);
    if ok || !out.is_empty() {
        return Ok(out);
    }
    let (raw, _) = read_all(DeflateDecoder::new(data), limit);
    if raw.is_empty() {
        Err(b"FlateDecode".to_vec())
    } else {
        Ok(raw)
    }
}

fn png_unpredict(data: &[u8], params: &Dict, doc: &Doc<'_>) -> Vec<u8> {
    let get = |k: &[u8], default: i64| doc.get(params, k).as_int().unwrap_or(default);
    let predictor = get(b"Predictor", 1);
    if predictor < 10 {
        return data.to_vec();
    }
    let colors = get(b"Colors", 1).clamp(1, 32) as usize;
    let bpc = get(b"BitsPerComponent", 8).clamp(1, 16) as usize;
    let columns = get(b"Columns", 1).clamp(1, 1 << 20) as usize;
    let bpp = (colors * bpc).div_ceil(8).max(1);
    let row = (columns * colors * bpc).div_ceil(8);
    let mut out = Vec::with_capacity(data.len());
    let mut prev = vec![0u8; row];
    for chunk in data.chunks(row + 1) {
        if chunk.len() < row + 1 {
            break;
        }
        let ft = chunk[0];
        let mut cur = chunk[1..].to_vec();
        for i in 0..row {
            let a = if i >= bpp { cur[i - bpp] } else { 0 };
            let b = prev[i];
            let c = if i >= bpp { prev[i - bpp] } else { 0 };
            let add = match ft {
                1 => a,
                2 => b,
                3 => ((u16::from(a) + u16::from(b)) / 2) as u8,
                4 => paeth(a, b, c),
                _ => 0,
            };
            cur[i] = cur[i].wrapping_add(add);
        }
        out.extend_from_slice(&cur);
        prev = cur;
    }
    out
}

fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let p = i16::from(a) + i16::from(b) - i16::from(c);
    let (pa, pb, pc) = ((p - i16::from(a)).abs(), (p - i16::from(b)).abs(), (p - i16::from(c)).abs());
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

fn ascii_hex(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut hi = None;
    for &b in data {
        if b == b'>' {
            break;
        }
        let Some(v) = hex_val(b) else { continue };
        match hi.take() {
            Some(h) => out.push(h << 4 | v),
            None => hi = Some(v),
        }
    }
    if let Some(h) = hi {
        out.push(h << 4);
    }
    out
}

fn ascii85(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut group = [0u32; 5];
    let mut n = 0;
    let mut i = 0;
    let data = data.strip_prefix(b"<~").unwrap_or(data);
    while i < data.len() {
        let b = data[i];
        i += 1;
        match b {
            b'~' => break,
            b'z' if n == 0 => out.extend_from_slice(&[0, 0, 0, 0]),
            b'!'..=b'u' => {
                group[n] = u32::from(b - b'!');
                n += 1;
                if n == 5 {
                    let v = group.iter().fold(0u32, |acc, &g| acc.wrapping_mul(85).wrapping_add(g));
                    out.extend_from_slice(&v.to_be_bytes());
                    n = 0;
                }
            }
            _ => {}
        }
    }
    if n > 1 {
        for g in group.iter_mut().skip(n) {
            *g = 84;
        }
        let v = group.iter().fold(0u32, |acc, &g| acc.wrapping_mul(85).wrapping_add(g));
        out.extend_from_slice(&v.to_be_bytes()[..n - 1]);
    }
    out
}

/// Big-endian UTF-16 code units (a trailing odd byte is dropped).
fn be_units(b: &[u8]) -> Vec<u16> {
    b.as_chunks::<2>().0.iter().map(|c| u16::from_be_bytes(*c)).collect()
}

/// A PDF text string: UTF-16BE or UTF-8 with a BOM, else PDFDocEncoding
/// (read as Latin-1, which matches it for printable characters).
pub fn decode_text_string(s: &[u8]) -> String {
    if let Some(rest) = s.strip_prefix(&[0xFE, 0xFF]) {
        let units: Vec<u16> = be_units(rest);
        return String::from_utf16_lossy(&units);
    }
    if let Some(rest) = s.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8_lossy(rest).into_owned();
    }
    s.iter().map(|&b| char::from(b)).collect()
}

/// Collects shown text, starting a new line when the baseline moves.
#[derive(Default)]
struct TextWriter {
    s: String,
    y: f64,
    last_y: Option<f64>,
    moved: bool,
    new_line: bool,
    leading: f64,
}

impl TextWriter {
    fn begin_text(&mut self) {
        self.y = 0.0;
        self.moved = true;
    }

    fn move_by(&mut self, _tx: f64, ty: f64) {
        self.y += ty;
        self.moved = true;
    }

    fn set_y(&mut self, y: f64) {
        self.y = y;
        self.moved = true;
    }

    fn next_line(&mut self) {
        self.y -= self.leading;
        self.moved = true;
        self.new_line = true;
    }

    fn show(&mut self, t: &str) {
        if t.is_empty() || self.s.len() > 4 << 20 {
            return;
        }
        if let Some(last) = self.last_y {
            if self.new_line || (self.y - last).abs() > 0.01 {
                if !self.s.ends_with('\n') {
                    self.s.push('\n');
                }
            } else if self.moved && !self.s.ends_with(char::is_whitespace) && !t.starts_with(char::is_whitespace) {
                // CJK text has no spaces between words; producers that
                // position each glyph run separately must not gain them.
                let joins_cjk = self.s.chars().next_back().is_some_and(is_cjk) && t.chars().next().is_some_and(is_cjk);
                if !joins_cjk {
                    self.s.push(' ');
                }
            }
        }
        self.s.push_str(t);
        self.last_y = Some(self.y);
        self.moved = false;
        self.new_line = false;
    }
}

/// Han, kana, CJK punctuation and full-width forms: scripts written
/// without spaces between words.
fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x2E80..=0x2FDF | 0x3000..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF | 0xFF00..=0xFFEF | 0x20000..=0x3134F)
}

/// Kangxi radicals (U+2F00–U+2FD5) in the order of their code points, as
/// the unified ideographs they are compatibility-equivalent to (NFKC).
/// Some PDF producers map common characters such as 文 to the radical.
const KANGXI: &str = "一丨丶丿乙亅二亠人儿入八冂冖冫几凵刀力勹匕匚匸十卜卩厂厶又口囗土士夂夊夕大女子宀寸小尢尸屮山巛工己巾干幺广廴廾弋弓彐彡彳心戈戶手支攴文斗斤方无日曰月木欠止歹殳毋比毛氏气水火爪父爻爿片牙牛犬玄玉瓜瓦甘生用田疋疒癶白皮皿目矛矢石示禸禾穴立竹米糸缶网羊羽老而耒耳聿肉臣自至臼舌舛舟艮色艸虍虫血行衣襾見角言谷豆豕豸貝赤走足身車辛辰辵邑酉釆里金長門阜隶隹雨靑非面革韋韭音頁風飛食首香馬骨高髟鬥鬯鬲鬼魚鳥鹵鹿麥麻黃黍黑黹黽鼎鼓鼠鼻齊齒龍龜龠";

fn unify_radical(c: char) -> char {
    match c as u32 {
        cp @ 0x2F00..=0x2FD5 => KANGXI.chars().nth((cp - 0x2F00) as usize).unwrap_or(c),
        0x2E9F => '母',
        0x2EF3 => '龟',
        _ => c,
    }
}

/// Trims lines, drops control characters and collapses blank runs.
fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut blank = 0;
    for line in s.lines() {
        let line: String = line
            .chars()
            .filter(|c| !c.is_control() || *c == '\t')
            .map(unify_radical)
            .collect::<String>()
            .trim()
            .to_string();
        if line.is_empty() {
            blank += 1;
            continue;
        }
        if !out.is_empty() {
            out.push_str(if blank > 0 { "\n\n" } else { "\n" });
        }
        blank = 0;
        out.push_str(&line);
    }
    out
}

#[derive(Default)]
struct FontCache {
    by_ref: HashMap<u32, Rc<Font>>,
}

impl FontCache {
    fn get(&mut self, doc: &Doc<'_>, r: &Obj) -> Rc<Font> {
        if let Obj::Ref(n) = r {
            if let Some(f) = self.by_ref.get(n) {
                return f.clone();
            }
            let f = Rc::new(Font::load(doc, doc.resolve(r)));
            if self.by_ref.len() < 4096 {
                self.by_ref.insert(*n, f.clone());
            }
            return f;
        }
        Rc::new(Font::load(doc, r))
    }
}

enum Fallback {
    /// Predefined UCS-2 / UTF-16 CMaps (`UniGB-UCS2-H`, …): codes are UTF-16BE.
    Utf16,
    /// A simple font's byte → character table.
    Simple(Box<[Option<char>; 256]>),
    /// CIDs with no Unicode mapping: nothing can be read.
    None,
}

struct Font {
    /// Code lengths by codespace range: (length, low, high).
    codespace: Vec<(usize, u32, u32)>,
    two_byte_default: bool,
    to_unicode: HashMap<u32, String>,
    fallback: Fallback,
}

impl Font {
    fn load(doc: &Doc<'_>, font: &Obj) -> Font {
        let Some(d) = font.as_dict() else {
            return Font { codespace: Vec::new(), two_byte_default: false, to_unicode: HashMap::new(), fallback: Fallback::Simple(Box::new(win_ansi())) };
        };
        let composite = doc.get(d, b"Subtype").as_name() == Some(b"Type0");
        let mut font = Font {
            codespace: Vec::new(),
            two_byte_default: composite,
            to_unicode: HashMap::new(),
            fallback: Fallback::None,
        };
        if let Some(cmap) = d.get(b"ToUnicode".as_slice()).and_then(|o| doc.stream_data(o)) {
            font.parse_cmap(&cmap);
        }
        font.fallback = if composite {
            match doc.get(d, b"Encoding") {
                Obj::Name(n) if contains(n, b"UCS2") || contains(n, b"UTF16") => Fallback::Utf16,
                _ => Fallback::None,
            }
        } else {
            let base = match doc.get(d, b"Encoding") {
                Obj::Name(n) => Some(n.as_slice()),
                Obj::Dict(e) => doc.get(e, b"BaseEncoding").as_name(),
                _ => None,
            };
            let mut table = if base == Some(b"MacRomanEncoding") { mac_roman() } else { win_ansi() };
            if let Some(enc) = doc.get(d, b"Encoding").as_dict() {
                if let Obj::Array(diffs) = doc.get(enc, b"Differences") {
                    let mut code = 0usize;
                    for o in diffs {
                        match doc.resolve(o) {
                            Obj::Int(c) => code = (*c).clamp(0, 255) as usize,
                            Obj::Name(n) => {
                                if code < 256 {
                                    if let Some(c) = glyph_char(n) {
                                        table[code] = Some(c);
                                    }
                                }
                                code += 1;
                            }
                            _ => {}
                        }
                    }
                }
            }
            Fallback::Simple(Box::new(table))
        };
        font
    }

    fn parse_cmap(&mut self, data: &[u8]) {
        let mut lx = Lexer::new(data, 0);
        let mut operands: Vec<Obj> = Vec::new();
        let code = |s: &[u8]| s.iter().take(4).fold(0u32, |acc, &b| acc << 8 | u32::from(b));
        while let Some(tok) = lx.value(0) {
            match tok {
                Obj::Op(op) => {
                    match op.as_slice() {
                        b"endcodespacerange" => {
                            for pair in operands.chunks(2) {
                                if let [Obj::Str(lo), Obj::Str(hi)] = pair {
                                    if (1..=4).contains(&lo.len()) && lo.len() == hi.len() {
                                        self.codespace.push((lo.len(), code(lo), code(hi)));
                                    }
                                }
                            }
                        }
                        b"endbfchar" => {
                            for pair in operands.chunks(2) {
                                if let [Obj::Str(src), dst] = pair {
                                    if let Some(t) = cmap_dest(dst) {
                                        self.insert(code(src), t);
                                    }
                                }
                            }
                        }
                        b"endbfrange" => {
                            for triple in operands.chunks(3) {
                                if let [Obj::Str(lo), Obj::Str(hi), dst] = triple {
                                    let (lo, hi) = (code(lo), code(hi));
                                    if hi < lo || hi - lo > 0xFFFF {
                                        continue;
                                    }
                                    match dst {
                                        Obj::Str(base) => {
                                            let units: Vec<u16> = be_units(base);
                                            if units.is_empty() {
                                                continue;
                                            }
                                            for (i, c) in (lo..=hi).enumerate() {
                                                let mut u = units.clone();
                                                let last = u.len() - 1;
                                                u[last] = u[last].wrapping_add(i as u16);
                                                self.insert(c, String::from_utf16_lossy(&u));
                                            }
                                        }
                                        Obj::Array(items) => {
                                            for (c, item) in (lo..=hi).zip(items) {
                                                if let Some(t) = cmap_dest(item) {
                                                    self.insert(c, t);
                                                }
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                    operands.clear();
                }
                other => {
                    if operands.len() < 30_000 {
                        operands.push(other);
                    }
                }
            }
            if self.to_unicode.len() > MAX_CMAP_ENTRIES {
                break;
            }
        }
    }

    fn insert(&mut self, code: u32, text: String) {
        if self.to_unicode.len() < MAX_CMAP_ENTRIES {
            self.to_unicode.insert(code, text);
        }
    }

    fn decode(&self, s: &[u8]) -> String {
        let mut out = String::new();
        let mut utf16: Vec<u16> = Vec::new();
        let mut i = 0;
        while i < s.len() {
            let len = self.code_len(&s[i..]);
            let end = (i + len).min(s.len());
            let code = s[i..end].iter().fold(0u32, |acc, &b| acc << 8 | u32::from(b));
            i = end;
            if let Some(t) = self.to_unicode.get(&code) {
                out.push_str(&String::from_utf16_lossy(&std::mem::take(&mut utf16)));
                out.push_str(t);
                continue;
            }
            match &self.fallback {
                Fallback::Utf16 => utf16.push(code as u16),
                Fallback::Simple(table) => {
                    if let Some(c) = table.get(code as usize).copied().flatten() {
                        out.push(c);
                    }
                }
                Fallback::None => {}
            }
        }
        out.push_str(&String::from_utf16_lossy(&utf16));
        out
    }

    fn code_len(&self, s: &[u8]) -> usize {
        for len in 1..=4 {
            if s.len() < len {
                break;
            }
            let c = s[..len].iter().fold(0u32, |acc, &b| acc << 8 | u32::from(b));
            if self.codespace.iter().any(|&(l, lo, hi)| l == len && (lo..=hi).contains(&c)) {
                return len;
            }
        }
        if self.two_byte_default {
            2
        } else {
            1
        }
    }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

fn cmap_dest(o: &Obj) -> Option<String> {
    match o {
        Obj::Str(s) => {
            let units: Vec<u16> = be_units(s);
            Some(String::from_utf16_lossy(&units))
        }
        Obj::Name(n) => glyph_char(n).map(String::from),
        _ => None,
    }
}

fn decode_with(font: Option<&Font>, s: &[u8]) -> String {
    match font {
        Some(f) => f.decode(s),
        None => s.iter().map(|&b| char::from(b)).filter(|c| !c.is_control()).collect(),
    }
}

/// WinAnsiEncoding: Latin-1 with the cp1252 additions in 0x80–0x9F.
fn win_ansi() -> [Option<char>; 256] {
    const HIGH: [u16; 32] = [
        0x20AC, 0, 0x201A, 0x0192, 0x201E, 0x2026, 0x2020, 0x2021, 0x02C6, 0x2030, 0x0160, 0x2039, 0x0152, 0, 0x017D, 0,
        0, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013, 0x2014, 0x02DC, 0x2122, 0x0161, 0x203A, 0x0153, 0, 0x017E, 0x0178,
    ];
    let mut t = [None; 256];
    for (i, slot) in t.iter_mut().enumerate() {
        *slot = match i {
            0x20..=0x7E | 0xA0..=0xFF => char::from_u32(i as u32),
            0x80..=0x9F => char::from_u32(u32::from(HIGH[i - 0x80])).filter(|&c| c != '\0'),
            _ => None,
        };
    }
    t
}

/// MacRomanEncoding: ASCII plus Apple's upper half.
fn mac_roman() -> [Option<char>; 256] {
    const HIGH: &str = "ÄÅÇÉÑÖÜáàâäãåçéèêëíìîïñóòôöõúùûü†°¢£§•¶ß®©™´¨≠ÆØ∞±≤≥¥µ∂∑∏π∫ªºΩæø¿¡¬√ƒ≈∆«»…\u{a0}ÀÃÕŒœ–—“”‘’÷◊ÿŸ⁄€‹›ﬁﬂ‡·‚„‰ÂÊÁËÈÍÎÏÌÓÔ\u{f8ff}ÒÚÛÙıˆ˜¯˘˙˚¸˝˛ˇ";
    let mut t = [None; 256];
    for (i, slot) in t.iter_mut().enumerate().take(0x7F).skip(0x20) {
        *slot = char::from_u32(i as u32);
    }
    for (slot, c) in t[0x80..].iter_mut().zip(HIGH.chars()) {
        *slot = Some(c);
    }
    t
}

/// Unicode for common glyph names (Adobe Glyph List subset plus `uniXXXX`).
fn glyph_char(name: &[u8]) -> Option<char> {
    let name = std::str::from_utf8(name).ok()?;
    if name.len() == 1 && name.as_bytes()[0].is_ascii_alphabetic() {
        return name.chars().next();
    }
    if let Some(hex) = name.strip_prefix("uni") {
        if hex.len() == 4 {
            return u32::from_str_radix(hex, 16).ok().and_then(char::from_u32);
        }
    }
    if let Some(hex) = name.strip_prefix('u') {
        if (4..=6).contains(&hex.len()) && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return u32::from_str_radix(hex, 16).ok().and_then(char::from_u32);
        }
    }
    const NAMES: &[(&str, char)] = &[
        ("space", ' '), ("exclam", '!'), ("quotedbl", '"'), ("numbersign", '#'), ("dollar", '$'), ("percent", '%'),
        ("ampersand", '&'), ("quotesingle", '\''), ("parenleft", '('), ("parenright", ')'), ("asterisk", '*'),
        ("plus", '+'), ("comma", ','), ("hyphen", '-'), ("period", '.'), ("slash", '/'), ("zero", '0'), ("one", '1'),
        ("two", '2'), ("three", '3'), ("four", '4'), ("five", '5'), ("six", '6'), ("seven", '7'), ("eight", '8'),
        ("nine", '9'), ("colon", ':'), ("semicolon", ';'), ("less", '<'), ("equal", '='), ("greater", '>'),
        ("question", '?'), ("at", '@'), ("bracketleft", '['), ("backslash", '\\'), ("bracketright", ']'),
        ("asciicircum", '^'), ("underscore", '_'), ("grave", '`'), ("braceleft", '{'), ("bar", '|'),
        ("braceright", '}'), ("asciitilde", '~'), ("quoteleft", '\u{2018}'), ("quoteright", '\u{2019}'),
        ("quotedblleft", '\u{201C}'), ("quotedblright", '\u{201D}'), ("endash", '\u{2013}'), ("emdash", '\u{2014}'),
        ("bullet", '\u{2022}'), ("ellipsis", '\u{2026}'), ("degree", '\u{00B0}'), ("minus", '\u{2212}'),
        ("multiply", '\u{00D7}'), ("yen", '\u{00A5}'), ("Euro", '\u{20AC}'), ("sterling", '\u{00A3}'),
        ("copyright", '\u{00A9}'), ("registered", '\u{00AE}'), ("nbspace", '\u{00A0}'),
    ];
    NAMES.iter().find(|(n, _)| *n == name).map(|&(_, c)| c)
}

/// Encodes 8-bit grey/RGB or 1-bit grey rows as a PNG.
fn encode_png(w: u32, h: u32, bit_depth: u8, rgb: bool, pixels: &[u8]) -> Vec<u8> {
    use flate2::write::ZlibEncoder;
    use std::io::Write;
    let colors = if rgb { 3 } else { 1 };
    let row = (w as usize * colors * bit_depth as usize).div_ceil(8);
    let mut raw = Vec::with_capacity((row + 1) * h as usize);
    for r in pixels.chunks(row).take(h as usize) {
        raw.push(0);
        raw.extend_from_slice(r);
    }
    let mut enc = ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    let _ = enc.write_all(&raw);
    let idat = enc.finish().unwrap_or_default();
    let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[bit_depth, if rgb { 2 } else { 0 }, 0, 0, 0]);
    png_chunk(&mut out, b"IHDR", &ihdr);
    png_chunk(&mut out, b"IDAT", &idat);
    png_chunk(&mut out, b"IEND", &[]);
    out
}

fn png_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 { 0xEDB8_8320 ^ (crc >> 1) } else { crc >> 1 };
        }
    }
    !crc
}

/// `D:YYYYMMDDHHmmSSOHH'mm'` → (local date-time, offset in seconds if given).
pub fn parse_pdf_date(s: &str) -> Option<(chrono::NaiveDateTime, Option<i32>)> {
    let s = s.trim();
    let s = s.strip_prefix("D:").unwrap_or(s);
    let digits: String = s.chars().take_while(char::is_ascii_digit).collect();
    if digits.len() < 4 {
        return None;
    }
    let num = |a: usize, b: usize, default: u32| -> u32 { digits.get(a..b).and_then(|x| x.parse().ok()).unwrap_or(default) };
    let year: i32 = digits[..4].parse().ok()?;
    let date = chrono::NaiveDate::from_ymd_opt(year, num(4, 6, 1), num(6, 8, 1))?;
    let dt = date.and_hms_opt(num(8, 10, 0), num(10, 12, 0), num(12, 14, 0))?;
    let rest = &s[digits.len()..];
    let offset = match rest.chars().next() {
        Some('Z') => Some(0),
        Some(sign @ ('+' | '-')) => {
            let tz: String = rest[1..].chars().filter(char::is_ascii_digit).collect();
            let h: i32 = tz.get(0..2)?.parse().ok()?;
            let m: i32 = tz.get(2..4).and_then(|m| m.parse().ok()).unwrap_or(0);
            if h > 14 || m > 59 {
                return None;
            }
            let secs = h * 3600 + m * 60;
            Some(if sign == '-' { -secs } else { secs })
        }
        _ => None,
    };
    Some((dt, offset))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexes_values_and_references() {
        let mut lx = Lexer::new(b"<< /A 1 0 R /B [1 2.5 (a\\)b) <4E6F>] /C#20D true >>", 0);
        let Some(Obj::Dict(d)) = lx.value(0) else { panic!("not a dict") };
        assert_eq!(d[b"A".as_slice()], Obj::Ref(1));
        assert_eq!(
            d[b"B".as_slice()],
            Obj::Array(vec![Obj::Int(1), Obj::Real(2.5), Obj::Str(b"a)b".to_vec()), Obj::Str(b"No".to_vec())])
        );
        assert_eq!(d[b"C D".as_slice()], Obj::Bool(true));
    }

    #[test]
    fn content_numbers_are_not_references() {
        let mut lx = Lexer::new(b"1 0 0 1 72 760 Tm", 0);
        let mut toks = Vec::new();
        while let Some(t) = lx.value(0) {
            toks.push(t);
        }
        assert_eq!(toks.len(), 7);
        assert_eq!(toks[6], Obj::Op(b"Tm".to_vec()));
    }

    #[test]
    fn filters_and_dates() {
        assert_eq!(ascii_hex(b"48 65 6C6C 6F>"), b"Hello");
        assert_eq!(ascii85(b"<~87cURD]i,\"Ebo80~>"), b"Hello World!");
        let (dt, off) = parse_pdf_date("D:20261002093000+08'00'").unwrap();
        assert_eq!(dt.to_string(), "2026-10-02 09:30:00");
        assert_eq!(off, Some(8 * 3600));
        assert_eq!(parse_pdf_date("D:2026").unwrap().1, None);
        assert_eq!(decode_text_string(&[0xFE, 0xFF, 0x88, 0x4C]), "行");
    }

    #[test]
    fn png_crc_matches_the_reference_value() {
        assert_eq!(crc32(b"IEND"), 0xAE42_6082);
    }

    #[test]
    fn garbage_does_not_panic() {
        for data in [&b"%PDF-1.7\n% synthetic"[..], b"%PDF-1 obj obj 1 0 obj << /Length 99999999 >> stream\nxx", b"1 0 obj [[[[[["] {
            let t = Doc::parse(data).text();
            assert_eq!(t.text, "");
        }
    }
}
