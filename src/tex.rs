//! DDS (as RSX exports it) -> RGBA8 -> a small PNG or JPEG for the glb.
//!
//! RSX writes every mip of a texture as its own `0x<GUID>_level<n>.dds`, so a
//! texture is never downscaled from 4K here: the largest mip that fits the cap
//! is picked and decoded, and only a non-square or odd mip gets resized.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use image::{codecs::jpeg::JpegEncoder, ImageEncoder, RgbaImage};

/// One decoded texture, top-left origin, RGBA8.
pub struct Rgba {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

/// What a texture is used for: decides how its channels are rebuilt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Role {
    /// Base colour (sRGB). Alpha kept when it cuts texels away.
    Color,
    /// Tangent-space normal (BC5, or BC1/BC7 with x/y in red/green).
    Normal,
    /// Base colour of a blended (decal / glass) material: alpha always kept,
    /// multiplied by its opacity mask when it has one.
    Blend,
}

/// Every mip RSX exported for a texture guid, keyed by width, from `dir`
/// (`<dir>/0x<GUID>_level<n>.dds`).
pub fn mips_of(dir: &Path, guid: u64) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for n in 0..16 {
        let p = dir.join(format!("0x{guid:X}_level{n}.dds"));
        if p.is_file() {
            out.push(p);
        }
    }
    out
}

/// The largest mip whose longer side is at most `cap`; the smallest one when
/// none fits, and the largest one there is when every mip is smaller.
pub fn pick_mip(mips: &[PathBuf], cap: u32) -> Result<(PathBuf, rpak::DdsInfo)> {
    let mut best: Option<(PathBuf, rpak::DdsInfo)> = None;
    let mut smallest: Option<(PathBuf, rpak::DdsInfo)> = None;
    for p in mips {
        let head = read_head(p)?;
        let info = rpak::parse_dds_header(&head).map_err(|e| anyhow!("{}: {e}", p.display()))?;
        let side = info.width.max(info.height);
        if smallest.as_ref().is_none_or(|(_, s)| s.width.max(s.height) > side) {
            smallest = Some((p.clone(), info.clone()));
        }
        if side <= cap && best.as_ref().is_none_or(|(_, b)| b.width.max(b.height) < side) {
            best = Some((p.clone(), info));
        }
    }
    best.or(smallest).ok_or_else(|| anyhow!("no mips"))
}

fn read_head(p: &Path) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut f = std::fs::File::open(p).with_context(|| format!("opening {}", p.display()))?;
    let mut b = vec![0u8; 148];
    let n = f.read(&mut b)?;
    b.truncate(n);
    Ok(b)
}

/// Decodes the top surface of a DDS file into RGBA8.
pub fn decode_dds(bytes: &[u8], role: Role) -> Result<Rgba> {
    let info = rpak::parse_dds_header(bytes).map_err(|e| anyhow!("dds: {e}"))?;
    let (w, h) = (info.width as usize, info.height as usize);
    let data = &bytes[info.data_offset as usize..];
    let dxgi = match info.format {
        rpak::DdsFormat::BlockCompressed(d) | rpak::DdsFormat::Uncompressed(d) => d,
        rpak::DdsFormat::LegacyFourcc(cc) => match &cc {
            b"DXT1" => 71,
            b"DXT3" => 74,
            b"DXT5" => 77,
            b"ATI1" | b"BC4U" => 80,
            b"ATI2" | b"BC5U" => 83,
            _ => bail!("unsupported fourcc {:?}", String::from_utf8_lossy(&cc)),
        },
    };
    let mut px = vec![0u8; w * h * 4];
    let mut block = |size: usize, f: &dyn Fn(&[u8], &mut [u8; 64])| -> Result<()> {
        let (bw, bh) = (w.div_ceil(4), h.div_ceil(4));
        if data.len() < bw * bh * size {
            bail!("dds payload too short: {} < {}", data.len(), bw * bh * size);
        }
        for by in 0..bh {
            for bx in 0..bw {
                let src = &data[(by * bw + bx) * size..][..size];
                let mut out = [0u8; 64];
                f(src, &mut out);
                for y in 0..4 {
                    let py = by * 4 + y;
                    if py >= h {
                        break;
                    }
                    for x in 0..4 {
                        let pxx = bx * 4 + x;
                        if pxx >= w {
                            break;
                        }
                        let o = (py * w + pxx) * 4;
                        let i = (y * 4 + x) * 4;
                        // SAFETY of indexing: o < w*h*4 by the bounds above.
                        unsafe_copy(&mut px, o, &out[i..i + 4]);
                    }
                }
            }
        }
        Ok(())
    };
    match dxgi {
        // BC1
        70..=72 => block(8, &|s, o| bcdec_rs::bc1(s, o, 16))?,
        // BC2
        73..=75 => block(16, &|s, o| bcdec_rs::bc2(s, o, 16))?,
        // BC3
        76..=78 => block(16, &|s, o| bcdec_rs::bc3(s, o, 16))?,
        // BC4: grey
        79..=81 => block(8, &|s, o| {
            let mut r = [0u8; 16];
            bcdec_rs::bc4(s, &mut r, 4, dxgi == 81);
            for i in 0..16 {
                o[i * 4..i * 4 + 4].copy_from_slice(&[r[i], r[i], r[i], 255]);
            }
        })?,
        // BC5: RG, B rebuilt for normals below.
        82..=84 => block(16, &|s, o| {
            let mut rg = [0u8; 32];
            bcdec_rs::bc5(s, &mut rg, 8, dxgi == 84);
            for i in 0..16 {
                o[i * 4..i * 4 + 4].copy_from_slice(&[rg[i * 2], rg[i * 2 + 1], 0, 255]);
            }
        })?,
        // BC6H: HDR, tone-mapped with a plain clamp (only skies and emissives use it).
        94..=96 => block(16, &|s, o| {
            let mut f = [0f32; 48];
            bcdec_rs::bc6h_float(s, &mut f, 12, dxgi == 96);
            for i in 0..16 {
                for c in 0..3 {
                    let v = f[i * 3 + c].max(0.0);
                    let v = v / (1.0 + v);
                    o[i * 4 + c] = (v.powf(1.0 / 2.2) * 255.0).round().clamp(0.0, 255.0) as u8;
                }
                o[i * 4 + 3] = 255;
            }
        })?,
        // BC7
        97..=99 => block(16, &|s, o| bcdec_rs::bc7(s, o, 16))?,
        // RGBA8 / BGRA8
        27..=32 | 87 | 88 | 90 | 91 => {
            if data.len() < w * h * 4 {
                bail!("dds payload too short");
            }
            px.copy_from_slice(&data[..w * h * 4]);
            if matches!(dxgi, 87 | 88 | 90 | 91) {
                for p in px.chunks_exact_mut(4) {
                    p.swap(0, 2);
                }
            }
        }
        other => bail!("unsupported DXGI format {other}"),
    }
    if role == Role::Normal {
        rebuild_normal(&mut px);
    }
    Ok(Rgba { width: w as u32, height: h as u32, pixels: px })
}

#[inline]
fn unsafe_copy(dst: &mut [u8], at: usize, src: &[u8]) {
    dst[at..at + src.len()].copy_from_slice(src);
}

/// Respawn normal maps are DirectX style (green = down) and usually two
/// channels; glTF wants OpenGL style (green = up) with all three.
fn rebuild_normal(px: &mut [u8]) {
    for p in px.chunks_exact_mut(4) {
        // Z is always rebuilt from X/Y: BC5 has no blue, and the BC1 normal
        // maps some materials use do not keep a trustworthy one.
        let x = p[0] as f32 / 127.5 - 1.0;
        let y = p[1] as f32 / 127.5 - 1.0;
        let z = (1.0 - x * x - y * y).max(0.0).sqrt();
        p[1] = 255 - p[1];
        p[2] = ((z * 0.5 + 0.5) * 255.0).round().clamp(0.0, 255.0) as u8;
        p[3] = 255;
    }
}

impl Rgba {
    /// Shrinks so the longer side is at most `cap` (Lanczos is overkill here;
    /// a triangle filter from an already close mip is plenty).
    pub fn fit(self, cap: u32) -> Rgba {
        if self.width.max(self.height) <= cap {
            return self;
        }
        let scale = cap as f32 / self.width.max(self.height) as f32;
        let nw = ((self.width as f32 * scale).round() as u32).max(1);
        let nh = ((self.height as f32 * scale).round() as u32).max(1);
        let img = RgbaImage::from_raw(self.width, self.height, self.pixels).expect("sized buffer");
        let out = image::imageops::resize(&img, nw, nh, image::imageops::FilterType::Triangle);
        Rgba { width: nw, height: nh, pixels: out.into_raw() }
    }

    /// Resamples to exactly `w` x `h` (an opacity mask onto its colour map).
    pub fn resized(self, w: u32, h: u32) -> Rgba {
        if self.width == w && self.height == h {
            return self;
        }
        let img = RgbaImage::from_raw(self.width, self.height, self.pixels).expect("sized buffer");
        let out = image::imageops::resize(&img, w, h, image::imageops::FilterType::Triangle);
        Rgba { width: w, height: h, pixels: out.into_raw() }
    }

    /// The share of texels whose alpha is under half, which is what an
    /// alpha-tested material actually cuts away.
    pub fn cutout_share(&self) -> f32 {
        let n = (self.width * self.height) as usize;
        if n == 0 {
            return 0.0;
        }
        let cut = self.pixels.chunks_exact(4).filter(|p| p[3] < 128).count();
        cut as f32 / n as f32
    }

    /// PNG when alpha matters, JPEG otherwise. Returns (bytes, mime).
    pub fn encode(&self, keep_alpha: bool, quality: u8) -> Result<(Vec<u8>, &'static str)> {
        let mut out = Vec::new();
        if keep_alpha {
            image::codecs::png::PngEncoder::new_with_quality(
                &mut out,
                image::codecs::png::CompressionType::Fast,
                image::codecs::png::FilterType::Adaptive,
            )
            .write_image(&self.pixels, self.width, self.height, image::ExtendedColorType::Rgba8)?;
            return Ok((out, "image/png"));
        }
        let rgb: Vec<u8> = self.pixels.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]).collect();
        JpegEncoder::new_with_quality(&mut out, quality).write_image(&rgb, self.width, self.height, image::ExtendedColorType::Rgb8)?;
        Ok((out, "image/jpeg"))
    }
}
