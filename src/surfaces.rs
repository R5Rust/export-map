//! Surface properties: what a collision triangle is made of, for impacts (and later footsteps).
//!
//! Each collision triangle carries a surfaceproperties id (`dsurfaceproperty_t.surfacePropId` on the world's leaves).
//! Retail prop models carry a single all-default record in their collision, so a prop's triangles take the model's
//! `$surfaceprop` instead (studiohdr v19.2 `surfacepropindex`, a u16 at 0xA6), looked up by name here.
//!
//! The id -> name / gamematerial table is `surfaceproperties.rson` (the S21 SDK script: Flowstate's
//! `platform\scripts\surfaceproperties.rson`, read as a script reference, not an asset; override with
//! R5_SURFACEPROPS). `gamematerial` is inherited through `base`. Written next to the map as `surfaces.json`
//! ({id: [name, gamematerial]}) for the client's impact tables.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Stock S21 `platform\scripts\surfaceproperties.rson`, copied 2026-11-20 (the main S21 install was removed).
pub const DEFAULT_RSON: &str = r"D:\R5Rust\s21\surfaceproperties.rson";

#[derive(Default)]
pub struct Table {
    /// lower-case name -> id
    pub by_name: BTreeMap<String, u8>,
    /// id -> (name, gamematerial)
    pub by_id: BTreeMap<u8, (String, String)>,
}

pub fn rson_path() -> PathBuf {
    std::env::var_os("R5_SURFACEPROPS").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(DEFAULT_RSON))
}

impl Table {
    pub fn load(path: &Path) -> Option<Table> {
        let text = std::fs::read_to_string(path).ok()?;
        // name: { key: value ... }  (one level; values may be quoted)
        let mut raw: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
        let mut cur: Option<String> = None;
        for line in text.lines() {
            let l = line.split("//").next().unwrap_or("").trim();
            if l.is_empty() {
                continue;
            }
            if l == "{" {
                continue;
            }
            if l.starts_with('}') {
                cur = None;
                continue;
            }
            let Some((k, v)) = l.split_once(':') else { continue };
            let (k, v) = (k.trim().to_ascii_lowercase(), v.trim().trim_matches('"').to_string());
            match &cur {
                None => {
                    cur = Some(k.clone());
                    raw.entry(k).or_default();
                }
                Some(name) => {
                    raw.entry(name.clone()).or_default().insert(k, v);
                }
            }
        }
        fn gm(raw: &BTreeMap<String, BTreeMap<String, String>>, name: &str, depth: u32) -> Option<String> {
            let p = raw.get(name)?;
            if let Some(g) = p.get("gamematerial") {
                return Some(g.clone());
            }
            if depth > 10 {
                return None;
            }
            gm(raw, &p.get("base")?.to_ascii_lowercase(), depth + 1)
        }
        let mut t = Table::default();
        for (name, kv) in &raw {
            let Some(id) = kv.get("id").and_then(|v| v.parse::<u32>().ok()).filter(|&i| i < 255) else { continue };
            let g = gm(&raw, name, 0).or_else(|| gm(&raw, "default", 0)).unwrap_or_else(|| "M".into());
            t.by_name.insert(name.clone(), id as u8);
            t.by_id.insert(id as u8, (name.clone(), g));
        }
        (!t.by_id.is_empty()).then_some(t)
    }

    pub fn write_json(&self, path: &Path) -> std::io::Result<()> {
        let mut s = String::from("{\n");
        let n = self.by_id.len();
        for (i, (id, (name, g))) in self.by_id.iter().enumerate() {
            s.push_str(&format!("  \"{id}\": [\"{name}\", \"{g}\"]{}\n", if i + 1 < n { "," } else { "" }));
        }
        s.push_str("}\n");
        std::fs::write(path, s)
    }
}

/// A retail (v19.2 header) model's `$surfaceprop` name.
pub fn model_surfaceprop(rmdl: &[u8]) -> Option<String> {
    let at = u16::from_le_bytes([*rmdl.get(0xA6)?, *rmdl.get(0xA7)?]) as usize;
    let s = rmdl.get(at..)?;
    let end = s.iter().position(|&c| c == 0)?;
    let name = std::str::from_utf8(&s[..end]).ok()?;
    (!name.is_empty() && name.len() < 64 && name.bytes().all(|c| c.is_ascii_graphic())).then(|| name.to_ascii_lowercase())
}
