//! Materials -> their base colour / normal / opacity textures -> small encoded
//! images.
//!
//! Material jsons come from RSX (`--texturenames guid`, so every slot is a
//! texture guid), exported with every pak loaded so each slot's role
//! (`_col`, `_nml`, `_msk`, ...) resolves through the shader set. Textures come
//! out as one DDS per mip; the right mip is decoded, shrunk if needed, and
//! encoded once into `<work>/imgcache`, so a later run (or a `--model` call)
//! reuses it without touching the paks.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::Result;
use rayon::prelude::*;
use serde_json::Value;

use crate::assets::Paks;
use crate::tex::{self, Role};

/// What a material resolved to.
#[derive(Clone, Debug, Default)]
pub struct MatInfo {
    pub path: String,
    pub shader: String,
    pub albedo: Option<u64>,
    pub normal: Option<u64>,
    /// A separate opacity mask (`_msk`, `opacityMultiplyTexture`).
    pub opacity: Option<u64>,
    /// Self-illumination / glow (`_ilm`, `emissiveTexture`): heirloom and weapon glows.
    pub emissive: Option<u64>,
    pub translucent: bool,
    pub nodraw: bool,
}

/// One image to build: texture guid, role, size cap, and for a blended colour
/// texture the opacity mask guid whose red channel becomes alpha (0 = none).
pub type ImgKey = (u64, Role, u32, u64);

/// One encoded image, ready for the glb.
#[derive(Clone)]
pub struct Encoded {
    pub bytes: Vec<u8>,
    pub mime: &'static str,
    /// True when the alpha channel actually cuts texels away.
    pub cutout: bool,
}

fn hex(v: &Value) -> Option<u64> {
    let s = v.as_str()?;
    u64::from_str_radix(s.trim_start_matches("0x").trim_start_matches("0X"), 16).ok().filter(|g| *g != 0)
}

fn parse_material(text: &str) -> Option<MatInfo> {
    let v: Value = serde_json::from_str(text).ok()?;
    let mut info = MatInfo {
        path: v.get("name").and_then(Value::as_str).unwrap_or_default().to_string(),
        shader: v.get("shaderType").and_then(Value::as_str).unwrap_or_default().to_string(),
        ..Default::default()
    };
    let flags = |k: &str| v.get(k).and_then(hex).unwrap_or(0);
    // Same reading r5ed's rpak::material uses: no depth write = drawn over the
    // world (decals, glass); uber bit 3 = nodraw.
    info.translucent = v.get("depthStencilFlags").is_some() && flags("depthStencilFlags") & 0x10 == 0;
    info.nodraw = flags("uberBufferFlags") & 0x8 != 0;
    let slots = v.get("$textures").and_then(Value::as_object)?;
    let types = v.get("$textureTypes").and_then(Value::as_object);
    let mut ordered: Vec<(u32, u64, String)> = slots
        .iter()
        .filter_map(|(k, val)| {
            let slot: u32 = k.parse().ok()?;
            let g = hex(val)?;
            let t = types.and_then(|t| t.get(k)).and_then(Value::as_str).unwrap_or("").to_ascii_lowercase();
            Some((slot, g, t))
        })
        .collect();
    ordered.sort_by_key(|o| o.0);
    let is_col = |t: &str| t == "_col" || t == "albedotexture" || t == "diffusetexture";
    let is_nml = |t: &str| t == "_nml" || t == "normaltexture";
    let is_msk = |t: &str| t == "_msk" || t == "_opa" || t == "opacitymultiplytexture" || t == "opacitytexture";
    info.albedo = ordered.iter().find(|o| is_col(&o.2)).map(|o| o.1);
    info.normal = ordered.iter().find(|o| is_nml(&o.2)).map(|o| o.1);
    info.opacity = ordered.iter().find(|o| is_msk(&o.2)).map(|o| o.1);
    let is_emi = |t: &str| matches!(t, "_ilm" | "_emi" | "_emis" | "_emissive" | "_glw" | "_glow" | "emissivetexture" | "selfillumtexture" | "emissive");
    info.emissive = ordered.iter().find(|o| is_emi(&o.2)).map(|o| o.1);
    let known = ordered.iter().any(|o| is_col(&o.2) || is_nml(&o.2));
    if !known {
        // Roles unresolved ("unavailable"): Respawn's slot order is colour
        // first, then normal (after a second colour layer on blend materials).
        // A wrong guess is caught later: a normal must decode as BC5.
        let blend = info.path.contains("_bm") || info.path.ends_with("_blend");
        info.albedo = ordered.first().map(|o| o.1);
        info.normal = ordered.get(if blend { 2 } else { 1 }).map(|o| o.1);
    }
    Some(info)
}

/// Resolves material asset guids to their info, exporting the jsons the
/// work dir does not have yet.
pub fn resolve(paks: &Paks, guids: &BTreeSet<u64>) -> Result<BTreeMap<u64, MatInfo>> {
    let dir = paks.cfg.work.join("materials");
    let json_of = |g: u64| -> Option<PathBuf> {
        let path = paks.path_of(g)?;
        let stem = path.strip_suffix(".rpak").unwrap_or(path);
        Some(dir.join(format!("{stem}.json")))
    };
    let todo: Vec<u64> = guids.iter().copied().filter(|g| json_of(*g).is_some_and(|p| !p.is_file())).collect();
    paks.export_together("matl", &["--texturenames", "guid"], &todo, &dir)?;
    let mut out = BTreeMap::new();
    for &g in guids {
        let Some(p) = json_of(g) else { continue };
        if let Some(info) = std::fs::read_to_string(&p).ok().and_then(|t| parse_material(&t)) {
            out.insert(g, info);
        }
    }
    Ok(out)
}

fn cache_path(work: &Path, k: &ImgKey) -> PathBuf {
    let r = match k.1 {
        Role::Normal => "n",
        Role::Color => "c",
        Role::Blend => "b",
    };
    work.join("imgcache").join(format!("{:016x}_{r}{}_{:016x}.bin", k.0, k.2, k.3))
}

/// Cache file layout: one byte mime (0 png, 1 jpeg), one byte cutout flag,
/// then the image.
fn read_cached(p: &Path) -> Option<Encoded> {
    let b = std::fs::read(p).ok()?;
    if b.len() < 3 {
        return None;
    }
    Some(Encoded { mime: if b[0] == 0 { "image/png" } else { "image/jpeg" }, cutout: b[1] != 0, bytes: b[2..].to_vec() })
}

fn build_one(dir: &Path, work: &Path, k: &ImgKey) -> Result<()> {
    let &(g, role, cap, mask) = k;
    let mips = tex::mips_of(dir, g);
    if mips.is_empty() {
        anyhow::bail!("RSX exported no mips");
    }
    let (path, _) = tex::pick_mip(&mips, cap)?;
    let mut img = tex::decode_dds(&std::fs::read(&path)?, role)?.fit(cap);
    if (role == Role::Blend || role == Role::Color) && mask != 0 {
        let mm = tex::mips_of(dir, mask);
        if !mm.is_empty() {
            let (mp, _) = tex::pick_mip(&mm, cap)?;
            let m = tex::decode_dds(&std::fs::read(&mp)?, Role::Color)?.resized(img.width, img.height);
            for (p, q) in img.pixels.chunks_exact_mut(4).zip(m.pixels.chunks_exact(4)) {
                p[3] = ((p[3] as u16 * q[0] as u16) / 255) as u8;
            }
        }
    }
    let (cutout, keep_alpha) = match role {
        Role::Color => {
            let c = img.cutout_share() > 0.01;
            (c, c)
        }
        Role::Blend => (true, true),
        Role::Normal => (false, false),
    };
    let (bytes, mime) = img.encode(keep_alpha, 85)?;
    let mut out = vec![if mime == "image/png" { 0u8 } else { 1u8 }, cutout as u8];
    out.extend_from_slice(&bytes);
    std::fs::write(cache_path(work, k), out)?;
    Ok(())
}

/// Encodes every requested image, exporting the DDS mips of the textures not
/// cached yet. Returns what it could build.
/// `pre` is a directory RSX already exported DDS mips into (a dependency
/// export); what it covers is converted from there without another run.
pub fn build_images(paks: &Paks, wanted: &BTreeSet<ImgKey>, prefer: &[&str], pre: Option<&Path>) -> Result<BTreeMap<ImgKey, Encoded>> {
    let work = &paks.cfg.work;
    std::fs::create_dir_all(work.join("imgcache"))?;
    if let Some(pre) = pre {
        let ready: Vec<&ImgKey> = wanted
            .iter()
            .filter(|k| !cache_path(work, k).is_file() && !tex::mips_of(pre, k.0).is_empty() && (k.3 == 0 || !tex::mips_of(pre, k.3).is_empty()))
            .collect();
        let errors: Vec<String> =
            ready.par_iter().filter_map(|k| build_one(pre, work, k).err().map(|e| format!("0x{:016x} ({:?}): {e}", k.0, k.1))).collect();
        for e in errors.iter().take(8) {
            eprintln!("  texture {e}");
        }
    }
    let todo: Vec<&ImgKey> = wanted.iter().filter(|k| !cache_path(work, k).is_file()).collect();
    if !todo.is_empty() {
        let mut need: BTreeSet<u64> = BTreeSet::new();
        for k in &todo {
            need.insert(k.0);
            if k.3 != 0 {
                need.insert(k.3);
            }
        }
        let dir = work.join(format!("textures-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let list: Vec<u64> = need.iter().copied().collect();
        // Batches keep the all-mips DDS export small on disk: each batch is
        // converted and deleted before the next.
        for chunk in list.chunks(2000) {
            let missing = paks.export("txtr", &["--exportsetting", "txtr=3"], chunk, &dir, prefer)?;
            if !missing.is_empty() {
                eprintln!("  {} textures are in no loaded pak", missing.len());
            }
            let jobs: Vec<&&ImgKey> = todo.iter().filter(|k| chunk.contains(&k.0)).collect();
            let errors: Vec<String> = jobs
                .par_iter()
                .filter_map(|k| build_one(&dir.join("texture"), work, k).err().map(|e| format!("0x{:016x} ({:?}): {e}", k.0, k.1)))
                .collect();
            for e in errors.iter().take(8) {
                eprintln!("  texture {e}");
            }
            if errors.len() > 8 {
                eprintln!("  ... {} more texture errors", errors.len() - 8);
            }
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
    let mut out = BTreeMap::new();
    for k in wanted {
        if let Some(e) = read_cached(&cache_path(work, k)) {
            out.insert(*k, e);
        }
    }
    Ok(out)
}

/// Material names that never draw as world surfaces: tools, sky, clips,
/// triggers, and particle-ish cards (god rays, AO blobs, energy).
/// r5rust: sky-shaded surfaces (the Firing Range's projected 'fake sky' panels). Kept, marked `sky` in extras,
/// and drawn by the client with the sky cubemap.
pub fn is_sky_material(name: &str) -> bool {
    name.to_ascii_lowercase().replace('\\', "/").starts_with("skies/")
}

pub fn is_hidden_material(name: &str) -> bool {
    let n = name.to_ascii_lowercase().replace('\\', "/");
    n.starts_with("tools/")
        || n.starts_with("world/atmosphere/")
        || n.starts_with("models/fx/")
        || n.contains("godray")
        || n.contains("nodraw")
        || n.contains("toolsskybox")
        || n.contains("invisible")
        || n.contains("clip")
        || n.contains("trigger")
        || n == "_default"
}
