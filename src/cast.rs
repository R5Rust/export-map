//! Cast model reader (github.com/dtzxporter/cast), for retail Apex models (mdl v21) that r5ed's raw rmdl reader
//! (studiohdr v16/v17 only) can't parse. RSX 2.4.1 writes one Cast file per LOD (`<stem>_LOD<n>.cast`).
//!
//! Layout: "cast" magic, version, root count, flags; then nodes. Node = id u32, size u32, hash u64, property count
//! u32, child count u32, properties, children. Property = type u16, name length u16, value count u32, name, values
//! (strings are NUL-terminated). RSX writes root > model > {skeleton > bones, material*, mesh*}; a material's hash is
//! the material asset's guid (checked: stasis_trim_02_avt_rgdp = 0x9B8CDE19BA793A2D), a mesh's `m` names it.

use anyhow::{anyhow, bail, Result};
use rpak::rmdl::{Mesh, RmdlModel};

/// Meshes left out because they aren't in the default bodygroup (export-map prints the total).
pub static SKIPPED_BODYGROUP_MESHES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

const MODEL: u32 = 0x6C64_6F6D;
const MESH: u32 = 0x6873_656D;
const MATERIAL: u32 = 0x6C74_616D;
const BONE: u32 = 0x656E_6F62;

enum Val {
    U(Vec<u64>),
    F(Vec<f32>),
    S(String),
}

struct Node {
    id: u32,
    hash: u64,
    props: Vec<(String, Val)>,
    children: Vec<Node>,
}

impl Node {
    fn get(&self, name: &str) -> Option<&Val> {
        self.props.iter().find(|(n, _)| n == name).map(|(_, v)| v)
    }
    fn floats(&self, name: &str) -> &[f32] {
        match self.get(name) {
            Some(Val::F(v)) => v,
            _ => &[],
        }
    }
    fn uints(&self, name: &str) -> &[u64] {
        match self.get(name) {
            Some(Val::U(v)) => v,
            _ => &[],
        }
    }
    fn string(&self, name: &str) -> &str {
        match self.get(name) {
            Some(Val::S(s)) => s,
            _ => "",
        }
    }
}

struct Rd<'a> {
    b: &'a [u8],
    at: usize,
}

impl Rd<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8]> {
        let s = self.b.get(self.at..self.at + n).ok_or_else(|| anyhow!("cast: truncated at {}", self.at))?;
        self.at += n;
        Ok(s)
    }
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn node(&mut self) -> Result<Node> {
        let start = self.at;
        let (id, size, hash) = (self.u32()?, self.u32()? as usize, self.u64()?);
        let (nprop, nchild) = (self.u32()?, self.u32()?);
        let mut props = Vec::new();
        for _ in 0..nprop {
            let (ty, nlen, count) = (self.u16()?, self.u16()? as usize, self.u32()? as usize);
            let name = String::from_utf8_lossy(self.take(nlen)?).into_owned();
            let v = match ty {
                0x73 => {
                    let rest = &self.b[self.at..];
                    let end = rest.iter().position(|&c| c == 0).ok_or_else(|| anyhow!("cast: unterminated string"))?;
                    let s = String::from_utf8_lossy(&rest[..end]).into_owned();
                    self.at += end + 1;
                    Val::S(s)
                }
                0x62 => Val::U(self.take(count)?.iter().map(|&x| x as u64).collect()),
                0x68 => Val::U(self.take(count * 2)?.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]]) as u64).collect()),
                0x69 => Val::U(self.take(count * 4)?.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap()) as u64).collect()),
                0x6C => Val::U(self.take(count * 8)?.chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().unwrap())).collect()),
                // f32 scalar / vec2 / vec3 / vec4, flattened
                0x66 | 0x7632 | 0x7633 | 0x7634 => {
                    let n = match ty {
                        0x66 => 1,
                        0x7632 => 2,
                        0x7633 => 3,
                        _ => 4,
                    };
                    Val::F(self.take(count * n * 4)?.chunks_exact(4).map(|c| f32::from_le_bytes(c.try_into().unwrap())).collect())
                }
                0x64 => Val::F(self.take(count * 8)?.chunks_exact(8).map(|c| f64::from_le_bytes(c.try_into().unwrap()) as f32).collect()),
                t => bail!("cast: unknown property type {t:#x} ({name})"),
            };
            props.push((name, v));
        }
        let mut children = Vec::new();
        for _ in 0..nchild {
            children.push(self.node()?);
        }
        self.at = start + size;
        Ok(Node { id, hash, props, children })
    }
}

/// The model in a Cast file, as r5ed's `RmdlModel`: `textures` holds the material guids (one per Cast material, in
/// file order), each mesh's `material` indexes it. Meshes over 65535 vertices are split (the struct has u16 indices).
pub fn read_model(b: &[u8]) -> Result<RmdlModel> {
    if b.len() < 16 || &b[0..4] != b"cast" {
        bail!("not a cast file");
    }
    let mut r = Rd { b, at: 4 };
    let (_ver, nroot, _flags) = (r.u32()?, r.u32()?, r.u32()?);
    let mut roots = Vec::new();
    for _ in 0..nroot {
        roots.push(r.node()?);
    }
    let model = roots.iter().flat_map(|n| n.children.iter()).find(|n| n.id == MODEL).ok_or_else(|| anyhow!("cast: no model node"))?;
    let mut textures: Vec<u64> = model.children.iter().filter(|n| n.id == MATERIAL).map(|n| n.hash).collect();
    let bone_count = model.children.iter().flat_map(|n| n.children.iter()).filter(|n| n.id == BONE).count();
    let mut meshes = Vec::new();
    let (mut lo, mut hi) = ([f32::MAX; 3], [f32::MIN; 3]);
    for m in model.children.iter().filter(|n| n.id == MESH) {
        // RSX names a mesh `<bodypart>_<model index>_LOD<n>_<material>`. The game spawns props and loot with body 0,
        // which draws model 0 of every bodypart; weapons keep each attachment (sights, suppressors, lasers) in its own
        // bodypart with an empty model 0, so drawing every mesh put all of them on the gun at once.
        // R5Flowstate's fork leaves the LOD out: `<bodypart>_<model index>_<material>` (the material's file name, else
        // its guid in decimal). Without "_LOD" the old split took the material's trailing number (quarryblock_01) for
        // the model index and dropped 73 Firing Range models once the model cache was rebuilt with the fork.
        let full = m.string("n");
        let mat_hash = m.uints("m").first().copied().unwrap_or(0);
        let mat_name = model.children.iter().find(|n| n.id == MATERIAL && n.hash == mat_hash).map(|n| n.string("n").to_string());
        let stem = [mat_name.unwrap_or_default(), mat_hash.to_string()]
            .iter()
            .filter(|s| !s.is_empty())
            .find_map(|s| full.strip_suffix(s.as_str()).and_then(|r| r.strip_suffix('_')))
            .map(|r| r.split("_LOD").next().unwrap_or(r))
            .or_else(|| full.contains("_LOD").then(|| full.split("_LOD").next().unwrap_or(full)));
        if let Some(stem) = stem {
            if let Some(idx) = stem.rsplit('_').next().and_then(|i| i.parse::<u32>().ok()) {
                if idx != 0 {
                    SKIPPED_BODYGROUP_MESHES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    continue;
                }
            }
        }
        let pos: Vec<[f32; 3]> = m.floats("vp").chunks_exact(3).map(|c| [c[0], c[1], c[2]]).collect();
        let nrm: Vec<[f32; 3]> = m.floats("vn").chunks_exact(3).map(|c| [c[0], c[1], c[2]]).collect();
        let uv: Vec<[f32; 2]> = m.floats("u0").chunks_exact(2).map(|c| [c[0], c[1]]).collect();
        // "vc": RSX's Color32 (bytes r, g, b, a) as a little-endian u32 per vertex. Retail props use it: the
        // "*_vert_color" paints are white in the texture and take their colour from here (VS: pow(rgb, 2.2))
        let vcol: Vec<[u8; 4]> = m.uints("vc").iter().map(|&v| [v as u8, (v >> 8) as u8, (v >> 16) as u8, (v >> 24) as u8]).collect();
        let col = |i: usize| vcol.get(i).copied().unwrap_or([255; 4]);
        let faces = m.uints("f");
        if pos.is_empty() || faces.len() < 3 {
            continue;
        }
        let mat_hash = m.uints("m").first().copied().unwrap_or(0);
        let slot = match textures.iter().position(|&t| t == mat_hash) {
            Some(i) => i,
            None => {
                textures.push(mat_hash);
                textures.len() - 1
            }
        } as u16;
        for p in &pos {
            for a in 0..3 {
                lo[a] = lo[a].min(p[a]);
                hi[a] = hi[a].max(p[a]);
            }
        }
        let vert = |i: usize| (pos[i], nrm.get(i).copied().unwrap_or([0.0, 0.0, 1.0]), uv.get(i).copied().unwrap_or([0.0; 2]));
        if pos.len() <= 65536 {
            meshes.push(Mesh {
                material: slot,
                center: [0.0; 3],
                positions: pos.clone(),
                normals: (0..pos.len()).map(|i| vert(i).1).collect(),
                uv0: (0..pos.len()).map(|i| vert(i).2).collect(),
                colors: if vcol.is_empty() { Vec::new() } else { (0..pos.len()).map(col).collect() },
                indices: faces.iter().map(|&i| i as u16).collect(),
            });
            continue;
        }
        // split by triangles into chunks that each reference at most 65535 vertices
        let mut cur = Mesh { material: slot, center: [0.0; 3], positions: Vec::new(), normals: Vec::new(), uv0: Vec::new(), colors: Vec::new(), indices: Vec::new() };
        let mut remap = std::collections::HashMap::new();
        for t in faces.chunks_exact(3) {
            if cur.positions.len() + 3 > 65535 {
                meshes.push(std::mem::replace(&mut cur, Mesh { material: slot, center: [0.0; 3], positions: Vec::new(), normals: Vec::new(), uv0: Vec::new(), colors: Vec::new(), indices: Vec::new() }));
                remap.clear();
            }
            for &i in t {
                let i = i as usize;
                let local = *remap.entry(i).or_insert_with(|| {
                    let (p, n, u) = vert(i);
                    cur.positions.push(p);
                    cur.normals.push(n);
                    cur.uv0.push(u);
                    if !vcol.is_empty() {
                        cur.colors.push(col(i));
                    }
                    cur.positions.len() - 1
                });
                cur.indices.push(local as u16);
            }
        }
        if !cur.indices.is_empty() {
            meshes.push(cur);
        }
    }
    let name = model.children.iter().flat_map(|n| n.children.iter()).find(|n| n.id == BONE).map(|b| b.string("n").to_string()).unwrap_or_default();
    Ok(RmdlModel {
        name,
        version: 21,
        bounds_min: lo,
        bounds_max: hi,
        illum_position: [0.0; 3],
        bone_count: bone_count as u16,
        contents: 1,
        textures,
        surface_prop: String::new(),
        meshes,
    })
}
