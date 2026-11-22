//! The map's real collision: Apex's BVH4 trees decoded into one triangle soup.
//!
//! Sources (all world space, Source inches, Z up):
//! - the map BSP's collision tree for model 0 (LUMP_BVH_NODES 0x12 + LUMP_BVH_LEAF_DATA 0x13
//!   + LUMP_VERTICES 0x03 + LUMP_CONTENTS_MASKS 0x10): triangle / quad leaves (kinds 4-7),
//!   convex hull leaves (kind 8, triangulated from their own face sets) and bundles (kind 3);
//! - static props named by the tree's prop leaves (kind 9): each one's model carries its own
//!   BVH4 collision (studiohdr `bvhOffset`, same node/leaf format), placed with the prop's
//!   origin/angles/scale.
//!
//! Every triangle keeps the contents mask of the leaf it came from, so the game can pick what a
//! trace hits (players: `TRACE_MASK_PLAYERSOLID`). Layout follows RSX's `bvh.cpp` and r5ed's
//! `rcomp::collision` decoder; the node walk is cross-checked against r5ed's in `--collision`.
//!
//! `collision.bin` (little endian):
//!   magic `R5COLL\0\0`, u32 version (1), u32 vertex count V, u32 triangle count T, u32 0,
//!   V x [f32; 3] positions, T x [u32; 3] indices, T x u32 contents mask,
//!   T x u8 source (0 world poly, 1 world hull, 2 static prop collision tree, 3 static prop
//!   physics tree, only written with `--prop-physics`). Player movement uses 0, 1 and 2.
//!   Version 2 adds T x u8 surface type: the surfaceproperties id (`surfacePropId` of the leaf's
//!   dsurfaceproperty_t, which `gamematerial` and the impact tables key on), 255 = unknown.
//! Prop triangles carry [`prop_contents`] (leaf mask combined with the model's mask).

use std::collections::{BTreeMap, HashMap};

use anyhow::{anyhow, Result};
use glam::{Quat, Vec3};
use rbsp::lumps::collision::BvhNode;

pub const SRC_POLY: u8 = 0;
pub const SRC_HULL: u8 = 1;
/// A prop's main collision tree (tree 0), the one traces use.
pub const SRC_PROP: u8 = 2;
/// A prop's physics tree (the vphysics replica), only with `--prop-physics`.
pub const SRC_PROP_PHYSICS: u8 = 3;

/// `TRACE_MASK_PLAYERSOLID` (R5Regenis data/natives/server_consts.tsv): SOLID | WINDOW | GRATE |
/// MOVEABLE | PLAYERCLIP | MONSTER. Only the static-world bits matter here.
pub const MASK_PLAYERSOLID: u32 = 0x0201_400B;

/// A prop triangle's contents as a trace sees it: the BSP prop leaf decides which traces reach
/// the prop at all (bullet-only clutter has no player bits there), the model's own mask decides
/// what the triangle is. Player bits = the model's player bits if the leaf admits player traces;
/// every other bit comes from the leaf (BULLETCLIP, BLOCKLOS, NOCLIMB, ...).
pub fn prop_contents(leaf: u32, model: u32) -> u32 {
    let player = if leaf & MASK_PLAYERSOLID != 0 { model & MASK_PLAYERSOLID } else { 0 };
    (leaf & !MASK_PLAYERSOLID) | player
}

const CHILD_NODE: u32 = 0;
const CHILD_EMPTY: u32 = 1;
const CHILD_BUNDLE: u32 = 3;
const CHILD_HULL: u32 = 8;
const CHILD_PROP: u32 = 9;

/// One BVH4 tree as raw bytes: the BSP's lumps for a model, or a model's embedded collision.
pub struct Tree<'a> {
    /// 64-byte nodes; child node indices count from `node_base`.
    pub nodes: &'a [u8],
    pub node_base: usize,
    /// The dword leaf stream; leaf offsets count from `leaf_base` (in dwords).
    pub leaf: &'a [u8],
    pub leaf_base: usize,
    /// The vertex region: float3 for leaf kinds 4/6, int16x3 for the packed kinds 5/7.
    pub verts: &'a [u8],
    /// Packed-vertex dequantisation: `origin + q * scale` (scale already x65536).
    pub origin: Vec3,
    pub scale: f32,
    pub masks: &'a [u32],
    /// The tree's surface property records (dsurfaceproperty_t, 8 bytes: u16, u8 surfacePropId, u8 contents,
    /// i32 name); a poly leaf header's low 12 bits index them.
    pub surf: &'a [u8],
}

#[derive(Default, Debug)]
pub struct Stats {
    pub leaves_by_kind: BTreeMap<u32, usize>,
    pub tris_by_kind: BTreeMap<u32, usize>,
    pub bad_refs: usize,
    pub nodes_visited: usize,
}

/// A prop leaf: static prop index (low 16 bits of the leaf word) and the leaf's contents mask.
#[derive(Clone, Copy, Debug)]
pub struct PropRef {
    pub index: u32,
    pub mask: u32,
}

pub struct Decoded {
    /// (corners, contents mask, source, surface type)
    pub tris: Vec<([Vec3; 3], u32, u8, u8)>,
    pub props: Vec<PropRef>,
    pub stats: Stats,
}

impl Tree<'_> {
    fn node(&self, i: usize) -> Option<BvhNode> {
        let at = i.checked_mul(64)?;
        self.nodes.get(at..at + 64).map(bytemuck::pod_read_unaligned)
    }
    fn word(&self, i: usize) -> Option<u32> {
        let at = i.checked_mul(4)?;
        self.leaf.get(at..at + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn surf_type(&self, idx: u32) -> u8 {
        self.surf.get(idx as usize * 8 + 2).copied().unwrap_or(255)
    }
    fn byte(&self, at: usize) -> Option<u8> {
        self.leaf.get(at).copied()
    }
    fn f32_at_byte(&self, at: usize) -> Option<f32> {
        self.leaf.get(at..at + 4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn i16_at_byte(&self, at: usize) -> Option<i16> {
        self.leaf.get(at..at + 2).map(|b| i16::from_le_bytes([b[0], b[1]]))
    }
    fn vert_f32(&self, i: u32) -> Option<Vec3> {
        let at = (i as usize).checked_mul(12)?;
        let b = self.verts.get(at..at + 12)?;
        let f = |o: usize| f32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        Some(Vec3::new(f(0), f(4), f(8)))
    }
    fn vert_packed(&self, i: u32) -> Option<Vec3> {
        let at = (i as usize).checked_mul(6)?;
        let b = self.verts.get(at..at + 6)?;
        let q = |o: usize| i16::from_le_bytes([b[o], b[o + 1]]) as f32;
        Some(self.origin + Vec3::new(q(0), q(2), q(4)) * self.scale)
    }

    /// Walks the whole tree from node 0 (relative) and decodes every leaf.
    pub fn decode(&self) -> Decoded {
        let mut out = Decoded { tris: Vec::new(), props: Vec::new(), stats: Stats::default() };
        let mut seen: std::collections::HashSet<usize> = std::collections::HashSet::new();
        let mut stack = vec![0usize];
        while let Some(rel) = stack.pop() {
            let ni = self.node_base + rel;
            if !seen.insert(ni) {
                continue;
            }
            let Some(node) = self.node(ni) else {
                out.stats.bad_refs += 1;
                continue;
            };
            out.stats.nodes_visited += 1;
            let node_mask = self.masks.get(node.contents_index() as usize).copied().unwrap_or(0);
            for slot in 0..4 {
                let kind = node.child_kind(slot);
                let at = node.child_index(slot) as usize;
                match kind {
                    CHILD_EMPTY => {}
                    CHILD_NODE => stack.push(at),
                    CHILD_BUNDLE => {
                        let start = self.leaf_base + at;
                        let Some(count) = self.word(start) else {
                            out.stats.bad_refs += 1;
                            continue;
                        };
                        if count > 4096 {
                            out.stats.bad_refs += 1;
                            continue;
                        }
                        let mut pay = start + 1 + count as usize;
                        for k in 0..count as usize {
                            let Some(desc) = self.word(start + 1 + k) else { break };
                            let mask = self.masks.get((desc & 0xFF) as usize).copied().unwrap_or(0);
                            self.leaf_at((desc >> 8) & 0xFF, pay, mask, &mut out);
                            pay += (desc >> 16) as usize;
                        }
                    }
                    k => self.leaf_at(k, self.leaf_base + at, node_mask, &mut out),
                }
            }
        }
        out
    }

    fn leaf_at(&self, kind: u32, at: usize, mask: u32, out: &mut Decoded) {
        *out.stats.leaves_by_kind.entry(kind).or_default() += 1;
        let before = out.tris.len();
        match kind {
            4..=7 => {
                if self.poly(kind, at, mask, SRC_POLY, None, out).is_none() {
                    out.stats.bad_refs += 1;
                }
            }
            CHILD_HULL => {
                if self.hull(at, mask, out).is_none() {
                    out.stats.bad_refs += 1;
                }
            }
            CHILD_PROP => match self.word(at) {
                Some(w) => out.props.push(PropRef { index: w & 0xFFFF, mask }),
                None => out.stats.bad_refs += 1,
            },
            _ => {}
        }
        *out.stats.tris_by_kind.entry(kind).or_default() += out.tris.len() - before;
    }

    /// A triangle (4/5) or parallelogram (6/7) leaf. Odd kinds read int16 vertices. `local`
    /// resolves indices into a hull's own corners instead of the tree's vertex region.
    /// Returns the leaf's size in dwords.
    fn poly(&self, kind: u32, at: usize, mask: u32, src: u8, local: Option<&[Vec3]>, out: &mut Decoded) -> Option<usize> {
        let h = self.word(at)?;
        let st = self.surf_type(h & 0xFFF);
        let count = ((h >> 12) & 0xF) as usize + 1;
        let quad = kind & 2 != 0;
        let packed = kind & 1 != 0;
        let (m, s1, s2) = if quad { (0x3FFu32, 10, 19) } else { (0x7FFu32, 11, 20) };
        let mut run = (h >> 16) << 10;
        for p in 0..count {
            let pk = self.word(at + 1 + p)?;
            run += pk & m;
            let ids = [run, run + ((pk >> s1) & 0x1FF) + 1, run + ((pk >> s2) & 0x1FF) + 1];
            let get = |i: u32| -> Option<Vec3> {
                match local {
                    Some(pts) => pts.get(i as usize).copied(),
                    None if packed => self.vert_packed(i),
                    None => self.vert_f32(i),
                }
            };
            let (Some(a), Some(b), Some(c)) = (get(ids[0]), get(ids[1]), get(ids[2])) else {
                out.stats.bad_refs += 1;
                continue;
            };
            out.tris.push(([a, b, c], mask, src, st));
            if quad {
                // The fourth corner is implied: b + c - a (single precision, like the engine).
                out.tris.push(([c, b, b + c - a], mask, src, st));
            }
        }
        Some(1 + count)
    }

    /// A convex hull leaf: packed corners in its own frame, then face planes, then its tri and
    /// quad sets as poly leaves indexing those corners.
    fn hull(&self, at: usize, mask: u32, out: &mut Decoded) -> Option<()> {
        let b = at * 4;
        let nv = self.byte(b)? as usize;
        let npl = self.byte(b + 1)? as usize;
        let nts = self.byte(b + 2)? as usize;
        let nqs = self.byte(b + 3)? as usize;
        let origin = Vec3::new(self.f32_at_byte(b + 4)?, self.f32_at_byte(b + 8)?, self.f32_at_byte(b + 12)?);
        let scale = self.f32_at_byte(b + 16)? * 65536.0;
        let mut pts = Vec::with_capacity(nv);
        for i in 0..nv {
            let o = b + 20 + i * 6;
            let q = Vec3::new(self.i16_at_byte(o)? as f32, self.i16_at_byte(o + 2)? as f32, self.i16_at_byte(o + 4)? as f32);
            pts.push(origin + q * scale);
        }
        let mut off = at + (20 + nv * 6 + npl * 3).div_ceil(4);
        for k in 0..nts + nqs {
            let kind = if k < nts { 5 } else { 7 };
            off += self.poly(kind, off, mask, SRC_HULL, Some(&pts), out)?;
        }
        Some(())
    }
}

/// A model's embedded collision (studiohdr v16/v17 `bvhOffset`), in model space, one entry per
/// tree the model carries.
/// `v19`: the retail header (studiohdr_v19_2_t, mdl v20/v21) keeps `bvhOffset` at 0xE6, ten fields later.
pub fn rmdl_collision(rmdl: &[u8], v19: bool) -> Result<Option<Vec<Decoded>>> {
    let bvh_field: usize = if v19 { 0xE6 } else { 0xD6 };
    let raw = rmdl.get(bvh_field..bvh_field + 2).map(|b| u16::from_le_bytes([b[0], b[1]])).ok_or_else(|| anyhow!("short studiohdr"))?;
    if raw == 0 {
        return Ok(None);
    }
    // FIX_OFFSET: an odd value means the even part is shifted left by 4.
    let fixed = ((raw & 0xFFFE) as usize) << (4 * (raw & 1) as usize);
    let i32_at = |o: usize| rmdl.get(o..o + 4).map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    let f32_at = |o: usize| rmdl.get(o..o + 4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    // RSX reads it from the header's base; accept the field-relative reading too if that is the
    // only one that parses.
    let mut found = None;
    for base in [fixed, bvh_field + fixed] {
        let (Some(cm), Some(sp), Some(sn), Some(hc)) = (i32_at(base), i32_at(base + 4), i32_at(base + 8), i32_at(base + 12)) else { continue };
        let ok = (1..=64).contains(&hc) && sp >= 16 && cm >= sp && sn >= cm && base + (sn as usize) <= rmdl.len();
        if ok {
            found = Some((base, cm as usize, sn as usize, hc as usize));
            break;
        }
    }
    let Some((base, cm, sn, hc)) = found else { return Err(anyhow!("bvh header does not parse (offset {raw:#x})")) };
    // surface property records sit between surfacePropsIndex and contentMasksIndex
    let sp_at = i32_at(base + 4).unwrap_or(0).max(0) as usize;
    let surf: &[u8] = rmdl.get(base + sp_at..base + cm).unwrap_or(&[]);
    let masks: Vec<u32> = (0..(sn - cm) / 4).filter_map(|i| i32_at(base + cm + i * 4).map(|v| v as u32)).collect();
    let blob = &rmdl[base..];
    let mut all = Vec::new();
    for h in 0..hc {
        let ho = base + 16 + h * 40;
        let (Some(nd), Some(vt), Some(lf)) = (i32_at(ho + 4), i32_at(ho + 8), i32_at(ho + 12)) else { break };
        let origin = Vec3::new(f32_at(ho + 24).unwrap_or(0.0), f32_at(ho + 28).unwrap_or(0.0), f32_at(ho + 32).unwrap_or(0.0));
        let scale = f32_at(ho + 36).unwrap_or(0.0) * 65536.0;
        let (nd, vt, lf) = (nd as usize, vt as usize, lf as usize);
        if nd >= blob.len() || vt > blob.len() || lf > blob.len() {
            return Err(anyhow!("collision tree {h} points past the model"));
        }
        let tree = Tree {
            nodes: &blob[nd..],
            node_base: 0,
            leaf: &blob[lf..],
            leaf_base: 0,
            verts: &blob[vt..],
            origin,
            scale,
            masks: &masks,
            surf,
        };
        all.push(tree.decode());
    }
    Ok(Some(all))
}

/// The model's physics-tree index: the byte after `bvhOffset` (RSX: `bvhUnk[0]`). It is 0xFF on
/// every model with a single tree and 1 on every model with two, and exactly the models with a
/// vphysics `.phy` have two, so it names the physics replica (RSX's note in
/// `ExportPhysicsModelBVH`: a coarse shell "more or less identical to the vphysics one").
pub fn physics_tree_index(rmdl: &[u8], v19: bool) -> Option<usize> {
    rmdl.get(if v19 { 0xE8 } else { 0xD8 }).copied().filter(|&b| b != 0xFF).map(usize::from)
}

/// Splits a model's trees into (collision, physics). Tree 0 is the default the engine traces
/// (it alone carries surface-property data, which footsteps and impacts read); the physics tree
/// is only what the stored index points at.
pub fn split_trees(mut trees: Vec<Decoded>, physics: Option<usize>) -> (Option<Decoded>, Option<Decoded>) {
    let phys = physics.filter(|&i| i > 0 && i < trees.len()).map(|i| trees.swap_remove(i));
    let main = if trees.is_empty() { None } else { Some(trees.swap_remove(0)) };
    (main, phys)
}

/// The studiohdr's movement hull, for checking the collision's space.
pub fn rmdl_hull(rmdl: &[u8]) -> Option<(Vec3, Vec3)> {
    let f = |o: usize| rmdl.get(o..o + 4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    Some((Vec3::new(f(68)?, f(72)?, f(76)?), Vec3::new(f(80)?, f(84)?, f(88)?)))
}

/// The soup being written: deduplicated vertices, triangles with masks and sources.
#[derive(Default)]
pub struct Soup {
    pub pos: Vec<[f32; 3]>,
    pub idx: Vec<[u32; 3]>,
    pub mask: Vec<u32>,
    pub src: Vec<u8>,
    pub surf: Vec<u8>,
    lookup: HashMap<[u32; 3], u32>,
    pub degenerate: usize,
}

impl Soup {
    fn vert(&mut self, p: Vec3) -> u32 {
        let key = [p.x.to_bits(), p.y.to_bits(), p.z.to_bits()];
        if let Some(&i) = self.lookup.get(&key) {
            return i;
        }
        let i = self.pos.len() as u32;
        self.pos.push(p.to_array());
        self.lookup.insert(key, i);
        i
    }

    pub fn push(&mut self, t: [Vec3; 3], mask: u32, src: u8, surf: u8) {
        if (t[1] - t[0]).cross(t[2] - t[0]).length_squared() < 1e-8 {
            self.degenerate += 1;
            return;
        }
        let i = [self.vert(t[0]), self.vert(t[1]), self.vert(t[2])];
        if i[0] == i[1] || i[1] == i[2] || i[0] == i[2] {
            self.degenerate += 1;
            return;
        }
        self.idx.push(i);
        self.mask.push(mask);
        self.src.push(src);
        self.surf.push(surf);
    }

    pub fn bounds(&self) -> (Vec3, Vec3) {
        self.pos.iter().fold((Vec3::splat(f32::MAX), Vec3::splat(f32::MIN)), |(lo, hi), p| (lo.min(Vec3::from(*p)), hi.max(Vec3::from(*p))))
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(24 + self.pos.len() * 12 + self.idx.len() * 17);
        b.extend_from_slice(b"R5COLL\0\0");
        for v in [2u32, self.pos.len() as u32, self.idx.len() as u32, 0] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        for p in &self.pos {
            for c in p {
                b.extend_from_slice(&c.to_le_bytes());
            }
        }
        for t in &self.idx {
            for c in t {
                b.extend_from_slice(&c.to_le_bytes());
            }
        }
        for m in &self.mask {
            b.extend_from_slice(&m.to_le_bytes());
        }
        b.extend_from_slice(&self.src);
        b.extend_from_slice(&self.surf);
        b
    }
}

/// Places a model-space triangle with a static prop's transform.
pub fn place(t: &[Vec3; 3], origin: Vec3, rot: Quat, scale: f32) -> [Vec3; 3] {
    t.map(|p| origin + rot * (p * scale))
}
