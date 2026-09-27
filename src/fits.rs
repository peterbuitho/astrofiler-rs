//! Minimal, fast FITS reader/writer.
//!
//! Supports what an image-filing tool needs: header parsing for every HDU
//! (including gzip-wrapped files), reading 2-D/3-D image data of any standard
//! BITPIX with BZERO/BSCALE applied, reading row bands for memory-bounded
//! stacking, writing float32 / uint16 images, and rewriting a primary header.
//! Tile-compressed images (.fz) can have their headers read but not their data.

use anyhow::{anyhow, bail, Context, Result};
use flate2::read::GzDecoder;
use std::fs::File;
use std::io::{BufReader, BufWriter, Cursor, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub const BLOCK: usize = 2880;
const CARD: usize = 80;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Undefined,
    /// COMMENT / HISTORY / blank-keyword text.
    Commentary(String),
}

impl Value {
    /// Render the value the way Python's `str()` would, so database contents and
    /// generated filenames match the original AstroFiler byte-for-byte.
    pub fn to_py_string(&self) -> String {
        match self {
            Value::Str(s) | Value::Commentary(s) => s.clone(),
            Value::Int(i) => i.to_string(),
            Value::Float(f) => py_float_repr(*f),
            Value::Bool(b) => {
                if *b {
                    "True".into()
                } else {
                    "False".into()
                }
            }
            Value::Undefined => String::new(),
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Int(i) => Some(*i as f64),
            Value::Float(f) => Some(*f),
            Value::Str(s) => s.trim().parse().ok(),
            Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            _ => None,
        }
    }

    /// Python truthiness: empty strings, zero and undefined are false.
    pub fn is_truthy(&self) -> bool {
        match self {
            Value::Str(s) | Value::Commentary(s) => !s.trim().is_empty(),
            Value::Int(i) => *i != 0,
            Value::Float(f) => *f != 0.0,
            Value::Bool(b) => *b,
            Value::Undefined => false,
        }
    }
}

pub fn py_float_repr(f: f64) -> String {
    if f.is_finite() && f.fract() == 0.0 && f.abs() < 1e16 {
        format!("{f:.1}")
    } else {
        format!("{f}")
    }
}

#[derive(Debug, Clone)]
pub struct Card {
    pub key: String,
    pub value: Value,
    pub comment: String,
}

#[derive(Debug, Clone, Default)]
pub struct Header {
    pub cards: Vec<Card>,
}

impl Header {
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.cards
            .iter()
            .find(|c| c.key == key && !matches!(c.value, Value::Commentary(_)))
            .map(|c| &c.value)
    }

    pub fn contains(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    /// Value as a string, `None` when missing or undefined.
    pub fn get_str(&self, key: &str) -> Option<String> {
        match self.get(key)? {
            Value::Undefined => None,
            v => Some(v.to_py_string()),
        }
    }

    /// Value as a string only when it is truthy (mirrors `hdr.get(k)` in `if` tests).
    pub fn get_truthy(&self, key: &str) -> Option<String> {
        self.get(key)
            .filter(|v| v.is_truthy())
            .map(|v| v.to_py_string())
    }

    pub fn get_f64(&self, key: &str) -> Option<f64> {
        self.get(key)?.as_f64()
    }

    pub fn get_i64(&self, key: &str) -> Option<i64> {
        match self.get(key)? {
            Value::Int(i) => Some(*i),
            Value::Float(f) => Some(*f as i64),
            Value::Str(s) => s.trim().parse().ok(),
            _ => None,
        }
    }

    pub fn set(&mut self, key: &str, value: Value) {
        if let Some(card) = self
            .cards
            .iter_mut()
            .find(|c| c.key == key && !matches!(c.value, Value::Commentary(_)))
        {
            card.value = value;
        } else {
            self.cards.push(Card {
                key: key.to_string(),
                value,
                comment: String::new(),
            });
        }
    }

    pub fn set_with_comment(&mut self, key: &str, value: Value, comment: &str) {
        self.set(key, value);
        if let Some(card) = self.cards.iter_mut().find(|c| c.key == key) {
            card.comment = comment.to_string();
        }
    }

    pub fn add_history(&mut self, text: &str) {
        self.cards.push(Card {
            key: "HISTORY".into(),
            value: Value::Commentary(text.to_string()),
            comment: String::new(),
        });
    }

    pub fn remove(&mut self, key: &str) {
        self.cards.retain(|c| c.key != key);
    }

    fn naxes(&self) -> Vec<usize> {
        let n = self.get_i64("NAXIS").unwrap_or(0).max(0) as usize;
        (1..=n)
            .map(|i| self.get_i64(&format!("NAXIS{i}")).unwrap_or(0).max(0) as usize)
            .collect()
    }

    /// Size in bytes of the data unit (unpadded).
    fn data_len(&self) -> u64 {
        let bitpix = self.get_i64("BITPIX").unwrap_or(8);
        let naxes = self.naxes();
        if naxes.is_empty() {
            return 0;
        }
        let groups = matches!(self.get("GROUPS"), Some(Value::Bool(true)));
        let prod: u64 = naxes
            .iter()
            .skip(if groups { 1 } else { 0 })
            .map(|&n| n as u64)
            .product();
        let pcount = self.get_i64("PCOUNT").unwrap_or(0).max(0) as u64;
        let gcount = self.get_i64("GCOUNT").unwrap_or(1).max(1) as u64;
        (bitpix.unsigned_abs() / 8) * gcount * (pcount + prod)
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(BLOCK * 2);
        for card in &self.cards {
            if card.key == "END" {
                continue;
            }
            out.extend_from_slice(&format_card(card));
        }
        let mut end = [b' '; CARD];
        end[..3].copy_from_slice(b"END");
        out.extend_from_slice(&end);
        pad_to_block(&mut out, b' ');
        out
    }
}

fn pad_to_block(buf: &mut Vec<u8>, fill: u8) {
    let rem = buf.len() % BLOCK;
    if rem != 0 {
        buf.resize(buf.len() + BLOCK - rem, fill);
    }
}

fn format_card(card: &Card) -> [u8; CARD] {
    let mut s = match &card.value {
        Value::Commentary(text) => format!("{:<8}{}", card.key, text),
        Value::Str(v) => {
            let escaped = v.replace('\'', "''");
            let quoted = format!("'{escaped:<8}'");
            let mut s = format!("{:<8}= {:<20}", card.key, quoted);
            if !card.comment.is_empty() {
                s.push_str(" / ");
                s.push_str(&card.comment);
            }
            s
        }
        other => {
            let v = match other {
                Value::Int(i) => i.to_string(),
                Value::Float(f) => format_fits_float(*f),
                Value::Bool(b) => {
                    if *b {
                        "T".into()
                    } else {
                        "F".into()
                    }
                }
                _ => String::new(),
            };
            let mut s = format!("{:<8}= {:>20}", card.key, v);
            if !card.comment.is_empty() {
                s.push_str(" / ");
                s.push_str(&card.comment);
            }
            s
        }
    };
    // FITS headers are 7-bit ASCII.
    s.retain(|c| c.is_ascii() && !c.is_ascii_control());
    let mut out = [b' '; CARD];
    let bytes = s.as_bytes();
    let n = bytes.len().min(CARD);
    out[..n].copy_from_slice(&bytes[..n]);
    out
}

fn format_fits_float(f: f64) -> String {
    if !f.is_finite() {
        return "0.0".into();
    }
    let s = py_float_repr(f);
    if s.len() <= 20 {
        s
    } else {
        format!("{f:.12E}")
    }
}

fn parse_card(raw: &[u8]) -> Card {
    let text: String = raw
        .iter()
        .map(|&b| if b.is_ascii() { b as char } else { ' ' })
        .collect();
    let key = text[..8].trim_end().to_string();
    let has_value = &text[8..10] == "= " && key != "COMMENT" && key != "HISTORY" && !key.is_empty();
    if !has_value {
        return Card {
            key,
            value: Value::Commentary(text[8..].trim_end().to_string()),
            comment: String::new(),
        };
    }
    let rest = text[10..].trim_start();
    if let Some(stripped) = rest.strip_prefix('\'') {
        // String value: '' is an escaped quote.
        let chars: Vec<char> = stripped.chars().collect();
        let mut value = String::new();
        let mut i = 0;
        while i < chars.len() {
            if chars[i] == '\'' {
                if i + 1 < chars.len() && chars[i + 1] == '\'' {
                    value.push('\'');
                    i += 2;
                    continue;
                }
                break;
            }
            value.push(chars[i]);
            i += 1;
        }
        let after: String = chars
            .get(i + 1..)
            .map(|c| c.iter().collect())
            .unwrap_or_default();
        let comment = after
            .split_once('/')
            .map(|(_, c)| c.trim().to_string())
            .unwrap_or_default();
        return Card {
            key,
            value: Value::Str(value.trim_end().to_string()),
            comment,
        };
    }
    let (val, comment) = match rest.split_once('/') {
        Some((v, c)) => (v.trim(), c.trim().to_string()),
        None => (rest.trim(), String::new()),
    };
    let value = if val.is_empty() {
        Value::Undefined
    } else if val == "T" {
        Value::Bool(true)
    } else if val == "F" {
        Value::Bool(false)
    } else if let Ok(i) = val.parse::<i64>() {
        Value::Int(i)
    } else if let Ok(f) = val.replace(['D', 'd'], "E").parse::<f64>() {
        Value::Float(f)
    } else {
        Value::Str(val.to_string())
    };
    Card {
        key,
        value,
        comment,
    }
}

/// Read one header starting at the reader's current position. Returns the header
/// and the number of bytes consumed (a multiple of 2880).
fn read_header<R: Read>(r: &mut R) -> Result<Option<(Header, u64)>> {
    let mut header = Header::default();
    let mut block = vec![0u8; BLOCK];
    let mut consumed = 0u64;
    loop {
        match r.read_exact(&mut block) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                if consumed == 0 {
                    return Ok(None);
                }
                bail!("truncated FITS header");
            }
            Err(e) => return Err(e.into()),
        }
        if consumed == 0 && !(block.starts_with(b"SIMPLE  ") || block.starts_with(b"XTENSION")) {
            if header.cards.is_empty() && block.iter().all(|&b| b == 0 || b == b' ') {
                return Ok(None); // trailing padding
            }
            bail!("not a FITS file (missing SIMPLE/XTENSION)");
        }
        consumed += BLOCK as u64;
        for raw in block.chunks_exact(CARD) {
            if &raw[..8] == b"END     " {
                merge_continue_cards(&mut header);
                return Ok(Some((header, consumed)));
            }
            header.cards.push(parse_card(raw));
        }
        if consumed > 1_000 * BLOCK as u64 {
            bail!("FITS header has no END card");
        }
    }
}

/// Fold long-string CONTINUE cards into the preceding string value.
fn merge_continue_cards(header: &mut Header) {
    let mut merged: Vec<Card> = Vec::with_capacity(header.cards.len());
    for card in header.cards.drain(..) {
        if card.key == "CONTINUE" {
            if let (Some(prev), Value::Commentary(text)) = (merged.last_mut(), &card.value) {
                if let Value::Str(s) = &mut prev.value {
                    if s.ends_with('&') {
                        s.pop();
                        let t = text.trim_start();
                        let part = t
                            .strip_prefix('\'')
                            .and_then(|t| t.split('\'').next())
                            .unwrap_or("");
                        s.push_str(part);
                        continue;
                    }
                }
            }
        }
        merged.push(card);
    }
    header.cards = merged;
}

pub fn is_gzip(path: &Path) -> bool {
    let mut magic = [0u8; 2];
    File::open(path)
        .and_then(|mut f| f.read_exact(&mut magic))
        .is_ok()
        && magic == [0x1f, 0x8b]
}

/// Read only the primary header. This is the hot path of repository ingest, so it
/// touches only the first few 2880-byte blocks of each file.
pub fn read_primary_header(path: &Path) -> Result<Header> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    // Sniff gzip from the same handle: every extra open is a round trip to a NAS.
    let mut buffered = BufReader::with_capacity(BLOCK * 4, file);
    let gzip = std::io::BufRead::fill_buf(&mut buffered)?.starts_with(&[0x1f, 0x8b]);
    let mut reader: Box<dyn Read> = if gzip {
        Box::new(GzDecoder::new(buffered))
    } else {
        Box::new(buffered)
    };
    let (mut header, _) = read_header(&mut reader)?.ok_or_else(|| anyhow!("empty FITS file"))?;
    // Tile-compressed files keep their real metadata in the first extension.
    if header.get_i64("NAXIS") == Some(0) && header.cards.len() < 12 {
        let skip = header.data_len().div_ceil(BLOCK as u64) * BLOCK as u64;
        std::io::copy(&mut reader.by_ref().take(skip), &mut std::io::sink())?;
        if let Some((ext, _)) = read_header(&mut reader)? {
            if matches!(ext.get("ZIMAGE"), Some(Value::Bool(true))) {
                for card in ext.cards {
                    if header.get(&card.key).is_none() && !card.key.starts_with('Z') {
                        header.cards.push(card);
                    }
                }
            }
        }
    }
    Ok(header)
}

#[derive(Debug, Clone)]
pub struct HduInfo {
    pub header: Header,
    pub data_start: u64,
    pub data_len: u64,
}

/// A FITS file opened for random access (gzip files are decompressed in memory).
pub struct FitsFile {
    source: Source,
    pub hdus: Vec<HduInfo>,
}

enum Source {
    File(BufReader<File>),
    Mem(Cursor<Vec<u8>>),
}

impl Read for Source {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Source::File(f) => f.read(buf),
            Source::Mem(m) => m.read(buf),
        }
    }
}
impl Seek for Source {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        match self {
            Source::File(f) => f.seek(pos),
            Source::Mem(m) => m.seek(pos),
        }
    }
}

impl FitsFile {
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let mut buffered = BufReader::with_capacity(1 << 20, file);
        let source = if std::io::BufRead::fill_buf(&mut buffered)?.starts_with(&[0x1f, 0x8b]) {
            let mut buf = Vec::new();
            GzDecoder::new(buffered).read_to_end(&mut buf)?;
            Source::Mem(Cursor::new(buf))
        } else {
            Source::File(buffered)
        };
        Self::parse(source, path)
    }

    /// A whole file already in memory (gzip or plain).
    pub fn from_bytes(bytes: Vec<u8>, path: &Path) -> Result<Self> {
        let bytes = if bytes.starts_with(&[0x1f, 0x8b]) {
            let mut buf = Vec::new();
            GzDecoder::new(&bytes[..]).read_to_end(&mut buf)?;
            buf
        } else {
            bytes
        };
        Self::parse(Source::Mem(Cursor::new(bytes)), path)
    }

    fn parse(mut source: Source, path: &Path) -> Result<Self> {
        let mut hdus = Vec::new();
        let mut pos = 0u64;
        loop {
            source.seek(SeekFrom::Start(pos))?;
            let Some((header, consumed)) = read_header(&mut source).or_else(|e| {
                if hdus.is_empty() {
                    Err(e)
                } else {
                    Ok(None)
                }
            })?
            else {
                break;
            };
            let data_start = pos + consumed;
            let data_len = header.data_len();
            pos = data_start + data_len.div_ceil(BLOCK as u64) * BLOCK as u64;
            hdus.push(HduInfo {
                header,
                data_start,
                data_len,
            });
        }
        if hdus.is_empty() {
            bail!("no HDUs in {}", path.display());
        }
        Ok(FitsFile { source, hdus })
    }

    /// Index of the first HDU holding a 2-D (or deeper) uncompressed image.
    pub fn image_hdu(&self) -> Result<usize> {
        for (i, hdu) in self.hdus.iter().enumerate() {
            let h = &hdu.header;
            if matches!(h.get("ZIMAGE"), Some(Value::Bool(true))) {
                bail!(
                    "tile-compressed FITS data is not supported; decompress with `funpack` first"
                );
            }
            let xt = h.get_str("XTENSION").unwrap_or_default();
            if (i == 0 || xt.trim() == "IMAGE") && h.naxes().len() >= 2 && hdu.data_len > 0 {
                return Ok(i);
            }
        }
        bail!("no image data found")
    }

    pub fn shape(&self, hdu: usize) -> ImageShape {
        let n = self.hdus[hdu].header.naxes();
        ImageShape {
            width: n[0],
            height: n[1],
            planes: n.get(2).copied().unwrap_or(1).max(1),
        }
    }

    /// Read `nrows` rows starting at `row` (rows run over height*planes), as f32
    /// with BZERO/BSCALE applied.
    pub fn read_rows(&mut self, hdu: usize, row: usize, nrows: usize) -> Result<Vec<f32>> {
        let info = &self.hdus[hdu];
        let h = &info.header;
        let bitpix = h.get_i64("BITPIX").unwrap_or(16);
        let bzero = h.get_f64("BZERO").unwrap_or(0.0);
        let bscale = h.get_f64("BSCALE").unwrap_or(1.0);
        let width = h.naxes()[0];
        let bpp = (bitpix.unsigned_abs() / 8) as usize;
        let offset = info.data_start + (row * width * bpp) as u64;
        let count = nrows * width;
        let mut raw = vec![0u8; count * bpp];
        self.source.seek(SeekFrom::Start(offset))?;
        self.source
            .read_exact(&mut raw)
            .context("reading image data")?;
        Ok(decode_pixels(&raw, bitpix, bzero, bscale))
    }

    pub fn read_image(&mut self) -> Result<Image> {
        let hdu = self.image_hdu()?;
        let shape = self.shape(hdu);
        let data = self.read_rows(hdu, 0, shape.height * shape.planes)?;
        Ok(Image {
            header: self.hdus[hdu].header.clone(),
            shape,
            data,
        })
    }
}

fn decode_pixels(raw: &[u8], bitpix: i64, bzero: f64, bscale: f64) -> Vec<f32> {
    let scale = |v: f64| (v * bscale + bzero) as f32;
    let identity = bzero == 0.0 && bscale == 1.0;
    match bitpix {
        8 => raw.iter().map(|&b| scale(b as f64)).collect(),
        16 => raw
            .chunks_exact(2)
            .map(|c| scale(i16::from_be_bytes([c[0], c[1]]) as f64))
            .collect(),
        32 => raw
            .chunks_exact(4)
            .map(|c| scale(i32::from_be_bytes([c[0], c[1], c[2], c[3]]) as f64))
            .collect(),
        64 => raw
            .chunks_exact(8)
            .map(|c| scale(i64::from_be_bytes(c.try_into().unwrap()) as f64))
            .collect(),
        -32 => raw
            .chunks_exact(4)
            .map(|c| {
                let v = f32::from_be_bytes([c[0], c[1], c[2], c[3]]);
                if identity {
                    v
                } else {
                    scale(v as f64)
                }
            })
            .collect(),
        -64 => raw
            .chunks_exact(8)
            .map(|c| scale(f64::from_be_bytes(c.try_into().unwrap())))
            .collect(),
        _ => Vec::new(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageShape {
    pub width: usize,
    pub height: usize,
    pub planes: usize,
}

impl ImageShape {
    pub fn len(&self) -> usize {
        self.width * self.height * self.planes
    }
    pub fn rows(&self) -> usize {
        self.height * self.planes
    }
}

#[derive(Debug, Clone)]
pub struct Image {
    pub header: Header,
    pub shape: ImageShape,
    pub data: Vec<f32>,
}

/// Read the first image of a file. The whole file is read up front (in
/// parallel parts when big), which is much faster from a NAS.
pub fn read_image(path: &Path) -> Result<Image> {
    let bytes =
        crate::util::read_file(path).with_context(|| format!("opening {}", path.display()))?;
    FitsFile::from_bytes(bytes, path)?.read_image()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutType {
    F32,
    U16,
}

/// Keys that describe data layout and must be regenerated when writing.
const STRUCTURAL: &[&str] = &[
    "SIMPLE", "BITPIX", "NAXIS", "NAXIS1", "NAXIS2", "NAXIS3", "EXTEND", "BZERO", "BSCALE",
    "XTENSION", "PCOUNT", "GCOUNT", "END", "CHECKSUM", "DATASUM",
];

/// Write a single-HDU image. Metadata cards are copied from `template`.
pub fn write_image(
    path: &Path,
    template: &Header,
    shape: ImageShape,
    data: &[f32],
    out: OutType,
) -> Result<()> {
    let mut h = Header::default();
    h.set("SIMPLE", Value::Bool(true));
    h.set(
        "BITPIX",
        Value::Int(if out == OutType::F32 { -32 } else { 16 }),
    );
    h.set("NAXIS", Value::Int(if shape.planes > 1 { 3 } else { 2 }));
    h.set("NAXIS1", Value::Int(shape.width as i64));
    h.set("NAXIS2", Value::Int(shape.height as i64));
    if shape.planes > 1 {
        h.set("NAXIS3", Value::Int(shape.planes as i64));
    }
    if out == OutType::U16 {
        h.set("BZERO", Value::Int(32768));
        h.set("BSCALE", Value::Int(1));
    }
    for card in &template.cards {
        if STRUCTURAL.contains(&card.key.as_str()) || card.key.starts_with("NAXIS") {
            continue;
        }
        h.cards.push(card.clone());
    }

    let tmp = tmp_path(path);
    {
        let mut w = BufWriter::with_capacity(1 << 20, File::create(&tmp)?);
        w.write_all(&h.to_bytes())?;
        let mut buf = Vec::with_capacity(data.len() * 4);
        match out {
            OutType::F32 => {
                for &v in data {
                    buf.extend_from_slice(&v.to_be_bytes());
                }
            }
            OutType::U16 => {
                for &v in data {
                    let v = if v.is_finite() {
                        v.round().clamp(0.0, 65535.0)
                    } else {
                        0.0
                    };
                    buf.extend_from_slice(&((v as i32 - 32768) as i16).to_be_bytes());
                }
            }
        }
        pad_to_block(&mut buf, 0);
        w.write_all(&buf)?;
        w.flush()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".astrofiler-tmp");
    path.with_file_name(name)
}

/// SHA-256 of the file as it would be after [`rewrite_primary_header`] with
/// `header`, computed without writing anything.
pub fn sha256_with_header(path: &Path, header: &Header) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut file = File::open(path)?;
    let (_, old_len) =
        read_header(&mut BufReader::new(&mut file))?.ok_or_else(|| anyhow!("empty FITS file"))?;
    let mut hasher = Sha256::new();
    hasher.update(header.to_bytes());
    file.seek(SeekFrom::Start(old_len))?;
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Replace the primary header of an uncompressed FITS file, keeping the data.
pub fn rewrite_primary_header(path: &Path, header: &Header) -> Result<()> {
    if is_gzip(path) {
        bail!("refusing to rewrite header of gzip-compressed file");
    }
    let mut file = File::open(path)?;
    let (_, old_len) =
        read_header(&mut BufReader::new(&mut file))?.ok_or_else(|| anyhow!("empty FITS file"))?;
    let new_bytes = header.to_bytes();
    if new_bytes.len() as u64 == old_len {
        let mut f = std::fs::OpenOptions::new().write(true).open(path)?;
        f.write_all(&new_bytes)?;
        return Ok(());
    }
    let tmp = tmp_path(path);
    {
        let mut src = File::open(path)?;
        src.seek(SeekFrom::Start(old_len))?;
        let mut w = BufWriter::new(File::create(&tmp)?);
        w.write_all(&new_bytes)?;
        std::io::copy(&mut src, &mut w)?;
        w.flush()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cards() {
        let c = parse_card(format!("{:<80}", "OBJECT  = 'M 31    '           / target").as_bytes());
        assert_eq!(c.value, Value::Str("M 31".into()));
        assert_eq!(c.comment, "target");
        let c = parse_card(format!("{:<80}", "EXPTIME =                300.0").as_bytes());
        assert_eq!(c.value, Value::Float(300.0));
        assert_eq!(c.value.to_py_string(), "300.0");
        let c = parse_card(format!("{:<80}", "XBINNING=                    1 / bin").as_bytes());
        assert_eq!(c.value, Value::Int(1));
        let c = parse_card(format!("{:<80}", "NOTE    = 'it''s'").as_bytes());
        assert_eq!(c.value, Value::Str("it's".into()));
        let c = parse_card(format!("{:<80}", "HISTORY hello").as_bytes());
        assert_eq!(c.value, Value::Commentary("hello".into()));
    }

    #[test]
    fn roundtrip_image() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.fits");
        let mut h = Header::default();
        h.set("OBJECT", Value::Str("M42".into()));
        h.set("EXPTIME", Value::Float(1.5));
        let shape = ImageShape {
            width: 7,
            height: 5,
            planes: 1,
        };
        let data: Vec<f32> = (0..35).map(|v| v as f32 * 10.0).collect();
        write_image(&p, &h, shape, &data, OutType::U16).unwrap();
        let img = read_image(&p).unwrap();
        assert_eq!(img.shape, shape);
        assert_eq!(img.data, data);
        assert_eq!(img.header.get_str("OBJECT").as_deref(), Some("M42"));
        let hdr = read_primary_header(&p).unwrap();
        assert_eq!(hdr.get_f64("EXPTIME"), Some(1.5));

        write_image(&p, &h, shape, &data, OutType::F32).unwrap();
        let mut f = FitsFile::open(&p).unwrap();
        let rows = f.read_rows(0, 2, 2).unwrap();
        assert_eq!(rows, data[14..28].to_vec());
    }

    #[test]
    fn rewrite_header_keeps_data() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.fits");
        let shape = ImageShape {
            width: 4,
            height: 4,
            planes: 1,
        };
        let data: Vec<f32> = (0..16).map(|v| v as f32).collect();
        write_image(&p, &Header::default(), shape, &data, OutType::F32).unwrap();
        let mut h = read_primary_header(&p).unwrap();
        for i in 0..60 {
            h.set(&format!("KEY{i}"), Value::Int(i));
        }
        let expected = sha256_with_header(&p, &h).unwrap();
        rewrite_primary_header(&p, &h).unwrap();
        assert_eq!(crate::util::sha256_file(&p).unwrap(), expected);
        let img = read_image(&p).unwrap();
        assert_eq!(img.data, data);
        assert_eq!(img.header.get_i64("KEY59"), Some(59));
    }
}
