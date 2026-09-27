//! Auto-stretched preview of a FITS image for the GUI.

use crate::fits;
use anyhow::Result;
use eframe::egui::ColorImage;
use rayon::prelude::*;
use std::path::Path;

pub struct Preview {
    pub image: ColorImage,
    pub info: String,
}

fn percentiles(data: &[f32], lo: f64, hi: f64) -> (f32, f32) {
    let step = (data.len() / 500_000).max(1);
    let mut s: Vec<f32> = data
        .iter()
        .step_by(step)
        .copied()
        .filter(|v| v.is_finite())
        .collect();
    if s.is_empty() {
        return (0.0, 1.0);
    }
    s.sort_unstable_by(|a, b| a.total_cmp(b));
    let at = |q: f64| s[((s.len() - 1) as f64 * q) as usize];
    let (a, b) = (at(lo), at(hi));
    if b > a {
        (a, b)
    } else {
        (a, a + 1.0)
    }
}

/// Downsample (block average) to at most `max` pixels on the long side and
/// apply an asinh stretch between the 0.5 and 99.8 percentiles.
pub fn render(path: &Path, max: usize) -> Result<Preview> {
    let img = fits::read_image(path)?;
    let s = img.shape;
    let (w, h) = (s.width, s.height);
    let step = w.max(h).div_ceil(max).max(1);
    let (ow, oh) = ((w / step).max(1), (h / step).max(1));
    let channels = if s.planes >= 3 { 3 } else { 1 };
    let plane = w * h;
    let ranges: Vec<(f32, f32)> = (0..channels)
        .map(|c| percentiles(&img.data[c * plane..(c + 1) * plane], 0.005, 0.998))
        .collect();
    let k = 12.0f32;
    let norm = k.asinh();
    let mut rgba = vec![0u8; ow * oh * 4];
    rgba.par_chunks_mut(ow * 4)
        .enumerate()
        .for_each(|(oy, row)| {
            // FITS rows run bottom-up.
            let sy = (oh - 1 - oy) * step;
            for ox in 0..ow {
                let sx = ox * step;
                let mut out = [0u8; 3];
                for c in 0..channels {
                    let base = &img.data[c * plane..];
                    let mut sum = 0.0f32;
                    let mut n = 0.0f32;
                    for y in sy..(sy + step).min(h) {
                        for x in sx..(sx + step).min(w) {
                            let v = base[y * w + x];
                            if v.is_finite() {
                                sum += v;
                                n += 1.0;
                            }
                        }
                    }
                    let (lo, hi) = ranges[c];
                    let t = if n > 0.0 {
                        ((sum / n - lo) / (hi - lo)).clamp(0.0, 1.0)
                    } else {
                        0.0
                    };
                    out[c] = ((t * k).asinh() / norm * 255.0) as u8;
                }
                if channels == 1 {
                    out = [out[0]; 3];
                }
                let p = &mut row[ox * 4..ox * 4 + 4];
                p.copy_from_slice(&[out[0], out[1], out[2], 255]);
            }
        });
    let h_ = &img.header;
    let info = format!(
        "{}×{}{}  {}  {}  {}s  {}",
        w,
        h,
        if s.planes > 1 {
            format!("×{}", s.planes)
        } else {
            String::new()
        },
        h_.get_str("OBJECT").unwrap_or_default(),
        h_.get_str("FILTER").unwrap_or_default(),
        h_.get_str("EXPTIME")
            .or_else(|| h_.get_str("EXPOSURE"))
            .unwrap_or_default(),
        h_.get_str("DATE-OBS").unwrap_or_default(),
    );
    Ok(Preview {
        image: ColorImage::from_rgba_unmultiplied([ow, oh], &rgba),
        info,
    })
}
