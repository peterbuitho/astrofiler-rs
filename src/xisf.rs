//! XISF (PixInsight) reader. XISF files are converted to FITS on import, as the
//! original does, so everything downstream only deals with FITS.
//!
//! Supports monolithic XISF 1.0 files with attached or inline image data,
//! UInt8/16/32 and Float32/64 samples, planar or interleaved ("normal") pixel
//! storage, zlib / lz4 / lz4hc / zstd compression with optional byte shuffling,
//! and FITSKeyword metadata.

use crate::fits::{self, Card, Header, ImageShape, OutType, Value};
use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use std::io::Read;
use std::path::Path;

pub struct XisfImage {
    pub header: Header,
    pub shape: ImageShape,
    pub data: Vec<f32>,
    pub sample_format: String,
}

pub fn read(path: &Path) -> Result<XisfImage> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    if bytes.len() < 16 || &bytes[..8] != b"XISF0100" {
        bail!("not an XISF 1.0 file");
    }
    let hlen = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
    let xml = std::str::from_utf8(
        bytes
            .get(16..16 + hlen)
            .ok_or_else(|| anyhow!("truncated XISF header"))?,
    )?;
    let xml = xml.trim_end_matches('\0');
    let doc = roxmltree::Document::parse(xml).context("parsing XISF XML header")?;
    let image = doc
        .descendants()
        .find(|n| n.has_tag_name("Image"))
        .ok_or_else(|| anyhow!("XISF file has no Image element"))?;

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

    let mut header = Header::default();
    for kw in image.children().filter(|n| n.has_tag_name("FITSKeyword")) {
        let key = kw.attribute("name").unwrap_or("").trim().to_uppercase();
        if key.is_empty() {
            continue;
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
        header.cards.push(Card {
            key,
            value,
            comment,
        });
    }
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
mod tests {
    use super::*;

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

    #[test]
    fn unshuffles() {
        // items [0x0102, 0x0304] shuffled -> [01,03,02,04]
        assert_eq!(unshuffle(&[1, 3, 2, 4], 2), vec![1, 2, 3, 4]);
    }
}
