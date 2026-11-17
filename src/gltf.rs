//! A small glTF-binary writer: indexed triangle meshes, PBR materials, and
//! textures embedded in the BIN chunk. Coordinates are written exactly as given
//! (here: Source units, Z up); the client converts.

use anyhow::Result;
use serde_json::{json, Value};

#[derive(Default)]
pub struct Prim {
    pub material: usize,
    pub positions: Vec<[f32; 3]>,
    pub normals: Vec<[f32; 3]>,
    pub uv0: Vec<[f32; 2]>,
    /// Lightmap UVs; empty when the geometry has none.
    pub uv1: Vec<[f32; 2]>,
    /// Vertex colours (RGBA8); empty when unused.
    pub colors: Vec<[u8; 4]>,
    pub indices: Vec<u32>,
}

impl Prim {
    pub fn triangles(&self) -> usize {
        self.indices.len() / 3
    }
}

#[derive(Clone, Default)]
pub struct Mat {
    pub name: String,
    pub base_color: Option<usize>,
    pub normal: Option<usize>,
    pub emissive: Option<usize>,
    pub lightmap: Option<usize>,
    pub factor: [f32; 4],
    pub alpha_mask: bool,
    pub alpha_blend: bool,
    pub double_sided: bool,
    pub extras: Value,
}

pub struct Image {
    pub bytes: Vec<u8>,
    pub mime: &'static str,
    pub name: String,
}

struct Bin {
    bytes: Vec<u8>,
    views: Vec<Value>,
}

impl Bin {
    fn push(&mut self, data: &[u8], target: Option<u32>) -> usize {
        while self.bytes.len() % 4 != 0 {
            self.bytes.push(0);
        }
        let off = self.bytes.len();
        self.bytes.extend_from_slice(data);
        let mut v = json!({"buffer": 0, "byteOffset": off, "byteLength": data.len()});
        if let Some(t) = target {
            v["target"] = json!(t);
        }
        self.views.push(v);
        self.views.len() - 1
    }
}

fn f32s<const N: usize>(v: &[[f32; N]]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * N * 4);
    for a in v {
        for x in a {
            out.extend_from_slice(&x.to_le_bytes());
        }
    }
    out
}

/// Returns the glb bytes and the number of glTF primitives (= draw calls).
pub fn write(prims: &[Prim], mats: &[Mat], images: &[Image], scene_extras: Value) -> Result<(Vec<u8>, usize)> {
    let mut draws = 0usize;
    let mut bin = Bin { bytes: Vec::new(), views: Vec::new() };
    let mut accessors: Vec<Value> = Vec::new();
    let mut meshes: Vec<Value> = Vec::new();
    let mut nodes: Vec<Value> = Vec::new();
    for p in prims.iter().filter(|p| !p.indices.is_empty()) {
        // 16-bit indices halve the index buffer, so big groups are cut into
        // chunks of at most 65535 vertices (still one mesh, one node).
        let mut gl_prims: Vec<Value> = Vec::new();
        for chunk in split_u16(p) {
            gl_prims.push(write_prim(&mut bin, &mut accessors, &chunk, p.material));
            draws += 1;
        }
        let name = mats.get(p.material).map(|m| m.name.clone()).unwrap_or_default();
        meshes.push(json!({"name": name, "primitives": gl_prims}));
        nodes.push(json!({"name": name, "mesh": meshes.len() - 1}));
    }
    let mut gl_images: Vec<Value> = Vec::new();
    let mut textures: Vec<Value> = Vec::new();
    for img in images {
        let v = bin.push(&img.bytes, None);
        gl_images.push(json!({"bufferView": v, "mimeType": img.mime, "name": img.name}));
        textures.push(json!({"source": gl_images.len() - 1, "sampler": 0}));
    }
    let materials: Vec<Value> = mats
        .iter()
        .map(|m| {
            let mut pbr = json!({"baseColorFactor": m.factor, "metallicFactor": 0.0, "roughnessFactor": 0.85});
            if let Some(t) = m.base_color {
                pbr["baseColorTexture"] = json!({"index": t});
            }
            let mut out = json!({"name": m.name, "pbrMetallicRoughness": pbr, "doubleSided": m.double_sided});
            if let Some(t) = m.normal {
                out["normalTexture"] = json!({"index": t});
            }
            if let Some(t) = m.emissive {
                out["emissiveTexture"] = json!({"index": t});
                out["emissiveFactor"] = json!([1.0, 1.0, 1.0]);
            }
            if m.alpha_blend {
                out["alphaMode"] = json!("BLEND");
            } else if m.alpha_mask {
                out["alphaMode"] = json!("MASK");
                out["alphaCutoff"] = json!(0.5);
            }
            let mut extras = if m.extras.is_object() { m.extras.clone() } else { json!({}) };
            if let Some(t) = m.lightmap {
                // Not core glTF: the lightmap texture index, sampled with
                // TEXCOORD_1. See README for how the page is encoded.
                extras["lightmapTexture"] = json!(t);
            }
            out["extras"] = extras;
            out
        })
        .collect();
    let node_ids: Vec<usize> = (0..nodes.len()).collect();
    let mut doc = json!({
        "asset": {"version": "2.0", "generator": "r5-web-client export-map"},
        "scene": 0,
        "scenes": [{"nodes": node_ids, "extras": scene_extras}],
        "nodes": nodes,
        "meshes": meshes,
        "materials": materials,
        "accessors": accessors,
        "bufferViews": bin.views,
        "buffers": [{"byteLength": bin.bytes.len()}],
        "extensionsUsed": ["KHR_mesh_quantization"],
        "extensionsRequired": ["KHR_mesh_quantization"],
    });
    if !gl_images.is_empty() {
        doc["images"] = json!(gl_images);
        doc["textures"] = json!(textures);
        // Repeat wrap, trilinear: world UVs tile far outside 0..1.
        doc["samplers"] = json!([{"magFilter": 9729, "minFilter": 9987, "wrapS": 10497, "wrapT": 10497}]);
    }
    let mut json_bytes = serde_json::to_vec(&doc)?;
    while json_bytes.len() % 4 != 0 {
        json_bytes.push(b' ');
    }
    let mut bin_bytes = bin.bytes;
    while bin_bytes.len() % 4 != 0 {
        bin_bytes.push(0);
    }
    let total = 12 + 8 + json_bytes.len() + 8 + bin_bytes.len();
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&0x4654_6C67u32.to_le_bytes());
    out.extend_from_slice(&2u32.to_le_bytes());
    out.extend_from_slice(&(total as u32).to_le_bytes());
    out.extend_from_slice(&(json_bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(&0x4E4F_534Au32.to_le_bytes());
    out.extend_from_slice(&json_bytes);
    out.extend_from_slice(&(bin_bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(&0x004E_4942u32.to_le_bytes());
    out.extend_from_slice(&bin_bytes);
    Ok((out, draws))
}

/// Cuts a primitive into pieces whose vertex count fits 16-bit indices.
fn split_u16(p: &Prim) -> Vec<Prim> {
    if p.positions.len() <= 65535 {
        return vec![Prim {
            material: p.material,
            positions: p.positions.clone(),
            normals: p.normals.clone(),
            uv0: p.uv0.clone(),
            uv1: p.uv1.clone(),
            colors: p.colors.clone(),
            indices: p.indices.clone(),
        }];
    }
    let mut out = Vec::new();
    let mut cur = Prim { material: p.material, ..Default::default() };
    let mut remap: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();
    let flush = |cur: &mut Prim, remap: &mut std::collections::HashMap<u32, u32>, out: &mut Vec<Prim>| {
        if !cur.indices.is_empty() {
            out.push(std::mem::replace(cur, Prim { material: p.material, ..Default::default() }));
        }
        remap.clear();
    };
    for t in p.indices.chunks_exact(3) {
        let new = t.iter().filter(|i| !remap.contains_key(i)).count();
        if cur.positions.len() + new > 65535 {
            flush(&mut cur, &mut remap, &mut out);
        }
        for &i in t {
            let local = *remap.entry(i).or_insert_with(|| {
                let k = i as usize;
                cur.positions.push(p.positions[k]);
                if p.normals.len() == p.positions.len() {
                    cur.normals.push(p.normals[k]);
                }
                if p.uv0.len() == p.positions.len() {
                    cur.uv0.push(p.uv0[k]);
                }
                if p.uv1.len() == p.positions.len() {
                    cur.uv1.push(p.uv1[k]);
                }
                if p.colors.len() == p.positions.len() {
                    cur.colors.push(p.colors[k]);
                }
                (cur.positions.len() - 1) as u32
            });
            cur.indices.push(local);
        }
    }
    flush(&mut cur, &mut remap, &mut out);
    out
}

/// Writes one primitive's buffers: float positions and UVs (coordinates stay
/// exactly as the game has them), normals as normalized bytes
/// (KHR_mesh_quantization), 16-bit indices.
fn write_prim(bin: &mut Bin, accessors: &mut Vec<Value>, p: &Prim, material: usize) -> Value {
    let n = p.positions.len();
    let (mut lo, mut hi) = ([f32::MAX; 3], [f32::MIN; 3]);
    for v in &p.positions {
        for k in 0..3 {
            lo[k] = lo[k].min(v[k]);
            hi[k] = hi[k].max(v[k]);
        }
    }
    let mut attrs = serde_json::Map::new();
    let pv = bin.push(&f32s(&p.positions), Some(34962));
    accessors.push(json!({"bufferView": pv, "componentType": 5126, "count": n, "type": "VEC3", "min": lo, "max": hi}));
    attrs.insert("POSITION".into(), json!(accessors.len() - 1));
    if p.normals.len() == n {
        let mut b = Vec::with_capacity(n * 4);
        for v in &p.normals {
            for x in v {
                b.push((x.clamp(-1.0, 1.0) * 127.0).round() as i8 as u8);
            }
            b.push(0);
        }
        let v = bin.push(&b, Some(34962));
        bin.views[v]["byteStride"] = json!(4);
        accessors.push(json!({"bufferView": v, "componentType": 5120, "normalized": true, "count": n, "type": "VEC3"}));
        attrs.insert("NORMAL".into(), json!(accessors.len() - 1));
    }
    if p.uv0.len() == n {
        let v = bin.push(&f32s(&p.uv0), Some(34962));
        accessors.push(json!({"bufferView": v, "componentType": 5126, "count": n, "type": "VEC2"}));
        attrs.insert("TEXCOORD_0".into(), json!(accessors.len() - 1));
    }
    if p.uv1.len() == n {
        let v = bin.push(&f32s(&p.uv1), Some(34962));
        accessors.push(json!({"bufferView": v, "componentType": 5126, "count": n, "type": "VEC2"}));
        attrs.insert("TEXCOORD_1".into(), json!(accessors.len() - 1));
    }
    if p.colors.len() == n {
        let b: Vec<u8> = p.colors.iter().flat_map(|c| *c).collect();
        let v = bin.push(&b, Some(34962));
        accessors.push(json!({"bufferView": v, "componentType": 5121, "normalized": true, "count": n, "type": "VEC4"}));
        attrs.insert("COLOR_0".into(), json!(accessors.len() - 1));
    }
    let ib: Vec<u8> = p.indices.iter().flat_map(|i| (*i as u16).to_le_bytes()).collect();
    let iv = bin.push(&ib, Some(34963));
    accessors.push(json!({"bufferView": iv, "componentType": 5123, "count": p.indices.len(), "type": "SCALAR"}));
    json!({"attributes": attrs, "indices": accessors.len() - 1, "material": material, "mode": 4})
}
