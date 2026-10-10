//! XISF (PixInsight) support. XISF files are catalogued as they are: their
//! FITS keywords are read from the XML header and can be rewritten there.
//! Converting one to FITS is still available as a separate command.
//!
//! Supports monolithic XISF 1.0 files with attached or inline image data,
//! UInt8/16/32 and Float32/64 samples, planar or interleaved ("normal") pixel
//! storage, zlib / lz4 / lz4hc / zstd compression with optional byte shuffling,
//! and FITSKeyword metadata.

use crate::fits::{self, Card, Header, ImageShape, OutType, Value};
use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use std::io::{Read, Seek, SeekFrom, Write};
use std::ops::Range;
use std::path::Path;

const SIGNATURE: &[u8; 8] = b"XISF0100";
/// Signature, header length and reserved bytes before the XML header.
const PREAMBLE: usize = 16;
/// Image data moves in steps of this size when the header outgrows its space.
const ALIGN: u64 = 4096;

/// XISF properties PixInsight keeps alongside the FITS keywords.
const PROPERTIES: &[(&str, &str)] = &[
    ("OBJECT", "Observation:Object:Name"),
    ("TELESCOP", "Instrument:Telescope:Name"),
    ("INSTRUME", "Instrument:Camera:Name"),
    ("FILTER", "Instrument:Filter:Name"),
    ("OBSERVER", "Observer:Name"),
];

pub fn is_xisf(path: &Path) -> bool {
    let mut magic = [0u8; 8];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut magic))
        .is_ok()
        && &magic == SIGNATURE
}

/// The XML header as stored, without the padding after it.
fn read_xml(file: &mut std::fs::File) -> Result<String> {
    let mut pre = [0u8; PREAMBLE];
    file.read_exact(&mut pre)
        .map_err(|_| anyhow!("not an XISF 1.0 file"))?;
    if &pre[..8] != SIGNATURE {
        bail!("not an XISF 1.0 file");
    }
    let hlen = u32::from_le_bytes(pre[8..12].try_into().unwrap()) as usize;
    if hlen > 64 << 20 {
        bail!("XISF header too large ({hlen} bytes); the file is probably damaged");
    }
    let mut xml = vec![0u8; hlen];
    file.read_exact(&mut xml)
        .map_err(|_| anyhow!("truncated XISF header"))?;
    let mut xml = String::from_utf8(xml).context("XISF header is not UTF-8")?;
    xml.truncate(xml.trim_end_matches('\0').len());
    Ok(xml)
}

/// Some writers (e.g. PixInsight processing history) leave control characters
/// in the XML that strict parsers reject; blank them out. Every character
/// keeps its place, so positions in the result are positions in the original.
fn sanitise(xml: &str) -> String {
    xml.chars()
        .map(|c| {
            if (c as u32) < 0x20 && !matches!(c, '\t' | '\n' | '\r') {
                ' '
            } else {
                c
            }
        })
        .collect()
}

fn image_node<'a>(doc: &'a roxmltree::Document<'a>) -> Result<roxmltree::Node<'a, 'a>> {
    doc.descendants()
        .find(|n| n.has_tag_name("Image"))
        .ok_or_else(|| anyhow!("XISF file has no Image element"))
}

fn card_of(kw: roxmltree::Node) -> Option<Card> {
    let key = kw.attribute("name").unwrap_or("").trim().to_uppercase();
    if key.is_empty() {
        return None;
    }
    let raw_val = kw.attribute("value").unwrap_or("");
    let comment = kw.attribute("comment").unwrap_or("").to_string();
    let value = if key == "COMMENT" || key == "HISTORY" {
        Value::Commentary(if comment.is_empty() {
            raw_val.to_string()
        } else {
            comment.clone()
        })
    } else {
        parse_value(raw_val)
    };
    Some(Card {
        key,
        value,
        comment,
    })
}

fn keywords(image: roxmltree::Node) -> Header {
    Header {
        cards: image
            .children()
            .filter(|n| n.has_tag_name("FITSKeyword"))
            .filter_map(card_of)
            .collect(),
    }
}

/// The FITS keywords of an XISF file. Only the XML header is read.
pub fn read_header(path: &Path) -> Result<Header> {
    let mut file =
        std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let xml = sanitise(&read_xml(&mut file)?);
    let doc = roxmltree::Document::parse(&xml).context("parsing XISF XML header")?;
    Ok(keywords(image_node(&doc)?))
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn keyword_xml(card: &Card) -> String {
    let (value, comment) = match &card.value {
        Value::Commentary(text) => (String::new(), text.clone()),
        Value::Str(s) => (format!("'{}'", s.replace('\'', "''")), card.comment.clone()),
        Value::Bool(b) => (
            (if *b { "T" } else { "F" }).to_string(),
            card.comment.clone(),
        ),
        Value::Int(i) => (i.to_string(), card.comment.clone()),
        Value::Float(f) => (fits::py_float_repr(*f), card.comment.clone()),
        Value::Undefined => (String::new(), card.comment.clone()),
    };
    format!(
        r#"<FITSKeyword name="{}" value="{}" comment="{}"/>"#,
        escape(&card.key),
        escape(&value),
        escape(&comment)
    )
}

/// The start of an XISF file with `header` in place of its keywords, and
/// where the image data it is followed by starts in the file as it is now.
struct Rewrite {
    /// Everything before the image data in the rewritten file.
    prefix: Vec<u8>,
    /// Start of the image data in the current file (None: all data is inline).
    data_start: Option<u64>,
}

fn plan_rewrite(path: &Path, header: &Header) -> Result<Rewrite> {
    let mut file =
        std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let original = read_xml(&mut file)?;
    let clean = sanitise(&original);
    let doc = roxmltree::Document::parse(&clean).context("parsing XISF XML header")?;
    if doc.descendants().any(|n| n.has_tag_name("Signature")) {
        bail!("this XISF file is digitally signed; editing it would break the signature");
    }
    let image = image_node(&doc)?;
    let current: Vec<(Card, Range<usize>)> = image
        .children()
        .filter(|n| n.has_tag_name("FITSKeyword"))
        .filter_map(|n| card_of(n).map(|c| (c, n.range())))
        .collect();
    let commentary = |c: &Card| matches!(c.value, Value::Commentary(_));

    // Text to replace, and keywords to add; only what differs is touched, so
    // values the file already has keep their exact spelling.
    let mut edits: Vec<(Range<usize>, String)> = Vec::new();
    let mut added = String::new();
    let mut changed: Vec<&Card> = Vec::new();
    let mut seen_commentary: std::collections::HashMap<&str, usize> = Default::default();
    for card in &header.cards {
        if commentary(card) {
            let n = seen_commentary.entry(card.key.as_str()).or_default();
            *n += 1;
            let have = current
                .iter()
                .filter(|(c, _)| commentary(c) && c.key == card.key)
                .count();
            if *n > have {
                added.push_str(&keyword_xml(card));
            }
            continue;
        }
        match current
            .iter()
            .find(|(c, _)| !commentary(c) && c.key == card.key)
        {
            Some((c, _)) if c.value == card.value && c.comment == card.comment => {}
            Some((_, range)) => {
                edits.push((range.clone(), keyword_xml(card)));
                changed.push(card);
            }
            None => {
                added.push_str(&keyword_xml(card));
                changed.push(card);
            }
        }
    }
    for (c, range) in &current {
        if !commentary(c) && header.get(&c.key).is_none() {
            edits.push((range.clone(), String::new()));
        }
    }
    if !added.is_empty() {
        match current.last() {
            Some((_, range)) => edits.push((range.end..range.end, added)),
            None => {
                let range = image.range();
                let text = &clean[range.clone()];
                if text.ends_with("/>") {
                    edits.push((range.end - 2..range.end, format!(">{added}</Image>")));
                } else {
                    let close = text
                        .rfind("</")
                        .ok_or_else(|| anyhow!("unexpected XISF Image element"))?;
                    edits.push((range.start + close..range.start + close, added));
                }
            }
        }
    }
    // PixInsight shows some of the same values as properties.
    for card in changed {
        let (Value::Str(text), Some((_, id))) = (
            &card.value,
            PROPERTIES.iter().find(|(key, _)| *key == card.key),
        ) else {
            continue;
        };
        let Some(prop) = image.children().find(|n| {
            n.has_tag_name("Property")
                && n.attribute("id") == Some(id)
                && n.attribute("type") == Some("String")
                && n.attribute("location").is_none()
        }) else {
            continue;
        };
        if let Some(attr) = prop.attribute_node("value") {
            edits.push((attr.range_value(), escape(text)));
        } else if let Some(t) = prop.children().find(|n| n.is_text()) {
            edits.push((t.range(), escape(text)));
        }
    }

    // Where each block of data is, as the header records it.
    let mut attachments: Vec<(Range<usize>, u64, String)> = Vec::new();
    for node in doc.descendants() {
        let Some(attr) = node.attribute_node("location") else {
            continue;
        };
        let mut parts = attr.value().splitn(3, ':');
        if parts.next() != Some("attachment") {
            continue;
        }
        let pos: u64 = parts
            .next()
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| anyhow!("bad location"))?;
        let rest = parts.next().unwrap_or("").to_string();
        attachments.push((attr.range_value(), pos, rest));
    }
    let render = |shift: u64| -> Result<Vec<u8>> {
        let mut all = edits.clone();
        for (range, pos, rest) in &attachments {
            all.push((range.clone(), format!("attachment:{}:{rest}", pos + shift)));
        }
        all.sort_by_key(|(r, _)| (r.start, r.end));
        if all.windows(2).any(|w| w[0].0.end > w[1].0.start) {
            bail!("overlapping edits in the XISF header");
        }
        let mut out = original.clone();
        for (range, text) in all.iter().rev() {
            out.replace_range(range.clone(), text);
        }
        Ok(out.into_bytes())
    };
    let preamble = |xml: &[u8]| {
        let mut out = SIGNATURE.to_vec();
        out.extend_from_slice(&(xml.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0u8; 4]);
        out
    };
    let Some(data_start) = attachments.iter().map(|a| a.1).min() else {
        let xml = render(0)?;
        let mut prefix = preamble(&xml);
        prefix.extend_from_slice(&xml);
        return Ok(Rewrite {
            prefix,
            data_start: None,
        });
    };
    // The data stays where it is while the header fits in front of it;
    // otherwise it moves down, and the positions in the header with it.
    let mut shift = 0u64;
    let xml = loop {
        let xml = render(shift)?;
        let need = (PREAMBLE + xml.len()) as u64;
        if need <= data_start + shift {
            break xml;
        }
        shift = (need + 256 - data_start).div_ceil(ALIGN) * ALIGN;
    };
    let mut prefix = preamble(&xml);
    prefix.extend_from_slice(&xml);
    prefix.resize((data_start + shift) as usize, 0);
    Ok(Rewrite {
        prefix,
        data_start: Some(data_start),
    })
}

/// Replace the FITS keywords of an XISF file with `header`, keeping the
/// image data and everything else in the file's header.
pub fn rewrite_header(path: &Path, header: &Header) -> Result<()> {
    let plan = plan_rewrite(path, header)?;
    if plan.data_start == Some(plan.prefix.len() as u64) {
        let mut f = std::fs::OpenOptions::new().write(true).open(path)?;
        f.write_all(&plan.prefix)?;
        return Ok(());
    }
    let tmp = fits::tmp_path(path);
    {
        let mut w = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
        w.write_all(&plan.prefix)?;
        if let Some(start) = plan.data_start {
            let mut src = std::fs::File::open(path)?;
            src.seek(SeekFrom::Start(start))?;
            std::io::copy(&mut src, &mut w)?;
        }
        w.flush()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// SHA-256 of the file as it would be after [`rewrite_header`] with
/// `header`, computed without writing anything.
pub fn sha256_with_header(path: &Path, header: &Header) -> Result<String> {
    use sha2::{Digest, Sha256};
    let plan = plan_rewrite(path, header)?;
    let mut hasher = Sha256::new();
    hasher.update(&plan.prefix);
    if let Some(start) = plan.data_start {
        let mut file = std::fs::File::open(path)?;
        file.seek(SeekFrom::Start(start))?;
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
    }
    Ok(format!("{:x}", hasher.finalize()))
}

pub struct XisfImage {
    pub header: Header,
    pub shape: ImageShape,
    pub data: Vec<f32>,
    pub sample_format: String,
}

pub fn read(path: &Path) -> Result<XisfImage> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    if bytes.len() < PREAMBLE || &bytes[..8] != SIGNATURE {
        bail!("not an XISF 1.0 file");
    }
    let hlen = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
    let xml = std::str::from_utf8(
        bytes
            .get(PREAMBLE..PREAMBLE + hlen)
            .ok_or_else(|| anyhow!("truncated XISF header"))?,
    )?;
    let xml = sanitise(xml.trim_end_matches('\0'));
    let doc = roxmltree::Document::parse(&xml).context("parsing XISF XML header")?;
    let image = image_node(&doc)?;

    let geometry: Vec<usize> = image
        .attribute("geometry")
        .ok_or_else(|| anyhow!("Image has no geometry"))?
        .split(':')
        .map(|v| v.parse().map_err(|_| anyhow!("bad geometry")))
        .collect::<Result<_>>()?;
    if geometry.len() < 3 {
        bail!("only 2-D XISF images are supported");
    }
    let shape = ImageShape {
        width: geometry[0],
        height: geometry[1],
        planes: geometry[2].max(1),
    };
    let sample_format = image
        .attribute("sampleFormat")
        .unwrap_or("UInt16")
        .to_string();
    let item_size = match sample_format.as_str() {
        "UInt8" => 1,
        "UInt16" => 2,
        "UInt32" | "Float32" => 4,
        "Float64" => 8,
        other => bail!("unsupported XISF sample format {other}"),
    };
    let big_endian = image.attribute("byteOrder") == Some("big");
    let planar = image
        .attribute("pixelStorage")
        .map(|s| s.eq_ignore_ascii_case("planar"))
        .unwrap_or(true);

    let raw = block_bytes(&bytes, image, image.attribute("location"))?;
    let raw = match image.attribute("compression") {
        Some(c) => decompress(&raw, c)?,
        None => raw,
    };
    if raw.len() < shape.len() * item_size {
        bail!("XISF image data is shorter than its geometry");
    }

    let mut data = decode(&raw[..shape.len() * item_size], &sample_format, big_endian);
    // Normalised float images (0..1) are common in XISF; keep values as-is.
    if !planar && shape.planes > 1 {
        let (w, h, c) = (shape.width, shape.height, shape.planes);
        let mut out = vec![0.0f32; data.len()];
        for p in 0..w * h {
            for ch in 0..c {
                out[ch * w * h + p] = data[p * c + ch];
            }
        }
        data = out;
    }

    let header = keywords(image);
    Ok(XisfImage {
        header,
        shape,
        data,
        sample_format,
    })
}

fn parse_value(s: &str) -> Value {
    let t = s.trim();
    if let Some(inner) = t.strip_prefix('\'') {
        let inner = inner.strip_suffix('\'').unwrap_or(inner);
        return Value::Str(inner.replace("''", "'").trim_end().to_string());
    }
    match t {
        "" => Value::Undefined,
        "T" => Value::Bool(true),
        "F" => Value::Bool(false),
        _ => t
            .parse::<i64>()
            .map(Value::Int)
            .or_else(|_| t.parse::<f64>().map(Value::Float))
            .unwrap_or_else(|_| Value::Str(t.to_string())),
    }
}

fn block_bytes(file: &[u8], node: roxmltree::Node, location: Option<&str>) -> Result<Vec<u8>> {
    let location = location.ok_or_else(|| anyhow!("Image has no location"))?;
    let parts: Vec<&str> = location.split(':').collect();
    match parts[0] {
        "attachment" => {
            let pos: usize = parts
                .get(1)
                .and_then(|v| v.parse().ok())
                .ok_or_else(|| anyhow!("bad location"))?;
            let len: usize = parts
                .get(2)
                .and_then(|v| v.parse().ok())
                .ok_or_else(|| anyhow!("bad location"))?;
            Ok(file
                .get(pos..pos + len)
                .ok_or_else(|| anyhow!("attachment beyond end of file"))?
                .to_vec())
        }
        "inline" | "embedded" => {
            let text: String = if parts[0] == "inline" {
                node.text().unwrap_or("").to_string()
            } else {
                node.children()
                    .find(|n| n.has_tag_name("Data"))
                    .and_then(|n| n.text())
                    .unwrap_or("")
                    .to_string()
            };
            let text: String = text.chars().filter(|c| !c.is_whitespace()).collect();
            if parts.get(1) == Some(&"hex") {
                (0..text.len())
                    .step_by(2)
                    .map(|i| u8::from_str_radix(&text[i..i + 2], 16).map_err(Into::into))
                    .collect()
            } else {
                Ok(base64::engine::general_purpose::STANDARD.decode(text)?)
            }
        }
        other => bail!("unsupported XISF data location '{other}'"),
    }
}

fn decompress(data: &[u8], spec: &str) -> Result<Vec<u8>> {
    // e.g. "zlib:123456" or "lz4+sh:123456:2"
    let parts: Vec<&str> = spec.split(':').collect();
    let (codec, shuffled) = match parts[0].strip_suffix("+sh") {
        Some(c) => (c, true),
        None => (parts[0], false),
    };
    let size: usize = parts
        .get(1)
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| anyhow!("bad compression spec"))?;
    let out = match codec {
        "zlib" => {
            let mut out = Vec::with_capacity(size);
            flate2::read::ZlibDecoder::new(data).read_to_end(&mut out)?;
            out
        }
        "lz4" | "lz4hc" => {
            lz4_flex::block::decompress(data, size).map_err(|e| anyhow!("lz4: {e}"))?
        }
        "zstd" => zstd::bulk::decompress(data, size)?,
        other => bail!("unsupported XISF compression '{other}'"),
    };
    if shuffled {
        let item: usize = parts.get(2).and_then(|v| v.parse().ok()).unwrap_or(1);
        return Ok(unshuffle(&out, item));
    }
    Ok(out)
}

fn unshuffle(data: &[u8], item: usize) -> Vec<u8> {
    if item <= 1 {
        return data.to_vec();
    }
    let n = data.len() / item;
    let mut out = vec![0u8; data.len()];
    for b in 0..item {
        for i in 0..n {
            out[i * item + b] = data[b * n + i];
        }
    }
    // Trailing bytes that don't fill an item are stored unshuffled.
    out[n * item..].copy_from_slice(&data[n * item..]);
    out
}

fn decode(raw: &[u8], fmt: &str, be: bool) -> Vec<f32> {
    macro_rules! conv {
        ($t:ty, $n:expr) => {
            raw.chunks_exact($n)
                .map(|c| {
                    let a: [u8; $n] = c.try_into().unwrap();
                    (if be {
                        <$t>::from_be_bytes(a)
                    } else {
                        <$t>::from_le_bytes(a)
                    }) as f32
                })
                .collect()
        };
    }
    match fmt {
        "UInt8" => raw.iter().map(|&b| b as f32).collect(),
        "UInt16" => conv!(u16, 2),
        "UInt32" => conv!(u32, 4),
        "Float32" => conv!(f32, 4),
        _ => conv!(f64, 8),
    }
}

/// Convert an XISF file to FITS next to `out` and return the header written.
pub fn convert_to_fits(xisf: &Path, out: &Path) -> Result<Header> {
    let img = read(xisf)?;
    let kind = if matches!(img.sample_format.as_str(), "UInt8" | "UInt16") {
        OutType::U16
    } else {
        OutType::F32
    };
    let mut header = img.header;
    header.add_history(&format!(
        "Converted from XISF ({}) by AstroFiler-rs",
        xisf.file_name().unwrap_or_default().to_string_lossy()
    ));
    fits::write_image(out, &header, img.shape, &img.data, kind)?;
    fits::read_primary_header(out)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A small XISF image with the given keywords (values as XISF spells
    /// them, e.g. `'M 31'` or `30.`).
    pub fn write_xisf(path: &Path, keywords: &[(&str, &str)], seed: u8) {
        let payload: Vec<u8> = (0..24u8).map(|v| v.wrapping_add(seed)).collect();
        let kw: String = keywords
            .iter()
            .map(|(k, v)| format!(r#"<FITSKeyword name="{k}" value="{v}" comment=""/>"#))
            .collect();
        let xml = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?><xisf version="1.0" xmlns="http://www.pixinsight.com/xisf"><Image geometry="4:3:1" sampleFormat="UInt16" location="attachment:4096:24">{kw}</Image></xisf>"#
        );
        let mut file = SIGNATURE.to_vec();
        file.extend_from_slice(&(xml.len() as u32).to_le_bytes());
        file.extend_from_slice(&[0u8; 4]);
        file.extend_from_slice(xml.as_bytes());
        file.resize(4096, 0);
        file.extend_from_slice(&payload);
        std::fs::write(path, file).unwrap();
    }

    fn make_xisf(dir: &Path, compress: bool) -> std::path::PathBuf {
        let pixels: Vec<u16> = (0..12u16).map(|v| v * 100).collect();
        let mut raw = Vec::new();
        for p in &pixels {
            raw.extend_from_slice(&p.to_le_bytes());
        }
        let (payload, comp_attr) = if compress {
            let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
            std::io::Write::write_all(&mut e, &raw).unwrap();
            (
                e.finish().unwrap(),
                format!(" compression=\"zlib:{}\"", raw.len()),
            )
        } else {
            (raw.clone(), String::new())
        };
        let data_pos = 4096;
        let xml = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?><xisf version="1.0" xmlns="http://www.pixinsight.com/xisf"><Image geometry="4:3:1" sampleFormat="UInt16" location="attachment:{}:{}"{}><FITSKeyword name="OBJECT" value="'M 42'" comment="target"/><FITSKeyword name="EXPTIME" value="30." comment=""/></Image></xisf>"#,
            data_pos,
            payload.len(),
            comp_attr
        );
        let mut file = b"XISF0100".to_vec();
        file.extend_from_slice(&(xml.len() as u32).to_le_bytes());
        file.extend_from_slice(&[0u8; 4]);
        file.extend_from_slice(xml.as_bytes());
        file.resize(data_pos, 0);
        file.extend_from_slice(&payload);
        let p = dir.join(if compress { "c.xisf" } else { "u.xisf" });
        std::fs::write(&p, file).unwrap();
        p
    }

    #[test]
    fn reads_and_converts() {
        let dir = tempfile::tempdir().unwrap();
        for compress in [false, true] {
            let p = make_xisf(dir.path(), compress);
            let img = read(&p).unwrap();
            assert_eq!(
                img.shape,
                ImageShape {
                    width: 4,
                    height: 3,
                    planes: 1
                }
            );
            assert_eq!(img.data[11], 1100.0);
            assert_eq!(img.header.get_str("OBJECT").as_deref(), Some("M 42"));
            let out = dir.path().join("out.fits");
            let h = convert_to_fits(&p, &out).unwrap();
            assert_eq!(h.get_f64("EXPTIME"), Some(30.0));
            assert_eq!(fits::read_image(&out).unwrap().data[5], 500.0);
        }
    }

    /// Like a file from PixInsight: properties, padding before the data.
    fn make_master(dir: &Path, data_pos: usize) -> std::path::PathBuf {
        let payload: Vec<u8> = (0..24u8).collect();
        let xml = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?><xisf version="1.0" xmlns="http://www.pixinsight.com/xisf"><Image geometry="4:3:1" sampleFormat="UInt16" location="attachment:{data_pos}:24"><Property id="Observation:Object:Name" type="String">NGC281 - Pacman</Property><FITSKeyword name="OBJECT" value="'NGC281 - Pacman'" comment="target"/><FITSKeyword name="EXPTIME" value="30." comment=""/><FITSKeyword name="HISTORY" value="" comment="Integration &amp; more"/><ICCProfile location="attachment:{}:8"/></Image></xisf>"#,
            data_pos + 24
        );
        let mut file = SIGNATURE.to_vec();
        file.extend_from_slice(&(xml.len() as u32).to_le_bytes());
        file.extend_from_slice(&[0u8; 4]);
        file.extend_from_slice(xml.as_bytes());
        assert!(file.len() <= data_pos);
        file.resize(data_pos, 0);
        file.extend_from_slice(&payload);
        file.extend_from_slice(b"ICCICCIC");
        let p = dir.join("master.xisf");
        std::fs::write(&p, file).unwrap();
        p
    }

    #[test]
    fn rewrites_keywords_in_place_and_by_moving_the_data() {
        let dir = tempfile::tempdir().unwrap();
        // Roomy: the edit fits in front of the data. Tight: the data moves.
        for (data_pos, moves) in [(4096usize, false), (600, true)] {
            let p = make_master(dir.path(), data_pos);
            let before = std::fs::read(&p).unwrap();
            let pixels = read(&p).unwrap().data;
            let mut h = read_header(&p).unwrap();
            assert_eq!(h.get_str("OBJECT").as_deref(), Some("NGC281 - Pacman"));
            h.set("OBJECT", Value::Str("NGC 281 \"Pacman\" & co".into()));
            h.set("IMAGETYP", Value::Str("Master Light".into()));
            h.add_history("Edited by a test");

            let expected = sha256_with_header(&p, &h).unwrap();
            rewrite_header(&p, &h).unwrap();
            assert_eq!(crate::util::sha256_file(&p).unwrap(), expected);

            let after = std::fs::read(&p).unwrap();
            assert_eq!(after.len() > before.len(), moves);
            let back = read_header(&p).unwrap();
            assert_eq!(
                back.get_str("OBJECT").as_deref(),
                Some("NGC 281 \"Pacman\" & co")
            );
            assert_eq!(back.get_str("IMAGETYP").as_deref(), Some("Master Light"));
            assert_eq!(back.get_f64("EXPTIME"), Some(30.0));
            assert_eq!(back.cards.len(), h.cards.len());
            // The image data is the same, wherever it is now.
            assert_eq!(read(&p).unwrap().data, pixels);
            assert!(after.ends_with(b"ICCICCIC"));
            let xml = String::from_utf8_lossy(&after);
            // Untouched keywords keep their spelling; the property follows.
            assert!(xml.contains(r#"value="30.""#));
            assert!(xml.contains(r#"type="String">NGC 281 &quot;Pacman&quot; &amp; co</Property>"#));
            let shift = after.len() - before.len();
            assert!(xml.contains(&format!("attachment:{}:24", data_pos + shift)));
            assert!(xml.contains(&format!("attachment:{}:8", data_pos + 24 + shift)));

            // Writing the same header again changes nothing.
            rewrite_header(&p, &back).unwrap();
            assert_eq!(std::fs::read(&p).unwrap(), after);
            std::fs::remove_file(&p).unwrap();
        }
    }

    #[test]
    fn unshuffles() {
        // items [0x0102, 0x0304] shuffled -> [01,03,02,04]
        assert_eq!(unshuffle(&[1, 3, 2, 4], 2), vec![1, 2, 3, 4]);
    }
}
