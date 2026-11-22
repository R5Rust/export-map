//! export-map: turns an R5Flowstate map from the local install into
//! `mapcache/<map>/world.glb` + `meta.json` for the three.js client, and single
//! models into `mapcache/models/<path>.glb`.
//!
//!   cargo run --release -- [map]                 (default mp_rr_canyonlands_staging_mu1)
//!   cargo run --release -- --model mdl/x/y.rmdl  [--out mapcache/models]
//!
//! Everything game-side is read through r5ed's crates (rbsp, rcomp, rpak) and
//! RSX; the install is only ever read (RSX gets copies, see assets.rs).

mod assets;
mod cast;
mod coll;
mod gltf;
mod mats;
mod surfaces;
mod tex;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{anyhow, bail, Context, Result};
use glam::{EulerRot, Quat, Vec3};
use serde_json::{json, Value};

use assets::{Config, Paks};
use tex::Role;

const DEFAULT_MAP: &str = "mp_rr_canyonlands_staging_mu1";
const DEFAULT_RSX: &str = r"%USERPROFILE%\Downloads\r5ed-5ebc9ec-win64\r5ed\rsx.exe";
// S21 game root, only for S21 exports (retail exports set R5_GAME_ROOT). The main S21 install was removed
// 2026-11-20; r5-modkit's test copy has the same stock paks and maps (hardlinks) with its own dlls.
const DEFAULT_GAME: &str = r"%USERPROFILE%\Games\R5Flowstate-modkit-test";

struct Args {
    map: String,
    models: Vec<String>,
    out: Option<PathBuf>,
    world_cap: u32,
    /// None = same as `world_cap`, so props and world surfaces get the same texture ceiling
    prop_cap: Option<u32>,
    normals: bool,
    lightmaps: bool,
    clean: bool,
    prop_tri_budget: usize,
    collision_only: bool,
    prop_physics: bool,
    /// Old behaviour: props outside the play area use the game's lower LODs by distance.
    far_lods: bool,
    /// Old behaviour: every static prop baked into world.glb at one LOD (no props.json, no runtime LOD).
    bake_props: bool,
}

fn usage() -> ! {
    eprintln!(
        "usage: export-map [map] [--model <mdl/..rmdl>] [--out <dir>] [--tex <px>] [--prop-tex <px>] [--prop-tris <n>] [--far-lods] [--bake-props] [--no-normals] [--no-lightmap] [--clean] [--collision] [--prop-physics]\n\
         --tex <px> (default 4096) caps world textures, --prop-tex <px> props (default: same as --tex)
         --prop-tris <n>: triangle budget for all static props (default 16000000); over it, the models costing the most
         drop one of the game's LODs at a time, every copy alike. --far-lods: old distance rings (lower LODs outside the play area)
         Static props default to runtime LOD: each model once per LOD in model space + props.json (instances, switch
         points, fade distances). --bake-props: the old way, every prop baked into world.glb at one LOD (the two
         options above only apply then)
         --collision: only (re)write collision.bin, reusing the BSP the last export extracted\n\
         --prop-physics: also write static props' physics trees into collision.bin (source 3)\n\
         env: R5_GAME_ROOT (default {DEFAULT_GAME}), R5_RSX (default the r5ed bundle's rsx.exe), R5_EXPORT_WORK (default %TEMP%\\r5-export-map)"
    );
    std::process::exit(2)
}

fn parse_args() -> Args {
    let mut a = Args { map: DEFAULT_MAP.into(), models: Vec::new(), out: None, world_cap: 4096, prop_cap: None, normals: true, lightmaps: true, clean: false, prop_tri_budget: 16_000_000, collision_only: false, prop_physics: false, far_lods: false, bake_props: false };
    let mut it = std::env::args().skip(1);
    while let Some(s) = it.next() {
        match s.as_str() {
            "--model" => a.models.push(it.next().unwrap_or_else(|| usage())),
            "--out" => a.out = Some(PathBuf::from(it.next().unwrap_or_else(|| usage()))),
            "--tex" => a.world_cap = it.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| usage()),
            "--prop-tex" => a.prop_cap = Some(it.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| usage())),
            "--far-lods" => a.far_lods = true,
            "--bake-props" => a.bake_props = true,
            "--no-normals" => a.normals = false,
            "--prop-tris" => a.prop_tri_budget = it.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| usage()),
            "--no-lightmap" => a.lightmaps = false,
            "--clean" => a.clean = true,
            "--collision" => a.collision_only = true,
            "--prop-physics" => a.prop_physics = true,
            "-h" | "--help" => usage(),
            s if s.starts_with('-') => usage(),
            s => a.map = s.to_string(),
        }
    }
    a
}

/// `mapcache/` at the project root, two levels above this crate.
fn mapcache_root() -> PathBuf {
    let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    crate_dir.parent().and_then(Path::parent).map_or_else(|| crate_dir.join("mapcache"), |root| root.join("mapcache"))
}

/// `models_root` + a `/`-separated relative glb path, with native separators.
fn model_out(models_root: &Path, model: &str) -> PathBuf {
    models_root.join(model_rel_path(model).replace('/', std::path::MAIN_SEPARATOR_STR))
}

fn config() -> Config {
    let env = |k: &str| std::env::var_os(k).map(PathBuf::from);
    let rsx = env("R5_RSX").or_else(|| env("R5ED_RSX")).unwrap_or_else(|| PathBuf::from(DEFAULT_RSX));
    let game_root = env("R5_GAME_ROOT").unwrap_or_else(|| PathBuf::from(DEFAULT_GAME));
    let work = env("R5_EXPORT_WORK").unwrap_or_else(|| std::env::temp_dir().join("r5-export-map"));
    Config { game_root, rsx, work }
}

/// The pak families a map's assets live in, most specific first.
fn pak_set(map: &str) -> Vec<String> {
    vec![
        format!("{map}.rpak"),
        format!("{map}_client_perm.rpak"),
        format!("{map}_client_temp.rpak"),
        "common_mp.rpak".into(),
        "common.rpak".into(),
        "common_early.rpak".into(),
        "common_flowstate.rpak".into(),
    ]
}

/// The map's paks plus every enabled mod's (for heirloom and weapon mods in `--model`).
fn pak_set_with_mods(map: &str, cfg: &Config) -> Vec<String> {
    let mut v = pak_set(map);
    for p in assets::mod_paks(cfg) {
        if !v.contains(&p) {
            v.push(p);
        }
    }
    v
}

fn main() {
    // debug: `export-map --dump-model <x.rmdl> [lod]` prints what the r5ed path makes of a raw model (bounds,
    // first vertices, materials), `--dump-cast <x.cast>` the same for a Cast file: used to line the two up
    let av: Vec<String> = std::env::args().collect();
    if av.get(1).map(String::as_str) == Some("--dump-model") {
        let p = std::path::PathBuf::from(&av[2]);
        let (rmdl, vg) = (std::fs::read(&p).unwrap(), std::fs::read(p.with_extension("vg")).unwrap());
        let m = parse_lod("", &rmdl, &vg, av.get(3).and_then(|s| s.parse().ok()).unwrap_or(0)).unwrap();
        dump_model(&m);
        return;
    }
    if av.get(1).map(String::as_str) == Some("--guid") {
        // retail paks carry no names: the guid RSX lists for an asset path (StringToGuid)
        for p in &av[2..] {
            println!("0x{:016X}  {p}", assets::string_to_guid(&p.replace('\\', "/").to_ascii_lowercase()));
        }
        return;
    }
    if av.get(1).map(String::as_str) == Some("--dump-cast") {
        dump_model(&cast::read_model(&std::fs::read(&av[2]).unwrap()).unwrap());
        return;
    }
    let args = parse_args();
    let cfg = config();
    if args.clean {
        let m = cfg.work.join("mirror");
        let _ = std::fs::remove_dir_all(&m);
        eprintln!("removed {}", m.display());
        if args.models.is_empty() && std::env::args().count() == 2 {
            return;
        }
    }
    let result = if args.collision_only {
        export_collision_cmd(&args, cfg)
    } else if args.models.is_empty() {
        export_map(&args, cfg)
    } else {
        export_model_cmd(&args, cfg)
    };
    if let Err(e) = result {
        eprintln!("error: {e:#}");
        let code = if e.to_string().contains("character model") { 3 } else { 1 };
        std::process::exit(code);
    }
}

// ---------------------------------------------------------------- models ----

/// `--dump-model` / `--dump-cast` output: enough to line two readings of one model up.
fn dump_model(m: &rpak::rmdl::RmdlModel) {
    let (mut lo, mut hi) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));
    let mut tris = 0;
    for mesh in &m.meshes {
        tris += mesh.indices.len() / 3;
        for p in &mesh.positions {
            lo = lo.min(Vec3::from(*p));
            hi = hi.max(Vec3::from(*p));
        }
    }
    println!("name {:?} v{} meshes {} tris {tris} bones {} surface {:?}", m.name, m.version, m.meshes.len(), m.bone_count, m.surface_prop);
    println!("vertex box {:.2?} .. {:.2?}   header box {:.2?} .. {:.2?}", lo, hi, m.bounds_min, m.bounds_max);
    println!("materials {:x?}", m.textures);
    for (k, mesh) in m.meshes.iter().enumerate().take(3) {
        println!("mesh {k}: material {} verts {} tris {}", mesh.material, mesh.positions.len(), mesh.indices.len() / 3);
        for i in 0..3.min(mesh.positions.len()) {
            println!("  p {:.3?} n {:.3?} uv {:.3?}", mesh.positions[i], mesh.normals[i], mesh.uv0[i]);
        }
        println!("  first tri {:?}", &mesh.indices[..3.min(mesh.indices.len())]);
    }
}

/// Character / NPC bodies: the client draws its own stand-ins for those.
fn is_character_model(path: &str) -> bool {
    let p = path.to_ascii_lowercase().replace('\\', "/");
    p.starts_with("mdl/humans/") || p.contains("/humans/") || p.contains("/pilots/") || p.contains("dummie")
}

/// Editor markers that never draw in game.
fn is_dev_model(path: &str) -> bool {
    let p = path.to_ascii_lowercase().replace('\\', "/");
    p.starts_with("mdl/dev/") || p.contains("editor_ref")
}

/// `mdl/props/x/y.rmdl` -> `props/x/y.glb` (lower case, safe characters).
pub fn model_rel_path(path: &str) -> String {
    let p = path.to_ascii_lowercase().replace('\\', "/");
    let p = p.strip_prefix("mdl/").unwrap_or(&p);
    let p = p.strip_suffix(".rmdl").unwrap_or(p);
    let clean: String = p.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '/' | '.') { c } else { '_' }).collect();
    format!("{}.glb", clean.trim_matches('/').replace("..", "_"))
}

/// Exports models by asset path; returns each one's `.rmdl` and `.vg` bytes.
/// Missing ones are left out.
fn load_model_files(paks: &Paks, paths: &BTreeSet<String>, prefer: &[&str]) -> Result<BTreeMap<String, (Vec<u8>, Vec<u8>)>> {
    let dir = paks.cfg.work.join("models");
    // named (S21) or unnamed retail export (`mdl\0x<GUID>.rmdl`)
    let file_of = |p: &str| {
        let named = dir.join(p.replace('/', "\\"));
        if named.is_file() {
            return named;
        }
        let unnamed = dir.join("mdl").join(format!("0x{:X}.rmdl", assets::string_to_guid(p)));
        if unnamed.is_file() { unnamed } else { named }
    };
    let mut todo = Vec::new();
    for p in paths {
        if file_of(p).is_file() && file_of(p).with_extension("vg").is_file() {
            continue;
        }
        if let Some(r) = paks.find(p, "mdl_", prefer) {
            todo.push(r.guid);
        }
    }
    if !todo.is_empty() {
        paks.export("mdl_", &["-exportfullpaths"], &todo, &dir, prefer)?;
    }
    // Retail models (mdl v21): r5ed can't read their raw form, so the geometry comes from a Cast copy (one file per
    // LOD) and only the header (hull, skins) from the raw .rmdl. "--cast" tells run_rsx to leave RSX's default format.
    if std::env::var_os("R5_RSX_MODELS").is_some() {
        let cdir = paks.cfg.work.join("models_cast");
        let want: Vec<u64> = paths
            .iter()
            .filter(|p| !cdir.join(format!("{}_LOD0.cast", p.trim_end_matches(".rmdl").replace('/', "\\"))).is_file())
            .filter_map(|p| paks.find(p, "mdl_", prefer).map(|r| r.guid))
            .collect();
        if !want.is_empty() {
            paks.export("mdl_", &["-exportfullpaths", "--cast"], &want, &cdir, prefer)?;
        }
        let _ = CAST_DIR.set(cdir);
    }
    let mut out = BTreeMap::new();
    for p in paths {
        let f = file_of(p);
        if let (Ok(rmdl), Ok(vg)) = (std::fs::read(&f), std::fs::read(f.with_extension("vg"))) {
            out.insert(p.clone(), (rmdl, vg));
        }
    }
    Ok(out)
}

/// How many LOD vertex groups a model has.
fn lod_count(name: &str, rmdl: &[u8]) -> usize {
    if let Some(n) = cast_lods(name) {
        return n;
    }
    rmdl.get(182..184).map_or(1, |b| u16::from_le_bytes([b[0], b[1]]) as usize).max(1)
}

/// Parses LOD `lod` (clamped to the last one) through r5ed's LOD0 reader:
/// each LOD sits in its own vertex group of the `.vg`, so that group is cut
/// out and its LOD header relabelled as level 0, which is all the reader
/// looks for. Group table: studiohdr +180 (offset from that field) / +182
/// (count), 16-byte records, decompressed size at +8.
/// Where the Cast copies of retail models are (set when models go through R5_RSX_MODELS).
static CAST_DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

fn cast_file(name: &str, lod: usize) -> Option<PathBuf> {
    let dir = CAST_DIR.get()?;
    let stem = name.trim_end_matches(".rmdl").replace('/', "\\");
    let p = dir.join(format!("{stem}_LOD{lod}.cast"));
    p.is_file().then_some(p)
}

/// LOD count of a retail model: its Cast files `_LOD0.._LODn`.
fn cast_lods(name: &str) -> Option<usize> {
    cast_file(name, 0)?;
    Some((1..8).take_while(|&l| cast_file(name, l).is_some()).count() + 1)
}

/// A retail (mdl v21) model LOD from its Cast file, in the same space r5ed's raw reader gives for S21 models: Cast
/// holds the raw vertex space verbatim (checked on S21 charge_pylon_01_cells, barrel, firing_range_mu1_targets_01_stand:
/// identical positions, normals, uvs and materials); only the triangle winding is the other way. The header hull
/// (studiohdr_v19_2_t +68/+80, same offsets as v16) comes from the raw .rmdl, reordered (y, z, x) as r5ed stores it, so
/// fix_axes turns the same models.
fn parse_cast_lod(rmdl: &[u8], path: &std::path::Path) -> Result<rpak::rmdl::RmdlModel> {
    let mut m = cast::read_model(&std::fs::read(path)?)?;
    for mesh in &mut m.meshes {
        for t in mesh.indices.chunks_exact_mut(3) {
            t.swap(0, 2);
        }
    }
    if let Some(h) = rmdl.get(68..92) {
        let f = |i: usize| f32::from_le_bytes(h[i * 4..i * 4 + 4].try_into().unwrap());
        m.bounds_min = [f(1), f(2), f(0)];
        m.bounds_max = [f(4), f(5), f(3)];
    }
    // Material slots in the model's own texture table (numtextures u16 @144, textureindex @146, entries = u64 material
    // guid; same offsets in v19.2 as v16), so skin families (skin_remap: @148/150/152, also unchanged) index the right
    // slot. Cast lists materials in its own order. The Firing Range dome panels need it: skin 1 = the sky material.
    let u16_at = |at: usize| rmdl.get(at..at + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
    if let (Some(n), Some(off)) = (u16_at(144), u16_at(146)) {
        let base = ((off & 0xFFFE) as usize) << (4 * (off & 1) as usize);
        let table: Vec<u64> = (0..n as usize).filter_map(|i| rmdl.get(base + i * 8..base + i * 8 + 8).map(|b| u64::from_le_bytes(b.try_into().unwrap()))).collect();
        if !table.is_empty() && m.textures.iter().all(|g| table.contains(g)) {
            let cast_order = std::mem::replace(&mut m.textures, table);
            for mesh in &mut m.meshes {
                let g = cast_order[mesh.material as usize];
                mesh.material = m.textures.iter().position(|t| *t == g).unwrap() as u16;
            }
        }
    }
    Ok(fix_axes(m))
}

fn parse_lod(name: &str, rmdl: &[u8], vg: &[u8], lod: usize) -> Result<rpak::rmdl::RmdlModel> {
    if let Some(n) = cast_lods(name) {
        let p = cast_file(name, lod.min(n - 1)).unwrap();
        return parse_cast_lod(rmdl, &p);
    }
    // r5ed's rmdl reader can panic on a model it doesn't understand (seen: an out-of-bounds
    // read at rmdl.rs:115 in a batch of Firing Range loot). One bad model must not take the
    // whole batch down, so a panic becomes an error for that model only.
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| parse_lod_inner(rmdl, vg, lod))) {
        Ok(r) => r,
        Err(p) => {
            let msg = p.downcast_ref::<String>().cloned().or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default();
            Err(anyhow!("the rmdl reader panicked: {msg}"))
        }
    }
}

fn parse_lod_inner(rmdl: &[u8], vg: &[u8], lod: usize) -> Result<rpak::rmdl::RmdlModel> {
    let groups = lod_count("", rmdl);
    let lod = lod.min(groups - 1);
    if lod == 0 {
        return rpak::rmdl::parse_model(rmdl, vg).map(fix_axes).map_err(|e| anyhow!("{e}"));
    }
    let goff = u16::from_le_bytes([rmdl[180], rmdl[181]]) as usize;
    let size_of = |g: usize| -> usize {
        let at = 180 + goff + g * 16 + 8;
        rmdl.get(at..at + 4).map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize)
    };
    let base: usize = (0..lod).map(size_of).sum();
    let size = size_of(lod);
    let Some(slice) = vg.get(base..base + size) else { bail!("vertex group {lod} lies outside the .vg") };
    let mut g = slice.to_vec();
    if g.len() < 8 {
        bail!("vertex group {lod} is too short");
    }
    g[3] |= 1;
    let lods = g[1] as usize;
    let lod_off = u32::from_le_bytes([g[4], g[5], g[6], g[7]]) as usize;
    for j in 0..lods {
        let at = 4 + lod_off + j * 8 + 2;
        if at < g.len() {
            g[at] = if j == 0 { 0 } else { 0xFF };
        }
    }
    rpak::rmdl::parse_model(rmdl, &g).map(fix_axes).map_err(|e| anyhow!("{e}"))
}

/// Rigid props keep their vertices on the model's own axes (Z up), but many
/// skinned ones (jump pad, flags: several bones) store them on the rotated
/// axes r5ed's reader calls vertex space, where model (x, y, z) is vertex
/// (y, z, x). Which one a model uses is read off its bounds: the vertex box
/// matches either the header hull as stored or the hull turned into vertex
/// space. Turned models are rotated back so every glb is Z up.
fn fix_axes(mut m: rpak::rmdl::RmdlModel) -> rpak::rmdl::RmdlModel {
    let (mut lo, mut hi) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));
    for mesh in &m.meshes {
        for p in &mesh.positions {
            lo = lo.min(Vec3::from(*p));
            hi = hi.max(Vec3::from(*p));
        }
    }
    if lo.x > hi.x {
        return m;
    }
    let conv = (Vec3::from(m.bounds_min), Vec3::from(m.bounds_max));
    let raw = (
        Vec3::from(rpak::rmdl::vertex_to_studiohdr_space(m.bounds_min)),
        Vec3::from(rpak::rmdl::vertex_to_studiohdr_space(m.bounds_max)),
    );
    let err = |b: (Vec3, Vec3)| (b.0 - lo).abs().element_sum() + (b.1 - hi).abs().element_sum();
    let size = (hi - lo).element_sum().max(1.0);
    // Turned only when the turned hull fits clearly better (a cube-ish model
    // fits both and is left alone).
    if err(conv) + 0.05 * size < err(raw) {
        let to_model = |v: [f32; 3]| rpak::rmdl::vertex_to_studiohdr_space(v);
        for mesh in &mut m.meshes {
            for p in &mut mesh.positions {
                *p = to_model(*p);
            }
            for n in &mut mesh.normals {
                *n = to_model(*n);
            }
        }
        m.surface_prop.push_str("|axes-fixed");
    }
    m
}

/// Exports and parses models (LOD0) by asset path. Missing ones are left out.
fn load_models(paks: &Paks, paths: &BTreeSet<String>, prefer: &[&str]) -> Result<BTreeMap<String, rpak::rmdl::RmdlModel>> {
    let mut out = BTreeMap::new();
    for (p, (rmdl, vg)) in load_model_files(paks, paths, prefer)? {
        match parse_lod(&p, &rmdl, &vg, 0) {
            Ok(m) => {
                out.insert(p, m);
            }
            Err(e) => eprintln!("  model {p}: {e}"),
        }
    }
    Ok(out)
}

/// Fraction of triangles whose winding agrees with their vertex normals
/// counter-clockwise (glTF's front face).
fn ccw_agreement(pos: &[[f32; 3]], nrm: &[[f32; 3]], idx: &[u32]) -> (usize, usize) {
    let (mut agree, mut total) = (0, 0);
    for t in idx.chunks_exact(3) {
        let [a, b, c] = [t[0] as usize, t[1] as usize, t[2] as usize];
        let (pa, pb, pc) = (Vec3::from(pos[a]), Vec3::from(pos[b]), Vec3::from(pos[c]));
        let g = (pb - pa).cross(pc - pa);
        let n = Vec3::from(nrm[a]) + Vec3::from(nrm[b]) + Vec3::from(nrm[c]);
        let d = g.dot(n);
        if d.abs() > 1e-6 {
            total += 1;
            if d > 0.0 {
                agree += 1;
            }
        }
    }
    (agree, total)
}

// ------------------------------------------------------------ geometry ----

/// Geometry grouped by material key, one glTF primitive (= one draw) each.
#[derive(Default)]
struct Groups {
    prims: Vec<gltf::Prim>,
    keys: Vec<String>,
    index: HashMap<String, usize>,
}

impl Groups {
    fn get(&mut self, key: &str) -> &mut gltf::Prim {
        let i = match self.index.get(key) {
            Some(i) => *i,
            None => {
                self.prims.push(gltf::Prim { material: self.prims.len(), ..Default::default() });
                self.keys.push(key.to_string());
                self.index.insert(key.to_string(), self.prims.len() - 1);
                self.prims.len() - 1
            }
        };
        &mut self.prims[i]
    }
}

/// How a material key resolves for the glb.
struct MatPlan {
    name: String,
    info: Option<mats::MatInfo>,
    cap: u32,
    extras: Value,
}

fn color_key(info: &mats::MatInfo, g: u64, cap: u32) -> mats::ImgKey {
    if info.translucent {
        (g, Role::Blend, cap, info.opacity.unwrap_or(0))
    } else {
        // cut-outs whose alpha lives in a separate opacityMultiplyTexture / _msk (e.g. the habitat pine branches'
        // skin) get it multiplied in, like blended materials
        (g, Role::Color, cap, info.opacity.unwrap_or(0))
    }
}

fn normal_cap(cap: u32) -> u32 {
    (cap / 2).max(256)
}

/// Every image the plans need.
fn wanted_images(plans: &BTreeMap<String, MatPlan>, normals: bool) -> BTreeSet<mats::ImgKey> {
    let mut wanted = BTreeSet::new();
    for plan in plans.values() {
        if let Some(i) = &plan.info {
            if let Some(g) = i.albedo {
                wanted.insert(color_key(i, g, plan.cap));
            }
            if normals {
                if let Some(g) = i.normal {
                    wanted.insert((g, Role::Normal, normal_cap(plan.cap), 0));
                }
            }
            if let Some(g) = i.emissive {
                wanted.insert((g, Role::Color, normal_cap(plan.cap), 0));
            }
        }
    }
    wanted
}

/// Builds glTF materials + images for every group from already-built images,
/// dropping groups whose material never draws.
fn assemble(
    groups: Groups,
    plans: &BTreeMap<String, MatPlan>,
    built: &BTreeMap<mats::ImgKey, mats::Encoded>,
    normals: bool,
) -> (Vec<gltf::Prim>, Vec<gltf::Mat>, Vec<gltf::Image>) {
    let mut images: Vec<gltf::Image> = Vec::new();
    let mut image_of: HashMap<mats::ImgKey, usize> = HashMap::new();
    let mut add_image = |key: mats::ImgKey, images: &mut Vec<gltf::Image>| -> Option<(usize, bool)> {
        let e = built.get(&key)?;
        let i = *image_of.entry(key).or_insert_with(|| {
            images.push(gltf::Image { bytes: e.bytes.clone(), mime: e.mime, name: format!("0x{:016x}", key.0) });
            images.len() - 1
        });
        Some((i, e.cutout))
    };
    let mut prims = Vec::new();
    let mut materials = Vec::new();
    for (mut prim, key) in groups.prims.into_iter().zip(groups.keys) {
        let Some(plan) = plans.get(&key) else { continue };
        let mut m = gltf::Mat { name: plan.name.clone(), factor: [1.0, 1.0, 1.0, 1.0], extras: plan.extras.clone(), ..Default::default() };
        let mut blended = false;
        if let Some(info) = &plan.info {
            if let Some(g) = info.albedo {
                if let Some((i, cut)) = add_image(color_key(info, g, plan.cap), &mut images) {
                    m.base_color = Some(i);
                    if info.translucent {
                        // Decals, glass, blended sand: alpha blend, vertex
                        // alpha included (Respawn fades decal edges with it).
                        m.alpha_blend = true;
                        blended = true;
                    } else {
                        // `cut` = the albedo (x its _msk) has transparent texels, a guess at alpha test. Loot pickups
                        // break it: the shared attachment sheet (models/test/weapons_test/pickups_test/
                        // wep_test_pickups_proxy_sheet01_tier01) has a _msk that isn't opacity, so the guess cut most
                        // attachments away (outline, no item). Retail's real cut-outs (foliage, fences, grates) are
                        // double-sided (raster_flags 2); the loot sheets are single-sided (6). Loot never cuts here.
                        let loot = plan.name.starts_with("models/test/weapons_test/pickups_test/") || plan.name.starts_with("models/weapons_r5/loot/");
                        let cut = cut && !loot;
                        m.alpha_mask = cut;
                        m.double_sided = cut;
                    }
                }
            }
            if normals {
                if let Some(g) = info.normal {
                    m.normal = add_image((g, Role::Normal, normal_cap(plan.cap), 0), &mut images).map(|x| x.0);
                }
            }
            if let Some(g) = info.emissive {
                m.emissive = add_image((g, Role::Color, normal_cap(plan.cap), 0), &mut images).map(|x| x.0);
            }
        }
        // r5rust: keep vertex colours on every world surface. Apex's world shader tints albedo by
        // pow(vertexColor, 2.2) (the "_vc" trims are white textures coloured this way); only blend
        // materials read the alpha as a layer factor. r5-web-client's original dropped them here.
        let _ = blended;
        if m.base_color.is_none() {
            m.factor = [0.55, 0.55, 0.55, 1.0];
        }
        prim.material = materials.len();
        materials.push(m);
        prims.push(prim);
    }
    (prims, materials, images)
}

/// `assemble` with the images built first.
fn finish(
    paks: &Paks,
    groups: Groups,
    plans: &BTreeMap<String, MatPlan>,
    normals: bool,
    prefer: &[&str],
) -> Result<(Vec<gltf::Prim>, Vec<gltf::Mat>, Vec<gltf::Image>)> {
    let wanted = wanted_images(plans, normals);
    let t = Instant::now();
    let built = mats::build_images(paks, &wanted, prefer, None)?;
    eprintln!("  images: {}/{} built in {:.1}s", built.len(), wanted.len(), t.elapsed().as_secs_f32());
    Ok(assemble(groups, plans, &built, normals))
}

// ----------------------------------------------------------- the model ----

/// `--model <path>` (repeatable): each model to `<out>/<path>.glb`, one JSON
/// line per model on stdout. Several in one call share the pak loads (about
/// 30 s for the batch instead of per model). Exit 1 when a model is in no
/// loaded pak or fails to export, 3 when the only refusals are characters.
fn export_model_cmd(args: &Args, cfg: Config) -> Result<()> {
    let t0 = Instant::now();
    let out_root = args.out.clone().unwrap_or_else(|| mapcache_root().join("models"));
    let mut todo = BTreeSet::new();
    let (mut failed, mut refused) = (0, 0);
    for raw in &args.models {
        let path = raw.replace('\\', "/").to_ascii_lowercase();
        let path = if path.starts_with("mdl/") { path } else { format!("mdl/{path}") };
        let path = if path.ends_with(".rmdl") { path } else { format!("{path}.rmdl") };
        let out = model_out(&out_root, &path);
        if is_character_model(&path) {
            eprintln!("error: {path} is a character model; the client draws its own stand-in (skipped)");
            println!("{}", json!({"model": path, "error": "character model, skipped"}));
            refused += 1;
        } else if out.is_file() {
            println!("{}", json!({"model": path, "glb": out.display().to_string(), "cached": true}));
        } else {
            todo.insert(path);
        }
    }
    if !todo.is_empty() {
        let set = pak_set_with_mods(&args.map, &cfg);
    let paks = Paks::open(cfg, &set)?;
        let map_pak = format!("{}.rpak", args.map);
        let prefer = [map_pak.as_str(), "common_mp.rpak", "common.rpak", "common_early.rpak", "common_flowstate.rpak"];
        let (found, missing): (BTreeSet<String>, BTreeSet<String>) = todo.into_iter().partition(|p| paks.find(p, "mdl_", &prefer).is_some());
        for p in &missing {
            eprintln!("error: {p} is not in any loaded rpak ({})", paks.load.keys().cloned().collect::<Vec<_>>().join(", "));
            println!("{}", json!({"model": p, "error": "not in any loaded rpak"}));
            failed += 1;
        }
        if !found.is_empty() {
            for (path, r) in models_to_glbs(&paks, &found, args, &prefer)? {
                let out = model_out(&out_root, &path);
                match r {
                    Ok(glb) => {
                        std::fs::create_dir_all(out.parent().unwrap())?;
                        std::fs::write(&out, &glb)?;
                        println!("{}", json!({"model": path, "glb": out.display().to_string(), "bytes": glb.len(), "seconds": t0.elapsed().as_secs_f32()}));
                    }
                    Err(e) => {
                        eprintln!("error: {path}: {e:#}");
                        println!("{}", json!({"model": path, "error": format!("{e:#}")}));
                        failed += 1;
                    }
                }
            }
        }
    }
    if failed > 0 {
        std::process::exit(1);
    }
    if refused > 0 {
        std::process::exit(3);
    }
    Ok(())
}

/// Several models as glbs, sharing the pak loads: one model export, one
/// material export and one texture export per pak for the whole batch.
fn models_to_glbs(paks: &Paks, paths: &BTreeSet<String>, args: &Args, prefer: &[&str]) -> Result<BTreeMap<String, Result<Vec<u8>>>> {
    // One RSX run with every pak loaded exports the models, their materials
    // and every texture mip they use (-exportdependencies): one load of
    // common.rpak instead of three.
    let work = paks.cfg.work.clone();
    let dep = work.join(format!("deps-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dep);
    let models_dir = work.join("models");
    let need: Vec<u64> = paths
        .iter()
        .filter(|p| !models_dir.join(p.replace('/', "\\")).is_file())
        .filter_map(|p| paks.find(p, "mdl_", prefer).map(|r| r.guid))
        .collect();
    if !need.is_empty() {
        paks.export_together(
            "mdl_",
            &["-exportfullpaths", "-exportdependencies", "--exportsetting", "txtr=3", "--texturenames", "guid"],
            &need,
            &dep,
        )?;
        move_tree(&dep.join("mdl"), &models_dir.join("mdl"))?;
        move_tree(&dep.join("material"), &work.join("materials").join("material"))?;
    }
    let models = load_models(paks, paths, prefer)?;
    let mut per: Vec<(String, Groups)> = Vec::new();
    let mut mat_guids = BTreeSet::new();
    for (path, model) in &models {
        if std::env::var_os("EXPORT_MAP_STATS").is_some() {
            let (mut lo, mut hi) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));
            for m in &model.meshes {
                for p in &m.positions {
                    lo = lo.min(Vec3::from(*p));
                    hi = hi.max(Vec3::from(*p));
                }
            }
            eprintln!("  {path}: bones {} hull {:?}..{:?} verts {lo:?}..{hi:?} {}", model.bone_count, model.bounds_min, model.bounds_max, model.surface_prop);
        }
        let mut groups = Groups::default();
        add_model(&mut groups, model, Vec3::ZERO, Quat::IDENTITY, 1.0, &mut mat_guids);
        per.push((path.clone(), groups));
    }
    let infos = mats::resolve(paks, &mat_guids)?;
    let plans: Vec<BTreeMap<String, MatPlan>> = per.iter().map(|(_, g)| model_plans(g, &infos, paks, args.prop_cap.unwrap_or(args.world_cap))).collect();
    let mut wanted = BTreeSet::new();
    for p in &plans {
        wanted.extend(wanted_images(p, args.normals));
    }
    let pre = dep.join("texture");
    let built = mats::build_images(paks, &wanted, prefer, pre.is_dir().then_some(pre.as_path()))?;
    let _ = std::fs::remove_dir_all(&dep);
    let mut out = BTreeMap::new();
    for ((path, groups), plan) in per.into_iter().zip(plans) {
        let (prims, mats, images) = assemble(groups, &plan, &built, args.normals);
        out.insert(path.clone(), gltf::write(&prims, &mats, &images, json!({"model": path, "units": "source inches", "up": "+Z"})).map(|w| w.0));
    }
    for p in paths {
        out.entry(p.clone()).or_insert_with(|| Err(anyhow!("RSX could not export {p}")));
    }
    Ok(out)
}

/// Moves every file under `from` to the same relative path under `to`.
fn move_tree(from: &Path, to: &Path) -> Result<()> {
    let Ok(rd) = std::fs::read_dir(from) else { return Ok(()) };
    std::fs::create_dir_all(to)?;
    for e in rd.flatten() {
        let (src, dst) = (e.path(), to.join(e.file_name()));
        if src.is_dir() {
            move_tree(&src, &dst)?;
        } else {
            let _ = std::fs::remove_file(&dst);
            std::fs::rename(&src, &dst).or_else(|_| std::fs::copy(&src, &dst).map(|_| ()))?;
        }
    }
    Ok(())
}

/// Appends a model's LOD0 into `groups`, keyed by material guid, transformed
/// by `scale`, then `rot`, then `origin` (the rmdl vertex blocks are already on
/// the axes a map places them on).
fn add_model(groups: &mut Groups, model: &rpak::rmdl::RmdlModel, origin: Vec3, rot: Quat, scale: f32, mats_out: &mut BTreeSet<u64>) {
    add_model_flip(groups, model, origin, rot, scale, mats_out, model_needs_flip(model), None, "", None)
}

/// The studiohdr skin table: `numskinref` (+148), `numskinfamilies` (+150) and `skinindex` (+152,
/// a u16 header offset: the low bit means x16), then families x skinrefs i16 texture indices
/// (studio_r5_v16.h `studiohdr_t`). Family 0 is the identity; a static prop picks one with its
/// record's skin halfword.
fn skin_remap(rmdl: &[u8], skin: u16) -> Option<Vec<u16>> {
    if skin == 0 {
        return None;
    }
    let u16_at = |at: usize| rmdl.get(at..at + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
    let (nref, nfam, off) = (u16_at(148)? as usize, u16_at(150)? as usize, u16_at(152)?);
    let base = ((off & 0xFFFE) as usize) << (4 * (off & 1) as usize);
    if skin as usize >= nfam || nref == 0 {
        return None;
    }
    (0..nref).map(|i| u16_at(base + (skin as usize * nref + i) * 2)).collect()
}

/// Static prop record flag byte (`StaticProp::unknown_1`), Source's `STATIC_PROP_*` bits as Respawn
/// uses them. 0x10 = no shadow: none of this map's 1308 such props is in its own CSM caster tree
/// (lumps 0x63/0x64), against 61% of the rest. 0x40 = drawn only in the 3D skybox (the vista).
const PROP_NO_SHADOW: u8 = 0x10;
const PROP_SKYBOX: u8 = 0x40;
/// World mesh flag bit set on decal-class meshes (0x00100210 decals, 0x00100a10 blend, 0x00103210 water).
const MESH_NO_SHADOW: u32 = 0x10;

/// True when most of a model's triangles wind clockwise against their normals
/// (D3D front faces), which glTF draws as back faces.
fn model_needs_flip(model: &rpak::rmdl::RmdlModel) -> bool {
    let (mut a, mut t) = (0, 0);
    for mesh in &model.meshes {
        let idx: Vec<u32> = mesh.indices.iter().map(|i| *i as u32).collect();
        let (x, y) = ccw_agreement(&mesh.positions, &mesh.normals, &idx);
        a += x;
        t += y;
    }
    a * 2 < t
}

/// `skin`: a material-slot remap from `skin_remap`. `suffix` is appended to the group key
/// ("|ns" no shadow, "|sky" 3D skybox) so such geometry keeps its own primitives and material.
/// `lighting`: the instance's packed light probe + cubemap (see `gltf::Prim::lighting`), written on every vertex.
#[allow(clippy::too_many_arguments)]
fn add_model_flip(groups: &mut Groups, model: &rpak::rmdl::RmdlModel, origin: Vec3, rot: Quat, scale: f32, mats_out: &mut BTreeSet<u64>, flip: bool, skin: Option<&[u16]>, suffix: &str, lighting: Option<u32>) {
    for m in &model.meshes {
        let slot = skin.and_then(|s| s.get(m.material as usize)).map_or(m.material as usize, |t| *t as usize);
        let guid = model.textures.get(slot).or_else(|| model.textures.get(m.material as usize)).copied().unwrap_or(0);
        if m.indices.is_empty() {
            continue;
        }
        mats_out.insert(guid);
        let key = format!("m:{guid:016x}{suffix}");
        let p = groups.get(&key);
        let base = p.positions.len() as u32;
        for (i, v) in m.positions.iter().enumerate() {
            p.positions.push((origin + rot * (Vec3::from(*v) * scale)).to_array());
            p.normals.push((rot * Vec3::from(m.normals[i])).normalize_or_zero().to_array());
            p.uv0.push(m.uv0[i]);
            p.colors.push(m.colors.get(i).copied().unwrap_or([255; 4]));
            if let Some(l) = lighting {
                p.lighting.push(l);
            }
        }
        for t in m.indices.chunks_exact(3) {
            if flip {
                p.indices.extend_from_slice(&[base + t[0] as u32, base + t[2] as u32, base + t[1] as u32]);
            } else {
                p.indices.extend_from_slice(&[base + t[0] as u32, base + t[1] as u32, base + t[2] as u32]);
            }
        }
    }
}

fn model_plans(groups: &Groups, infos: &BTreeMap<u64, mats::MatInfo>, paks: &Paks, cap: u32) -> BTreeMap<String, MatPlan> {
    let mut plans = BTreeMap::new();
    for key in &groups.keys {
        let Some(rest) = key.strip_prefix("m:") else { continue };
        let (rest, flags) = split_key_flags(rest);
        let Ok(g) = u64::from_str_radix(rest, 16) else { continue };
        let info = infos.get(&g).cloned();
        if info.as_ref().is_some_and(|i| i.nodraw) {
            continue;
        }
        let name = info.as_ref().map(|i| i.path.clone()).or_else(|| paks.path_of(g).map(str::to_string)).unwrap_or_else(|| format!("0x{g:016x}"));
        if mats::is_hidden_material(&name) {
            continue;
        }
        if mats::is_sky_material(&name) {
            eprintln!("  sky material kept: {name}");
        }
        let mut extras = json!({"material": name, "shader": info.as_ref().map(|i| i.shader.clone()), "translucent": info.as_ref().map(|i| i.translucent), "sky": mats::is_sky_material(&name)});
        flags.apply(&mut extras);
        plans.insert(key.clone(), MatPlan { name, info, cap, extras });
    }
    plans
}

/// Placement flags a group key carries after the material ("|ns", "|sky").
#[derive(Clone, Copy, Default)]
struct KeyFlags {
    no_shadow: bool,
    skybox: bool,
    /// a runtime-LOD prop library mesh (model space, drawn instanced from props.json)
    lib: bool,
}

impl KeyFlags {
    fn suffix(self) -> &'static str {
        match (self.no_shadow, self.skybox) {
            (_, true) => "|sky",
            (true, false) => "|ns",
            _ => "",
        }
    }
    /// glb material extras: `noShadow` (never casts sun shadows), `skybox` (3D skybox only:
    /// drawn from the sky camera, never in the world pass, no shadows, no collision).
    fn apply(self, extras: &mut Value) {
        if self.no_shadow || self.skybox {
            extras["noShadow"] = json!(true);
        }
        if self.skybox {
            extras["skybox"] = json!(true);
        }
        if self.lib {
            extras["propLibrary"] = json!(true);
        }
    }
}

/// Strips the placement suffixes off a group key.
fn split_key_flags(key: &str) -> (&str, KeyFlags) {
    let mut f = KeyFlags::default();
    let mut k = key;
    loop {
        if let Some(r) = k.strip_suffix("|ns") {
            f.no_shadow = true;
            k = r;
        } else if let Some(r) = k.strip_suffix("|sky") {
            f.skybox = true;
            k = r;
        } else if let Some(r) = k.strip_suffix("|lib") {
            f.lib = true;
            k = r;
        } else {
            return (k, f);
        }
    }
}

// ------------------------------------------------------------- the map ----

/// Reads the client map RSX exported: `<stem>.bsp` plus one
/// `<stem>.bsp.XXXX.client.bsp_lump` per lump, and the `.ent` partitions.
fn read_bsp(dir: &Path, stem: &str) -> Result<(rbsp::Bsp, BTreeMap<String, Vec<u8>>)> {
    let path = dir.join(format!("{stem}.bsp"));
    let head = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    let mut header = rbsp::Header::parse(&head).map_err(|e| anyhow!("{}: {e}", path.display()))?;
    // Retail Apex (2026) writes BSP v54. Its records measure the same as v51/52 (meshes 28 B, material sorts 12 B,
    // probes 44 B, ...) and it adds lumps 0x19-0x1B and 0x4A, which r5ed doesn't read; so read it as v52.
    // Anything that decodes wrong under that assumption shows up in the export log / render diff.
    if header.version == 54 {
        eprintln!("  retail BSP v54: reading it with the v52 layouts (new lumps 0x19 0x1A 0x1B 0x4A ignored)");
        header.version = 52;
    }
    let mut lumps = BTreeMap::new();
    let mut names = BTreeMap::new();
    for e in std::fs::read_dir(dir)?.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(rest) = name.strip_prefix(&format!("{stem}.bsp.")) else { continue };
        let Some(hex) = rest.split('.').next() else { continue };
        if hex.len() != 4 || !rest.contains("bsp_lump") {
            continue;
        }
        let Ok(id) = u16::from_str_radix(hex, 16) else { continue };
        let data = std::fs::read(e.path())?;
        if data.is_empty() && header.lumps[id as usize].length > 0 {
            bail!("lump {id:#06x} exported empty: a starpak it streams from is missing");
        }
        lumps.insert(id, data);
        names.insert(id, name);
    }
    let bsp = rbsp::Bsp { header, lumps, layout: rbsp::Layout::Sidecar, sidecar_names: names, packed_size: 0 };
    let mut parts = BTreeMap::new();
    for p in rbsp::container::PARTITION_NAMES {
        if let Ok(b) = std::fs::read(dir.join(format!("{stem}_{p}.ent"))) {
            parts.insert(p.to_string(), b);
        }
    }
    Ok((bsp, parts))
}

/// The map's client BSP, extracted from its paks by RSX. `reuse` keeps the last extraction when
/// it is still on disk (the collision-only pass doesn't need a fresh one).
fn extract_bsp(paks: &Paks, stem: &str, reuse: bool) -> Result<(rbsp::Bsp, BTreeMap<String, Vec<u8>>)> {
    eprintln!("== bsp");
    let bsp_dir = paks.cfg.work.join("bsp").join(stem);
    let maps = bsp_dir.join("wrap").join("maps");
    if reuse && maps.join(format!("{stem}.bsp")).is_file() {
        eprintln!("  reusing {}", maps.display());
        return read_bsp(&maps, stem);
    }
    let _ = std::fs::remove_dir_all(&bsp_dir);
    let client: Vec<&PathBuf> = [format!("{stem}_client_perm.rpak"), format!("{stem}_client_temp.rpak")]
        .iter()
        .filter_map(|b| paks.load.get(b))
        .collect();
    if client.is_empty() {
        bail!("the install has no client map paks for {stem}");
    }
    let mut a: Vec<std::ffi::OsString> =
        vec!["-export".into(), "--exporttypes".into(), "wrap".into(), "--exportdir".into(), bsp_dir.clone().into()];
    a.extend(client.iter().map(|p| p.as_os_str().to_owned()));
    assets::run_rsx(&paks.cfg, &a, &bsp_dir)?;
    read_bsp(&maps, stem)
}

/// `--collision`: rewrite only `collision.bin` (and print what the decode found).
fn export_collision_cmd(args: &Args, cfg: Config) -> Result<()> {
    let t0 = Instant::now();
    let stem = args.map.clone();
    let out_dir = args.out.clone().unwrap_or_else(|| mapcache_root().join(&stem));
    eprintln!("== paks");
    let paks = Paks::open(cfg, &pak_set(&stem))?;
    let map_pak = format!("{stem}.rpak");
    let prefer = [map_pak.as_str(), "common_mp.rpak", "common.rpak", "common_early.rpak", "common_flowstate.rpak"];
    let (bsp, _) = extract_bsp(&paks, &stem, true)?;
    std::fs::create_dir_all(&out_dir)?;
    write_collision(&bsp, &paks, &prefer, &out_dir, true, args.prop_physics)?;
    eprintln!("done in {:.1}s", t0.elapsed().as_secs_f32());
    Ok(())
}

fn tree_of<'a>(bsp: &'a rbsp::Bsp, m: &rbsp::lumps::collision::Model, masks: &'a [u32]) -> coll::Tree<'a> {
    // LUMP_SURFACE_PROPERTIES (0x11): the world's dsurfaceproperty_t records
    coll::Tree {
        nodes: bsp.lump(rbsp::ids::BVH_NODES),
        node_base: m.bvh_node.max(0) as usize,
        leaf: bsp.lump(rbsp::ids::BVH_LEAF_DATA),
        leaf_base: m.bvh_leaf.max(0) as usize,
        verts: bsp.lump(rbsp::ids::VERTICES).get(m.vert_index.max(0) as usize * 12..).unwrap_or(&[]),
        origin: Vec3::from(m.origin),
        scale: m.decode_scale * 65536.0,
        masks,
        surf: bsp.lump(rbsp::ids::SURFACE_PROPERTIES),
    }
}

/// Decodes the map's collision (world tree + static prop models) into `<out>/collision.bin`.
fn write_collision(bsp: &rbsp::Bsp, paks: &Paks, prefer: &[&str], out_dir: &Path, verbose: bool, prop_physics: bool) -> Result<()> {
    use rbsp::lumps::collision as c;
    let t0 = Instant::now();
    let v = bsp.version_kind().ok_or_else(|| anyhow!("unknown map version"))?;
    // surfaceproperties id -> name / gamematerial, for props (their model's $surfaceprop) and the client (surfaces.json)
    let surf_table = surfaces::Table::load(&surfaces::rson_path());
    match &surf_table {
        Some(t) => {
            t.write_json(&out_dir.join("surfaces.json"))?;
            eprintln!("  surfaces.json: {} surface properties", t.by_id.len());
        }
        None => eprintln!("  no surfaceproperties.rson at {} (R5_SURFACEPROPS): props keep their collision's surface ids", surfaces::rson_path().display()),
    }
    let models = c::parse_models(bsp.lump(rbsp::ids::MODELS), v).map_err(|e| anyhow!("models: {e}"))?;
    let masks: Vec<u32> =
        c::parse_content_masks(bsp.lump(rbsp::ids::CONTENTS_MASKS), v).map_err(|e| anyhow!("masks: {e}"))?.iter().map(|m| m.mask).collect();
    let mut soup = coll::Soup::default();
    let mut src_tris = [0usize; 4];
    let mut mask_hist: BTreeMap<u32, usize> = BTreeMap::new();
    let mut outside = 0usize;

    // Brush models other than the world belong to entities (triggers, movers, func_brush):
    // reported, not baked into the static soup.
    if verbose {
        for (i, m) in models.iter().enumerate().skip(1) {
            if !(m.bvh_node >= 0 && m.mins[0] <= m.maxs[0]) {
                continue;
            }
            let d = tree_of(bsp, m, &masks).decode();
            let ms: BTreeSet<u32> = d.tris.iter().map(|t| t.1).collect();
            let ms: Vec<String> = ms.iter().map(|m| format!("{m:#x}")).collect();
            eprintln!("  model {i}: {} tris, {} prop leaves, masks {ms:?}", d.tris.len(), d.props.len());
        }
    }

    let m0 = models.first().copied().ok_or_else(|| anyhow!("the map has no models"))?;
    let world = tree_of(bsp, &m0, &masks).decode();
    eprintln!(
        "  world tree: {} nodes, leaves by kind {:?}, triangles by kind {:?}, {} prop leaves, {} bad refs",
        world.stats.nodes_visited,
        world.stats.leaves_by_kind,
        world.stats.tris_by_kind,
        world.props.len(),
        world.stats.bad_refs
    );
    if verbose {
        // Cross-check against r5ed's own decoder.
        match rcomp::collision::decode_leaves(bsp, 0) {
            Ok(l) => eprintln!("  r5ed decode_leaves(0): {} leaves, {} triangles", l.len(), l.iter().map(|x| x.triangles.len()).sum::<usize>()),
            Err(e) => eprintln!("  r5ed decode_leaves(0): {e}"),
        }
    }
    let (lo, hi) = (Vec3::from(m0.mins) - 1.0, Vec3::from(m0.maxs) + 1.0);
    for (t, mask, src, st) in &world.tris {
        // The vertex pool opens with a padding record a few entries name: drop what lies
        // outside the model's own box (r5ed does the same).
        if !t.iter().all(|p| p.is_finite() && p.cmpge(lo).all() && p.cmple(hi).all()) {
            outside += 1;
            continue;
        }
        let before = soup.idx.len();
        soup.push(*t, *mask, *src, *st);
        if soup.idx.len() > before {
            src_tris[*src as usize] += 1;
            *mask_hist.entry(*mask).or_default() += 1;
        }
    }

    // Static props named by prop leaves, each with its model's own collision tree.
    let sprp = match rbsp::lumps::parse_at(rbsp::ids::GAME_LUMP, v, bsp.lump(rbsp::ids::GAME_LUMP), 0) {
        Ok(rbsp::Lump::GameLump(gl)) => gl.static_props().cloned(),
        _ => None,
    };
    // debug: R5_PROPS_AT=x,y lists the static props within 2500u of that point (flags study)
    if let (Some(sp), Ok(at)) = (&sprp, std::env::var("R5_PROPS_AT")) {
        let c: Vec<f32> = at.split(',').filter_map(|v| v.parse().ok()).collect();
        for (i, p) in sp.props.iter().enumerate() {
            let (dx, dy) = (p.origin[0] - c[0], p.origin[1] - c[1]);
            if dx * dx + dy * dy < 2500.0 * 2500.0 {
                eprintln!("PROP {i:5} {:60} o ({:7.0},{:7.0},{:6.0}) s {:.2} solid {} flags {:#04x} u2 {} u3 {} u4 {:.1} u5 {:#x} u6 {:#x} u7 {:#x} dm {:#x} wind {:#x} sd {} u8 {:?}",
                    sp.model_of(i).unwrap_or("?"), p.origin[0], p.origin[1], p.origin[2], p.scale, p.unknown_0, p.unknown_1, p.unknown_2, p.unknown_3, p.unknown_4, p.unknown_5, p.unknown_6, p.unknown_7, p.diffuse_modulation, p.wind_data, p.setdress_level, p.unknown_8);
            }
        }
    }
    let (mut placed, mut no_file, mut no_bvh) = (0usize, 0usize, 0usize);
    if let Some(sp) = &sprp {
        let mut by_model: BTreeMap<String, Vec<coll::PropRef>> = BTreeMap::new();
        let mut leaf_masks: BTreeMap<u32, usize> = BTreeMap::new();
        for r in &world.props {
            *leaf_masks.entry(r.mask).or_default() += 1;
            if let Some(m) = sp.model_of(r.index as usize) {
                by_model.entry(m.replace('\\', "/").to_ascii_lowercase()).or_default().push(*r);
            }
        }
        let leaf_masks: Vec<String> = leaf_masks.iter().map(|(m, n)| format!("{m:#x} x{n}")).collect();
        eprintln!("  prop leaves name {} static props over {} models; leaf masks {leaf_masks:?}", world.props.len(), by_model.len());
        if verbose {
            // Props the BSP keeps away from player traces (no PLAYERCLIP/SOLID bit on their leaf).
            let mut ghost: Vec<(usize, usize, &String)> = by_model
                .iter()
                .map(|(n, r)| (r.iter().filter(|x| x.mask & coll::MASK_PLAYERSOLID == 0).count(), r.len(), n))
                .filter(|x| x.0 > 0)
                .collect();
            ghost.sort_by(|a, b| b.0.cmp(&a.0));
            let shown: Vec<String> = ghost.iter().take(14).map(|(g, n, m)| format!("{m} {g}/{n}")).collect();
            eprintln!("  props players pass through (leaf has no player bits), top models: {shown:?}");
        }
        let wanted: BTreeSet<String> = by_model.keys().cloned().collect();
        let files = load_model_files(paks, &wanted, prefer)?;
        let mut space_checks = 0usize;
        let mut costs: Vec<(usize, usize, usize, usize, String)> = Vec::new();
        let mut tree_counts: BTreeMap<String, usize> = BTreeMap::new();
        for (name, refs) in &by_model {
            let Some((rmdl, _)) = files.get(name) else {
                no_file += refs.len();
                if verbose {
                    eprintln!("    {name}: no model file ({} props)", refs.len());
                }
                continue;
            };
            // retail models (read through Cast) have the v19.2 header
            let v19 = cast_lods(name).is_some();
            let (main_tree, physics) = match coll::rmdl_collision(rmdl, v19) {
                Ok(Some(trees)) if trees.iter().any(|t| !t.tris.is_empty()) => {
                    let pi = coll::physics_tree_index(rmdl, v19);
                    *tree_counts.entry(format!("{} trees, physics index {pi:?}", trees.len())).or_default() += 1;
                    coll::split_trees(trees, pi)
                }
                Ok(_) => {
                    no_bvh += refs.len();
                    if verbose {
                        let ps = refs.iter().filter(|r| r.mask & coll::MASK_PLAYERSOLID != 0).count();
                        eprintln!("    {name}: no bvh collision ({} props, {ps} with a player-solid leaf)", refs.len());
                    }
                    continue;
                }
                Err(e) => {
                    no_bvh += refs.len();
                    eprintln!("    {name}: {e}");
                    continue;
                }
            };
            let main_tree = main_tree.unwrap_or_else(|| coll::Decoded { tris: Vec::new(), props: Vec::new(), stats: Default::default() });
            // R5_PROPCOLL=physics: collide players against the prop's physics tree where it has one (the coarse shell
            // the model header points at) instead of the detailed tree 0. Probe data suggests the game does this.
            let (main_tree, physics) = match (std::env::var("R5_PROPCOLL").as_deref(), physics) {
                (Ok("physics"), Some(p)) if !p.tris.is_empty() => (p, None),
                (_, p) => (main_tree, p),
            };
            let phys_n = physics.as_ref().map_or(0, |d| d.tris.len());
            // retail prop collision records do not decode as surface ids (all default, or values like "paintbomb"): the model's $surfaceprop decides
            let model_surf: Option<u8> = surf_table.as_ref().and_then(|t| {
                let n = surfaces::model_surfaceprop(rmdl)?;
                t.by_name.get(&n).copied()
            });
            if verbose && space_checks < 6 {
                // The trees should sit inside the studiohdr hull (the axes a map places it on).
                let (blo, bhi) =
                    main_tree.tris.iter().flat_map(|t| t.0).fold((Vec3::splat(f32::MAX), Vec3::splat(f32::MIN)), |(a, b), p| (a.min(p), b.max(p)));
                if let Some((hlo, hhi)) = coll::rmdl_hull(rmdl) {
                    eprintln!(
                        "    {name}: collision tree {} tris box {blo:.0}..{bhi:.0}, physics tree {phys_n} tris, studiohdr hull {hlo:.0}..{hhi:.0}",
                        main_tree.tris.len()
                    );
                }
                space_checks += 1;
            }
            costs.push((main_tree.tris.len() * refs.len(), main_tree.tris.len(), phys_n, refs.len(), name.clone()));
            let mut trees: Vec<(&coll::Decoded, u8)> = vec![(&main_tree, coll::SRC_PROP)];
            if prop_physics {
                if let Some(d) = &physics {
                    trees.push((d, coll::SRC_PROP_PHYSICS));
                }
            }
            for r in refs {
                let rec = sp.props[r.index as usize];
                let scale = if rec.scale > 0.0 { rec.scale } else { 1.0 };
                let (o, q) = (Vec3::from(rec.origin), source_rot(Vec3::from(rec.angles)));
                for (tree, src) in &trees {
                    for (t, mask, _, st) in &tree.tris {
                        let m = coll::prop_contents(r.mask, *mask);
                        let before = soup.idx.len();
                        let st = model_surf.unwrap_or(*st);
                        soup.push(coll::place(t, o, q, scale), m, *src, st);
                        if soup.idx.len() > before {
                            src_tris[*src as usize] += 1;
                            *mask_hist.entry(m).or_default() += 1;
                        }
                    }
                }
                placed += 1;
            }
        }
        eprintln!("  prop models by tree count: {tree_counts:?}");
        if verbose {
            costs.sort_by(|a, b| b.0.cmp(&a.0));
            eprintln!("  heaviest prop models (collision tris x copies):");
            for (total, tris, phys_n, n, name) in costs.iter().take(10) {
                eprintln!("    {total:>8}: {tris:>6} tris (physics {phys_n:>4}) x {n:>4}  {name}");
            }
        }
    }
    let (blo, bhi) = soup.bounds();
    let solid = soup.mask.iter().filter(|m| **m & coll::MASK_PLAYERSOLID != 0).count();
    let mut top: Vec<(&u32, &usize)> = mask_hist.iter().collect();
    top.sort_by(|a, b| b.1.cmp(a.1));
    let top: Vec<String> = top
        .iter()
        .take(16)
        .map(|(m, n)| format!("{m:#x}{} x{n}", if **m & coll::MASK_PLAYERSOLID != 0 { "" } else { " (not player-solid)" }))
        .collect();
    let bytes = soup.to_bytes();
    std::fs::write(out_dir.join("collision.bin"), &bytes)?;
    eprintln!(
        "  collision.bin: {} tris ({} world poly, {} world hull, {} static prop, {} prop physics tree), {} verts, {:.1} MB, {solid} player-solid; dropped {outside} outside the world box, {} degenerate",
        soup.idx.len(),
        src_tris[0],
        src_tris[1],
        src_tris[2],
        src_tris[3],
        soup.pos.len(),
        bytes.len() as f64 / 1e6,
        soup.degenerate
    );
    eprintln!(
        "  props: {placed} placed, {no_file} without a model file, {no_bvh} without bvh collision; bounds {blo:.0}..{bhi:.0}; {:.1}s",
        t0.elapsed().as_secs_f32()
    );
    eprintln!("  masks: {top:?}");
    Ok(())
}

fn kv_vec3(e: &rbsp::lumps::entities::Entity, k: &str) -> Option<Vec3> {
    let v: Vec<f32> = e.get(k)?.split_whitespace().filter_map(|s| s.parse().ok()).collect();
    (v.len() >= 3).then(|| Vec3::new(v[0], v[1], v[2]))
}

/// A retail (mdl v21) model's LOD switch points and fade distance, read off its header: lodOffset u16 @200 (from
/// that field, low bit = x16 like the other u16 offsets), lodCount u16 @202 (f32 switch points, Source's LOD metric
/// = 100 / pixel width of a unit sphere), fadeDistance f32 @204 (-1 = never). Same fields as RSX's studiohdr_v19_2_t
/// (lodOffset / lodCount / fadeDistance), 16 bytes later in v21 (checked on pipe_modular_painted_red_16: 4 LODs at
/// 0/35/75/120, fade 3174). None for other versions (only the Cast path reads retail models).
fn lod_info(rmdl: &[u8]) -> Option<(Vec<f32>, f32)> {
    let u16_at = |at: usize| rmdl.get(at..at + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
    let f32_at = |at: usize| rmdl.get(at..at + 4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    let (off, n) = (u16_at(200)?, u16_at(202)? as usize);
    if n == 0 || n > 8 {
        return None;
    }
    let base = 200 + (((off & 0xFFFE) as usize) << (4 * (off & 1) as usize));
    let thr: Option<Vec<f32>> = (0..n).map(|i| f32_at(base + i * 4)).collect();
    Some((thr?, f32_at(204)?))
}

/// A retail (mdl v21) model's hull box (hull_min @0x44, hull_max @0x50), STUDIOHDR_FLAGS_NO_FORCED_FADE (flags @0
/// & 0x800) and set-dress tier (byte @0xF0): what the S30 static-prop code (r5apex_dx12_s30 FUN_1403f7620 fade + size
/// cull, FUN_1403f89f0 hides a prop whose tier is above mat_set_dress_level) reads.
fn hull_info(rmdl: &[u8]) -> Option<([f32; 6], bool, u8)> {
    let f = |at: usize| rmdl.get(at..at + 4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    let flags = u32::from_le_bytes(rmdl.get(0..4)?.try_into().ok()?);
    Some(([f(0x44)?, f(0x48)?, f(0x4C)?, f(0x50)?, f(0x54)?, f(0x58)?], flags & 0x800 != 0, *rmdl.get(0xF0)?))
}

/// Appends a model in its own space to the library groups; returns (group key, first index, index count) per run.
fn add_model_lib(groups: &mut Groups, model: &rpak::rmdl::RmdlModel, flip: bool, skin: Option<&[u16]>, suffix: &str, mats_out: &mut BTreeSet<u64>) -> Vec<(String, u32, u32)> {
    let mut runs: Vec<(String, u32, u32)> = Vec::new();
    for m in &model.meshes {
        if m.indices.is_empty() {
            continue;
        }
        let slot = skin.and_then(|s| s.get(m.material as usize)).map_or(m.material as usize, |t| *t as usize);
        let guid = model.textures.get(slot).or_else(|| model.textures.get(m.material as usize)).copied().unwrap_or(0);
        mats_out.insert(guid);
        let key = format!("m:{guid:016x}{suffix}");
        let p = groups.get(&key);
        let base = p.positions.len() as u32;
        let first = p.indices.len() as u32;
        for (i, v) in m.positions.iter().enumerate() {
            p.positions.push(*v);
            p.normals.push(Vec3::from(m.normals[i]).normalize_or_zero().to_array());
            p.uv0.push(m.uv0[i]);
            p.colors.push(m.colors.get(i).copied().unwrap_or([255; 4]));
        }
        for t in m.indices.chunks_exact(3) {
            let t = [t[0] as u32, t[1] as u32, t[2] as u32];
            let t = if flip { [t[0], t[2], t[1]] } else { t };
            p.indices.extend(t.iter().map(|x| base + x));
        }
        let count = p.indices.len() as u32 - first;
        match runs.last_mut() {
            Some(r) if r.0 == key && r.1 + r.2 == first => r.2 += count,
            _ => runs.push((key, first, count)),
        }
    }
    runs
}

/// One prop library entry: a model with one skin and placement flags, every LOD.
struct LibVariant {
    model: String,
    skin: u16,
    no_shadow: bool,
    center: Vec3,
    radius: f32,
    fade: f32,
    /// model-header hull box + NO_FORCED_FADE (`hull_info`)
    hull: Option<([f32; 6], bool, u8)>,
    /// per LOD: switch point, runs
    lods: Vec<(f32, Vec<(String, u32, u32)>)>,
}

fn source_rot(angles: Vec3) -> Quat {
    // pitch, yaw, roll -> yaw about Z, then pitch about Y, then roll about X
    // (rdoc::Transform::from_source_angles does the same).
    Quat::from_euler(EulerRot::ZYX, angles.y.to_radians(), angles.x.to_radians(), angles.z.to_radians())
}

/// What a map's level script does to `skybox_cam_level` once entities load: (origin offset, new angles).
/// mp_rr_canyonlands_staging_mu1.nut: `SKYBOX_Z_OFFSET_STAGING_AREA = 32.0`, `SKYBOX_ANGLES_STAGING_AREA = <0, 60, 0>`.
fn sky_script_override(map: &str) -> Option<(Vec3, Vec3)> {
    match map {
        "mp_rr_canyonlands_staging_mu1" => Some((Vec3::new(0.0, 0.0, 32.0), Vec3::new(0.0, 60.0, 0.0))),
        _ => None,
    }
}

fn r1(v: f32) -> f64 {
    (v as f64 * 10.0).round() / 10.0
}

fn export_map(args: &Args, cfg: Config) -> Result<()> {
    let t0 = Instant::now();
    let stem = args.map.clone();
    let out_dir = args.out.clone().unwrap_or_else(|| mapcache_root().join(&stem));
    let models_dir = mapcache_root().join("models");
    eprintln!("== paks");
    let paks = Paks::open(cfg, &pak_set(&stem))?;
    let map_pak = format!("{stem}.rpak");
    let prefer = [map_pak.as_str(), "common_mp.rpak", "common.rpak", "common_early.rpak", "common_flowstate.rpak"];

    let (bsp, parts) = extract_bsp(&paks, &stem, false)?;
    let version = bsp.version_kind().ok_or_else(|| anyhow!("unknown map version {}", bsp.version()))?;
    eprintln!("  rBSP v{} ({version:?}), {} lumps, partitions {:?}", bsp.version(), bsp.lumps.len(), parts.keys().collect::<Vec<_>>());

    // ---- entities
    let mut ents: Vec<rbsp::lumps::entities::Entity> = Vec::new();
    if let Ok(t) = rbsp::lumps::entities::parse(bsp.lump(rbsp::ids::ENTITIES), version) {
        ents.extend(t.entities().iter().cloned());
    }
    for (name, data) in &parts {
        match rbsp::lumps::entities::parse(data, version) {
            Ok(t) => ents.extend(t.entities().iter().cloned()),
            Err(e) => eprintln!("  partition {name}: {e}"),
        }
    }
    let class = |e: &rbsp::lumps::entities::Entity| e.classname().unwrap_or("").to_string();
    let sky_ent = ents.iter().find(|e| class(e) == "sky_camera");
    let sky_camera = sky_ent.and_then(|e| kv_vec3(e, "origin"));
    // The 3D skybox (docs/skybox.md): the client draws it from sky_camera + R^T * eye / skyscale.
    // "skyscale" is m_skybox3d.scale (the S21 client divides by it, FUN_14099df90); "scale" is unused.
    let sky_scale = sky_ent.and_then(|e| e.get("skyscale")).and_then(|s| s.trim().parse::<f32>().ok()).unwrap_or(16.0);
    let mut sky_angles = sky_ent.and_then(|e| kv_vec3(e, "angles")).unwrap_or(Vec3::ZERO);
    let mut sky_camera = sky_camera;
    if let (Some(over), Some(c)) = (sky_script_override(&stem), sky_camera.as_mut()) {
        // Level scripts move the camera at EntitiesDidLoad; the BSP can't know.
        *c += over.0;
        sky_angles = over.1;
        eprintln!("  sky camera moved by the level script: {c:?} angles {sky_angles:?}");
    }
    // Where players actually go: spawns, range targets and the props scripts
    // use, plus a margin. Static props inside keep LOD0.
    let play_area: Option<(Vec3, Vec3)> = {
        let pts: Vec<Vec3> = ents
            .iter()
            .filter(|e| {
                let c = class(e);
                c.starts_with("info_spawnpoint_")
                    || c == "info_player_start"
                    || ((c.starts_with("prop_") || c == "script_mover") && e.get("model").is_some_and(|m| !is_dev_model(m)))
            })
            .filter_map(|e| kv_vec3(e, "origin"))
            .collect();
        pts.first().map(|f| {
            let (lo, hi) = pts.iter().fold((*f, *f), |(a, b), p| (a.min(*p), b.max(*p)));
            (lo - Vec3::splat(2048.0), hi + Vec3::splat(2048.0))
        })
    };
    if let Some((lo, hi)) = play_area {
        eprintln!("  play area: x {:.0}..{:.0}, y {:.0}..{:.0}", lo.x, hi.x, lo.y, hi.y);
    }

    // ---- world meshes
    eprintln!("== world");
    let runs = rcomp::meshes::decode_meshes(&bsp).map_err(|e| anyhow!("decoding render meshes: {e}"))?;
    let meshes = rbsp::lumps::render::parse_meshes(bsp.lump(rbsp::ids::MESHES), version).map_err(|e| anyhow!("{e}"))?;
    let sorts = rbsp::lumps::render::parse_material_sorts(bsp.lump(rbsp::ids::MATERIAL_SORT), version).map_err(|e| anyhow!("{e}"))?;
    let lm_headers = rbsp::lumps::lighting::parse_lightmap_headers(bsp.lump(rbsp::ids::LIGHTMAP_HEADERS), version).unwrap_or_default();
    let mut model_tris: BTreeMap<u32, usize> = BTreeMap::new();
    for r in &runs {
        *model_tris.entry(r.model).or_default() += r.indices.len() / 3;
    }
    eprintln!("  {} mesh runs, triangles per bsp model: {model_tris:?}", runs.len());

    // Which lightmap page each run samples, by its material sort.
    let page_of = |mi: usize| -> Option<usize> {
        let s = sorts.get(meshes.get(mi)?.material_sort as usize)?;
        (s.lightmap >= 0).then_some(s.lightmap as usize)
    };
    let lightmaps = if args.lightmaps { build_lightmap_atlas(&bsp, &lm_headers)? } else { None };

    // debug: R5_MESHFLAGS=1 prints the render mesh flag values (with an example material each), then stops
    if std::env::var_os("R5_MESHFLAGS").is_some() {
        let mut h: BTreeMap<u32, (usize, BTreeSet<String>)> = BTreeMap::new();
        for r in &runs {
            let e = h.entry(r.flags).or_default();
            e.0 += r.indices.len() / 3;
            if e.1.len() < 6 { e.1.insert(r.material.clone()); }
        }
        for (f, (n, m)) in &h { eprintln!("MESHFLAGS {f:#010x} tris {n:8} e.g. {m:?}"); }
        // R5_MESHFLAGS=<material substring> lists every run of that material
        let want = std::env::var("R5_MESHFLAGS").ok().filter(|v| v != "1").unwrap_or_else(|| "world_foundry_trims_01".into()).to_ascii_lowercase();
        for r in runs.iter().filter(|r| r.material.to_ascii_lowercase().contains(&want)) {
            eprintln!("TRIMS flags {:#010x} model {} bounds {:?}..{:?}", r.flags, r.model, r.bounds_min, r.bounds_max);
        }
        return Ok(());
    }
    // Brush models other than the world (*1, *2 ...) are stored in their entity's local space: the game draws them at
    // the entity's origin + angles (breachable_glass windows in the Firing Range's town). Exported raw they floated
    // around the map origin (Kainz, 2026-11-18). Ones without a drawing entity (triggers: tool materials) are skipped.
    let brush_xf: BTreeMap<u32, (glam::Mat3, Vec3)> = ents
        .iter()
        .filter_map(|e| {
            let n: u32 = e.get("model")?.strip_prefix('*')?.parse().ok()?;
            let o = kv_vec3(e, "origin").unwrap_or(Vec3::ZERO);
            let a = kv_vec3(e, "angles").unwrap_or(Vec3::ZERO);
            // Source angles (pitch, yaw, roll): R = Rz(yaw) Ry(pitch) Rx(roll)
            let r = glam::Mat3::from_rotation_z(a.y.to_radians()) * glam::Mat3::from_rotation_y(a.x.to_radians()) * glam::Mat3::from_rotation_x(a.z.to_radians());
            Some((n, (r, o)))
        })
        .collect();
    let mut skipped_brush = 0usize;
    let mut groups = Groups::default();
    let mut world_names: BTreeSet<String> = BTreeSet::new();
    let (mut skipped_tools, mut skipped_sky) = (0usize, 0usize);
    let (mut agree, mut total) = (0usize, 0usize);
    for r in &runs {
        let (a, t) = ccw_agreement(
            &r.positions.iter().map(|p| p.to_array()).collect::<Vec<_>>(),
            &r.normals.iter().map(|p| p.to_array()).collect::<Vec<_>>(),
            &r.indices,
        );
        agree += a;
        total += t;
    }
    // Respawn stores clockwise-front triangles; flip unless the data says
    // otherwise (measured, not assumed).
    let flip_world = agree * 2 < total;
    eprintln!("  winding: {agree}/{total} triangles counter-clockwise -> {}", if flip_world { "flipping" } else { "kept" });
    for (mi, r) in runs.iter().enumerate() {
        if std::env::var_os("EXPORT_MAP_DUMP_SAND").is_some() && r.material.contains("sand_desert_02") {
            let a: Vec<u8> = r.colors.iter().map(|c| c[3]).collect();
            let lo = a.iter().filter(|&&x| x < 64).count();
            eprintln!("  RUN {mi} {} vtype {} flags {:#x} verts {} alpha<64 {}", r.material, r.vertex_type, r.flags, a.len(), lo);
        }
        if mats::is_hidden_material(&r.material) {
            skipped_tools += r.indices.len() / 3;
            continue;
        }
        // The 3D skybox: geometry around the sky camera, far from play space.
        if let Some(sc) = sky_camera {
            let c = (r.bounds_min + r.bounds_max) * 0.5;
            if c.distance(sc) < 4096.0 && r.model == 0 {
                skipped_sky += r.indices.len() / 3;
                continue;
            }
        }
        let xf = if r.model == 0 {
            None
        } else {
            match brush_xf.get(&r.model) {
                Some(x) => Some(*x),
                None => {
                    skipped_brush += r.indices.len() / 3;
                    continue;
                }
            }
        };
        let name = r.material.replace('\\', "/").to_ascii_lowercase();
        world_names.insert(name.clone());
        let page = page_of(mi).filter(|_| lightmaps.is_some());
        // Mesh flag 0x10 marks decal-class meshes (decals, blended overlays, water): the map's own
        // CSM shadow meshes (lumps 0x7C-0x7F) leave them out, so they never cast sun shadows.
        let flags = KeyFlags { no_shadow: r.flags & MESH_NO_SHADOW != 0, skybox: false, lib: false };
        let key = match page {
            Some(_) => format!("w:{name}{}", flags.suffix()),
            None => format!("w:{name}|nolm{}", flags.suffix()),
        };
        let p = groups.get(&key);
        let base = p.positions.len() as u32;
        for i in 0..r.positions.len() {
            match xf {
                Some((rot, o)) => {
                    p.positions.push((rot * r.positions[i] + o).to_array());
                    p.normals.push((rot * r.normals[i]).to_array());
                }
                None => {
                    p.positions.push(r.positions[i].to_array());
                    p.normals.push(r.normals[i].to_array());
                }
            }
            p.uv0.push(r.uv0[i].to_array());
            p.colors.push(r.colors[i]);
            if let (Some(pg), Some(atlas)) = (page, &lightmaps) {
                p.uv1.push(atlas.remap(pg, r.uv1[i].to_array()));
            }
        }
        for t in r.indices.chunks_exact(3) {
            if t[0] == t[1] || t[1] == t[2] || t[0] == t[2] {
                continue;
            }
            if flip_world {
                p.indices.extend_from_slice(&[base + t[0], base + t[2], base + t[1]]);
            } else {
                p.indices.extend_from_slice(&[base + t[0], base + t[1], base + t[2]]);
            }
        }
    }
    eprintln!("  skipped {skipped_tools} tool/sky-material triangles, {skipped_sky} 3D-skybox triangles, {skipped_brush} entity-brush triangles without a drawing entity; {} brush models placed by their entity", brush_xf.len());

    // ---- static props
    eprintln!("== static props");
    let sprp = {
        let parsed = rbsp::lumps::parse_at(rbsp::ids::GAME_LUMP, version, bsp.lump(rbsp::ids::GAME_LUMP), 0).map_err(|e| anyhow!("game lump: {e}"))?;
        match parsed {
            rbsp::Lump::GameLump(gl) => gl.static_props().cloned(),
            _ => None,
        }
    };
    let mut prop_count = 0usize;
    let mut prop_skipped: BTreeMap<String, usize> = BTreeMap::new();
    let mut lib: Vec<LibVariant> = Vec::new();
    let mut instances: Vec<Value> = Vec::new();
    let mut prop_mat_guids = BTreeSet::new();
    // The skybox vistas: props drawn only from the sky camera. The client lights them with their own
    // light probe (LUMP_STATIC_PROP_LIGHTPROBE_INDEX 0x66, one u32 per prop) and env cubemap (record
    // halfword unknown_3), since a nearest-probe lookup out there finds nothing meaningful.
    let skybox_props: Vec<Value> = sprp.as_ref().map_or_else(Vec::new, |sp| {
        let probes = bsp.lump(0x66);
        (0..sp.prop_count().min(sp.props.len()))
            .filter(|&i| sp.props[i].unknown_1 & PROP_SKYBOX != 0)
            .map(|i| {
                let p = sp.props[i];
                let probe = probes.get(i * 4..i * 4 + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
                json!({"index": i, "model": sp.model_of(i), "origin": p.origin, "angles": p.angles, "probe": probe, "cubemap": p.unknown_3})
            })
            .collect()
    });
    if let Some(sp) = &sprp {
        let n = sp.prop_count().min(sp.props.len());
        let mut wanted: BTreeSet<String> = (0..n)
            .filter_map(|i| sp.model_of(i))
            .map(|m| m.replace('\\', "/").to_ascii_lowercase())
            .filter(|m| !is_dev_model(m) && !is_character_model(m))
            .collect();
        eprintln!("  {n} props over {} models", wanted.len());
        // R5_EXTRA_MODELS: a file of model paths (tools/build_loot.py models.txt: what loot looks like on the ground)
        // that go in the runtime library with no instances; the client places them (crates/client/src/loot.rs)
        let extra: Vec<String> = std::env::var_os("R5_EXTRA_MODELS")
            .and_then(|f| std::fs::read_to_string(f).ok())
            .map(|t| t.lines().map(|l| l.trim().replace('\\', "/").to_ascii_lowercase()).filter(|l| l.starts_with("mdl/")).collect())
            .unwrap_or_default();
        if !extra.is_empty() {
            eprintln!("  + {} extra library models (R5_EXTRA_MODELS)", extra.len());
            wanted.extend(extra.iter().cloned());
        }
        let files = load_model_files(&paks, &wanted, &prefer)?;
        // LOD0 for every prop by default, so detail is the same wherever you fly (r5rust has no runtime LOD switching).
        // `--far-lods`: the old distance rings, the game's own lower LODs for the backdrop outside the play area.
        let far_lods = args.far_lods;
        let lod_for = |o: Vec3| -> usize {
            if !far_lods {
                return 0;
            }
            let d = play_area.map_or(0.0, |(lo, hi): (Vec3, Vec3)| {
                let dx = (lo.x - o.x).max(o.x - hi.x).max(0.0);
                let dy = (lo.y - o.y).max(o.y - hi.y).max(0.0);
                (dx * dx + dy * dy).sqrt()
            });
            if d <= 0.0 {
                0
            } else if d < 3000.0 {
                1
            } else if d < 10000.0 {
                2
            } else {
                3
            }
        };
        // Triangle budget: every model starts at LOD0 inside the play area; the
        // model costing the most triangles drops one of the game's own LODs at a
        // time until the props fit. Hero pieces with few copies keep LOD0, the
        // clutter repeated hundreds of times goes first.
        let names: Vec<Option<String>> = (0..n).map(|i| sp.model_of(i).map(|m| m.replace('\\', "/").to_ascii_lowercase())).collect();
        let mut copies: HashMap<String, usize> = HashMap::new();
        for (i, nm) in names.iter().enumerate() {
            if let Some(nm) = nm {
                if files.contains_key(nm) && lod_for(Vec3::from(sp.props[i].origin)) == 0 {
                    *copies.entry(nm.clone()).or_default() += 1;
                }
            }
        }
        let mut lod_tris_of: HashMap<String, Vec<usize>> = HashMap::new();
        for nm in copies.keys().filter(|_| args.bake_props) {
            let (rmdl, vg) = &files[nm];
            let t: Vec<usize> = (0..lod_count(nm, rmdl))
                .map(|l| parse_lod(nm, rmdl, vg, l).map(|m| m.meshes.iter().map(|x| x.indices.len() / 3).sum()).unwrap_or(usize::MAX / 1024))
                .collect();
            lod_tris_of.insert(nm.clone(), t);
        }
        let mut base_lod: HashMap<String, usize> = copies.keys().map(|k| (k.clone(), 0)).collect();
        let cost = |k: &String, l: usize| copies[k] * lod_tris_of.get(k).map_or(0, |t| t[l]);
        let instanced = !args.bake_props;
        let mut total: usize = copies.keys().map(|k| cost(k, 0)).sum();
        let all_lod0 = total;
        while !instanced && total > args.prop_tri_budget {
            let pick = base_lod
                .iter()
                .filter(|(k, l)| **l + 1 < lod_tris_of[*k].len() && lod_tris_of[*k][**l + 1] < lod_tris_of[*k][**l])
                .max_by_key(|(k, l)| cost(k, **l))
                .map(|(k, _)| k.clone());
            let Some(k) = pick else { break };
            let l = base_lod[&k];
            total = total - cost(&k, l) + cost(&k, l + 1);
            base_lod.insert(k, l + 1);
        }
        let mut hist = [0usize; 6];
        for l in base_lod.values() {
            hist[(*l).min(5)] += 1;
        }
        eprintln!(
            "  {} props: {all_lod0} triangles at LOD0 -> {total} under the {} budget (models per base LOD: {hist:?}; runtime LOD: budget unused)",
            if far_lods { "play-area" } else { "all" },
            args.prop_tri_budget
        );
        let mut dropped: Vec<(&String, &usize)> = base_lod.iter().filter(|(_, l)| **l > 0).collect();
        dropped.sort();
        for (k, l) in dropped {
            eprintln!("    LOD{l} for all {} copies of {k} ({} -> {} triangles each)", copies[k], lod_tris_of[k][0], lod_tris_of[k][*l]);
        }
        let mut parsed: HashMap<(String, usize), Option<(rpak::rmdl::RmdlModel, bool)>> = HashMap::new();
        let mut lod_tris = [0usize; 8];
        let mut skinned = 0usize;
        let mut flag_hist: BTreeMap<&str, usize> = BTreeMap::new();
        let prop_probes = bsp.lump(0x66);
        let mut no_probe = 0usize;
        let (mut lib_index, mut lib_tris): (HashMap<(String, u16, bool), usize>, usize) = (HashMap::new(), 0);
        for i in 0..n {
            let rec = sp.props[i];
            let Some(name) = sp.model_of(i).map(|m| m.replace('\\', "/").to_ascii_lowercase()) else { continue };
            let Some((rmdl, vg)) = files.get(&name) else {
                *prop_skipped.entry(name).or_default() += 1;
                continue;
            };
            let pflags = KeyFlags { no_shadow: rec.unknown_1 & PROP_NO_SHADOW != 0, skybox: rec.unknown_1 & PROP_SKYBOX != 0, lib: false };
            let lighting = prop_probes.get(i * 4..i * 4 + 4).map(|b| (u32::from_le_bytes([b[0], b[1], b[2], b[3]]) & 0xFF_FFFF) | ((rec.unknown_3 as u32 & 0xFF) << 24));
            // Runtime LOD: every prop but the 3D skybox vistas goes in the model library once per (model, skin,
            // shadow flag) with all its LODs, plus an instance record (props.json).
            if instanced && !pflags.skybox {
                let vkey = (name.clone(), rec.unknown_2, pflags.no_shadow);
                let vi = match lib_index.get(&vkey) {
                    Some(&v) => Some(v),
                    None => {
                        let n_lods = lod_count(&name, rmdl);
                        let (thr, fade) = if cast_lods(&name).is_some() { lod_info(rmdl).unwrap_or((vec![0.0], -1.0)) } else { (vec![0.0], -1.0) };
                        let skin = skin_remap(rmdl, rec.unknown_2);
                        let suffix = format!("{}|lib", pflags.suffix());
                        let mut lods = Vec::new();
                        let (mut lo, mut hi) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));
                        for l in 0..n_lods.min(thr.len().max(1)) {
                            let entry = parsed.entry((name.clone(), l)).or_insert_with(|| match parse_lod(&name, rmdl, vg, l) {
                                Ok(m) => {
                                    let f = model_needs_flip(&m);
                                    Some((m, f))
                                }
                                Err(e) => {
                                    eprintln!("  model {name} lod {l}: {e}");
                                    None
                                }
                            });
                            let Some((model, flip)) = entry.as_ref() else { break };
                            if l == 0 {
                                for m in &model.meshes {
                                    for p in &m.positions {
                                        lo = lo.min(Vec3::from(*p));
                                        hi = hi.max(Vec3::from(*p));
                                    }
                                }
                            }
                            // a negative switch point is a shadow-only LOD: not drawn in the view
                            let sw = thr.get(l).copied().unwrap_or(0.0);
                            if sw < 0.0 {
                                continue;
                            }
                            let runs = add_model_lib(&mut groups, model, *flip, skin.as_deref(), &suffix, &mut prop_mat_guids);
                            lib_tris += runs.iter().map(|r| r.2 as usize / 3).sum::<usize>();
                            lods.push((sw, runs));
                        }
                        if lods.is_empty() || lo.x > hi.x {
                            None
                        } else {
                            let center = (lo + hi) * 0.5;
                            lib.push(LibVariant { model: name.clone(), skin: rec.unknown_2, no_shadow: pflags.no_shadow, center, radius: (hi - lo).length() * 0.5, fade, hull: hull_info(rmdl), lods });
                            lib_index.insert(vkey, lib.len() - 1);
                            Some(lib.len() - 1)
                        }
                    }
                };
                let Some(vi) = vi else {
                    *prop_skipped.entry(name).or_default() += 1;
                    continue;
                };
                let q = source_rot(Vec3::from(rec.angles));
                let scale = if rec.scale > 0.0 { rec.scale } else { 1.0 };
                if lighting.is_none() {
                    no_probe += 1;
                }
                *flag_hist.entry(pflags.suffix()).or_default() += 1;
                instances.push(json!([vi, rec.origin[0], rec.origin[1], rec.origin[2], q.x, q.y, q.z, q.w, scale, lighting.map_or(-1i64, |l| l as i64), rec.unknown_4]));
                prop_count += 1;
                continue;
            }
            // A skybox vista is seen from the sky camera a few units away: always full detail.
            let ring = if pflags.skybox { 0 } else { lod_for(Vec3::from(rec.origin)) };
            let lod = if pflags.skybox { 0 } else { (ring + base_lod.get(&name).copied().unwrap_or(0)).min(lod_count(&name, rmdl) - 1) };
            let entry = parsed.entry((name.clone(), lod)).or_insert_with(|| match parse_lod(&name, rmdl, vg, lod) {
                Ok(m) => {
                    let f = model_needs_flip(&m);
                    Some((m, f))
                }
                Err(e) => {
                    eprintln!("  model {name} lod {lod}: {e}");
                    None
                }
            });
            let Some((model, flip)) = entry.as_ref() else {
                *prop_skipped.entry(name).or_default() += 1;
                continue;
            };
            let scale = if rec.scale > 0.0 { rec.scale } else { 1.0 };
            lod_tris[lod.min(7)] += model.meshes.iter().map(|m| m.indices.len() / 3).sum::<usize>();
            let skin = skin_remap(rmdl, rec.unknown_2);
            if skin.is_some() {
                skinned += 1;
            }
            *flag_hist.entry(pflags.suffix()).or_default() += 1;
            // The game lights a static prop from its own baked probe: LUMP_STATIC_PROP_LIGHTPROBE_INDEX (0x66) holds
            // one probe index per prop record (probes 0..N of lump 0x65 are these per-prop probes, baked at the
            // prop's (lighting) origin; the k-d tree in 0x67/0x68 only references the rest). Its env cubemap is
            // record halfword 3. docs/apex-lighting.md section 6.
            if lighting.is_none() {
                no_probe += 1;
            }
            add_model_flip(&mut groups, model, Vec3::from(rec.origin), source_rot(Vec3::from(rec.angles)), scale, &mut prop_mat_guids, *flip, skin.as_deref(), pflags.suffix(), lighting);
            prop_count += 1;
        }
        // the extra models: one library variant each (skin 0, casts shadows), every LOD, no instances
        let mut extra_ok = 0usize;
        for name in extra.iter().filter(|_| instanced) {
            let vkey = (name.clone(), 0u16, false);
            if lib_index.contains_key(&vkey) {
                extra_ok += 1;
                continue;
            }
            let Some((rmdl, vg)) = files.get(name) else {
                eprintln!("  extra model {name}: not found in the loaded paks");
                continue;
            };
            let n_lods = lod_count(name, rmdl);
            let (thr, fade) = if cast_lods(name).is_some() { lod_info(rmdl).unwrap_or((vec![0.0], -1.0)) } else { (vec![0.0], -1.0) };
            let suffix = "|lib".to_string();
            let mut lods = Vec::new();
            let (mut lo, mut hi) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));
            for l in 0..n_lods.min(thr.len().max(1)) {
                let entry = parsed.entry((name.clone(), l)).or_insert_with(|| match parse_lod(name, rmdl, vg, l) {
                    Ok(m) => {
                        let f = model_needs_flip(&m);
                        Some((m, f))
                    }
                    Err(e) => {
                        eprintln!("  extra model {name} lod {l}: {e}");
                        None
                    }
                });
                let Some((model, flip)) = entry.as_ref() else { break };
                if l == 0 {
                    for m in &model.meshes {
                        for p in &m.positions {
                            lo = lo.min(Vec3::from(*p));
                            hi = hi.max(Vec3::from(*p));
                        }
                    }
                }
                let sw = thr.get(l).copied().unwrap_or(0.0);
                if sw < 0.0 {
                    continue;
                }
                let runs = add_model_lib(&mut groups, model, *flip, None, &suffix, &mut prop_mat_guids);
                lib_tris += runs.iter().map(|r| r.2 as usize / 3).sum::<usize>();
                lods.push((sw, runs));
            }
            if lods.is_empty() || lo.x > hi.x {
                continue;
            }
            let center = (lo + hi) * 0.5;
            lib.push(LibVariant { model: name.clone(), skin: 0, no_shadow: false, center, radius: (hi - lo).length() * 0.5, fade, hull: hull_info(rmdl), lods });
            lib_index.insert(vkey, lib.len() - 1);
            extra_ok += 1;
        }
        if !extra.is_empty() {
            eprintln!("  extra library models: {extra_ok} of {}", extra.len());
        }
        eprintln!("  meshes outside the default bodygroup, left out: {}", cast::SKIPPED_BODYGROUP_MESHES.load(std::sync::atomic::Ordering::Relaxed));
    eprintln!("  prop triangles by LOD: {:?}", &lod_tris[..4]);
        if instanced {
            eprintln!("  runtime LOD: {} instances of {} library variants, {lib_tris} library triangles (all LODs)", instances.len(), lib.len());
        }
        eprintln!("  per-prop light probes (lump 0x66): {} entries, {no_probe} props without one", prop_probes.len() / 4);
        eprintln!("  props by placement (''=normal, |ns=no shadow, |sky=3D skybox): {flag_hist:?}; {skinned} drawn with a non-default skin");
        let fixed: BTreeSet<&str> =
            parsed.iter().filter(|(_, v)| v.as_ref().is_some_and(|(m, _)| m.surface_prop.ends_with("|axes-fixed"))).map(|(k, _)| k.0.as_str()).collect();
        eprintln!("  {} static models stored on turned axes (rotated back): {:?}", fixed.len(), fixed.iter().take(12).collect::<Vec<_>>());
        let missing: usize = prop_skipped.values().sum();
        eprintln!("  placed {prop_count} props; {missing} not placed ({} models: dev markers, characters or not exportable)", prop_skipped.len());
    }

    // ---- materials
    eprintln!("== materials");
    let mut world_guid: BTreeMap<String, u64> = BTreeMap::new();
    for n in &world_names {
        let row = ["wldc", "wldu", "rgdp", "rgdu", "rgdc"]
            .iter()
            .find_map(|sfx| paks.find(&format!("material/{n}_{sfx}.rpak"), "matl", &prefer));
        match row {
            Some(r) => {
                world_guid.insert(n.clone(), r.guid);
            }
            None => eprintln!("  world material {n}: not in the loaded paks"),
        }
    }
    let mut all_guids: BTreeSet<u64> = world_guid.values().copied().collect();
    all_guids.extend(prop_mat_guids.iter().copied());
    let infos = mats::resolve(&paks, &all_guids)?;
    let mut plans = model_plans(&groups, &infos, &paks, args.prop_cap.unwrap_or(args.world_cap));
    let lm_index = lightmaps.as_ref().map(|_| ());
    for key in &groups.keys {
        let Some(rest) = key.strip_prefix("w:") else { continue };
        let (rest, kflags) = split_key_flags(rest);
        let (name, lit) = match rest.strip_suffix("|nolm") {
            Some(n) => (n, false),
            None => (rest, true),
        };
        let info = world_guid.get(name).and_then(|g| infos.get(g)).cloned();
        if info.as_ref().is_some_and(|i| i.nodraw) {
            continue;
        }
        if mats::is_sky_material(name) {
            eprintln!("  sky material kept (world): {name}");
        }
        let mut extras = json!({"material": name, "world": true, "lightmapped": lit && lm_index.is_some(),
            "shader": info.as_ref().map(|i| i.shader.clone()), "translucent": info.as_ref().map(|i| i.translucent), "sky": mats::is_sky_material(name)});
        kflags.apply(&mut extras);
        plans.insert(key.clone(), MatPlan { name: name.to_string(), info, cap: args.world_cap, extras });
    }

    eprintln!("== textures");
    // final glTF material index of each group key: assemble keeps the keys that have a plan, in order
    let mat_of_key: HashMap<String, usize> = groups.keys.iter().filter(|k| plans.contains_key(*k)).enumerate().map(|(i, k)| (k.clone(), i)).collect();
    let (prims, mut materials, mut images) = finish(&paks, groups, &plans, args.normals, &prefer)?;
    let props_json = (!lib.is_empty()).then(|| {
        let variants: Vec<Value> = lib
            .iter()
            .map(|v| {
                let lods: Vec<Value> = v
                    .lods
                    .iter()
                    .map(|(sw, runs)| json!({"switch": sw, "draws": runs.iter().filter_map(|(k, f, c)| mat_of_key.get(k).map(|m| json!([m, f, c]))).collect::<Vec<_>>()}))
                    .collect();
                let mut o = json!({"model": v.model, "skin": v.skin, "noShadow": v.no_shadow, "center": [v.center.x, v.center.y, v.center.z], "radius": v.radius, "fade": v.fade, "lods": lods});
                if let Some((hull, no_fade, tier)) = v.hull {
                    o["hull"] = json!(hull);
                    o["noForcedFade"] = json!(no_fade);
                    o["dressTier"] = json!(tier);
                }
                o
            })
            .collect();
        json!({
            "version": 1,
            "note": "r5rust runtime LOD. draws = [glb material, first index, index count] inside that material's single primitive (model space). instances = [variant, x, y, z, qx, qy, qz, qw, scale, lighting (probe | cubemap << 24, -1 none), fade distance (-1 = the model's)]. switch = Source LOD metric (100 / pixel width of a unit sphere).",
            "variants": variants,
            "instances": instances,
        })
    });
    if let Some(atlas) = &lightmaps {
        let (bytes, mime) = atlas.encode()?;
        images.push(gltf::Image { bytes, mime, name: "lightmap".into() });
        let li = images.len() - 1;
        for m in materials.iter_mut() {
            if m.extras.get("lightmapped").and_then(Value::as_bool) == Some(true) {
                m.lightmap = Some(li);
            }
        }
    }

    // ---- bounds + stats
    let (mut lo, mut hi) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));
    let mut tris = 0usize;
    for p in &prims {
        tris += p.triangles();
        for v in &p.positions {
            lo = lo.min(Vec3::from(*v));
            hi = hi.max(Vec3::from(*v));
        }
    }

    // ---- entities -> meta
    let mut spawns = Vec::new();
    let mut targets = Vec::new();
    let mut dynamic = Vec::new();
    let mut dyn_models: BTreeSet<String> = BTreeSet::new();
    let mut sun = Value::Null;
    for e in &ents {
        let c = class(e);
        let origin = kv_vec3(e, "origin").unwrap_or(Vec3::ZERO);
        let angles = kv_vec3(e, "angles").unwrap_or(Vec3::ZERO);
        if c == "info_player_start" || (c.starts_with("info_spawnpoint_") && !c.ends_with("_target")) {
            spawns.push(json!([r1(origin.x), r1(origin.y), r1(origin.z), r1(angles.y)]));
            continue;
        }
        if c.starts_with("info_spawnpoint_") {
            targets.push(json!([r1(origin.x), r1(origin.y), r1(origin.z), r1(angles.y)]));
            continue;
        }
        if c == "light_environment" {
            // Source: the "pitch" key is the light's downward angle as a
            // negative number; "angles" carries the same pitch positive.
            let pitch = e.get("pitch").and_then(|p| p.trim().parse::<f32>().ok()).map(|p| -p).unwrap_or(angles.x);
            let (p, y) = (pitch.to_radians(), angles.y.to_radians());
            let dir = Vec3::new(p.cos() * y.cos(), p.cos() * y.sin(), -p.sin());
            let l: Vec<f32> = e.get("_light").unwrap_or("255 255 255 200").split_whitespace().filter_map(|s| s.parse().ok()).collect();
            let amb: Vec<f32> = e.get("_ambient").unwrap_or("").split_whitespace().filter_map(|s| s.parse().ok()).collect();
            sun = json!({
                "dir": [dir.x, dir.y, dir.z],
                "dirMeaning": "direction the sunlight travels (sun -> ground), Source axes",
                "color": [l.first().copied().unwrap_or(255.0) / 255.0, l.get(1).copied().unwrap_or(255.0) / 255.0, l.get(2).copied().unwrap_or(255.0) / 255.0],
                "brightness": l.get(3).copied(),
                "ambient": if amb.len() >= 3 { json!([amb[0] / 255.0, amb[1] / 255.0, amb[2] / 255.0]) } else { Value::Null },
                "ambientBrightness": amb.get(3).copied(),
            });
            continue;
        }
        let Some(model) = e.get("model").filter(|m| m.ends_with(".rmdl")) else { continue };
        if c.starts_with("info_") {
            continue;
        }
        let model = model.replace('\\', "/").to_ascii_lowercase();
        let skip = if is_character_model(&model) {
            Some("character")
        } else if is_dev_model(&model) {
            Some("editor marker")
        } else {
            None
        };
        if skip == Some("editor marker") && !c.starts_with("prop_") {
            // ambient_generic / traverse / soundscape nodes: editor helpers, never drawn.
            continue;
        }
        if skip.is_none() {
            dyn_models.insert(model.clone());
        }
        dynamic.push(json!({
            "classname": c,
            "model": model,
            "origin": [r1(origin.x), r1(origin.y), r1(origin.z)],
            "angles": [r1(angles.x), r1(angles.y), r1(angles.z)],
            "scale": e.get("scale").and_then(|s| s.parse::<f32>().ok()).unwrap_or(1.0),
            "targetname": e.get("targetname"),
            "editorclass": e.get("editorclass"),
            "glb": if skip.is_none() { Value::from(format!("models/{}", model_rel_path(&model))) } else { Value::Null },
            "skipped": skip,
        }));
    }

    // ---- dynamic prop models
    eprintln!("== dynamic prop models ({})", dyn_models.len());
    let mut dyn_report = Vec::new();
    let mut batch = BTreeSet::new();
    for m in &dyn_models {
        if model_out(&models_dir, m).is_file() {
            dyn_report.push(json!({"model": m, "cached": true}));
        } else if paks.find(m, "mdl_", &prefer).is_none() {
            eprintln!("  {m}: not in any loaded rpak");
            dyn_report.push(json!({"model": m, "error": "not in any loaded rpak"}));
        } else {
            batch.insert(m.clone());
        }
    }
    if !batch.is_empty() {
        let t = Instant::now();
        let built = models_to_glbs(&paks, &batch, args, &prefer)?;
        let secs = t.elapsed().as_secs_f32();
        for (m, r) in built {
            match r {
                Ok(glb) => {
                    let out = model_out(&models_dir, &m);
                    std::fs::create_dir_all(out.parent().unwrap())?;
                    std::fs::write(&out, &glb)?;
                    eprintln!("  {m}: {} KB", glb.len() / 1024);
                    dyn_report.push(json!({"model": m, "bytes": glb.len()}));
                }
                Err(e) => {
                    eprintln!("  {m}: {e}");
                    dyn_report.push(json!({"model": m, "error": e.to_string()}));
                }
            }
        }
        eprintln!("  {} models in {secs:.1}s (one batch)", batch.len());
    }

    // ---- write
    eprintln!("== write");
    std::fs::create_dir_all(&out_dir)?;
    // Keep the previous export one generation back (world.prev.glb, meta.prev.json, collision.prev.bin).
    for (cur, prev) in [("world.glb", "world.prev.glb"), ("meta.json", "meta.prev.json"), ("collision.bin", "collision.prev.bin"), ("props.json", "props.prev.json")] {
        if out_dir.join(cur).is_file() {
            let _ = std::fs::remove_file(out_dir.join(prev));
            std::fs::rename(out_dir.join(cur), out_dir.join(prev)).with_context(|| format!("backing up {cur}"))?;
            eprintln!("  previous {cur} kept as {prev}");
        }
    }
    let (glb, draw_calls) = gltf::write(
        &prims,
        &materials,
        &images,
        json!({"map": stem, "units": "source inches", "up": "+Z", "note": "coordinates are the game's, untouched"}),
    )?;
    std::fs::write(out_dir.join("world.glb"), &glb)?;
    match &props_json {
        Some(pj) => std::fs::write(out_dir.join("props.json"), serde_json::to_vec(pj)?)?,
        // a baked export must not leave an older props.json next to it (the client would draw the props twice)
        None => {
            let _ = std::fs::remove_file(out_dir.join("props.json"));
        }
    }
    let meta = json!({
        "map": stem,
        "bounds": [lo.x, lo.y, lo.z, hi.x, hi.y, hi.z],
        "playerSpawns": spawns,
        "combatTargets": targets,
        "sun": sun,
        "skyCamera": sky_camera.map(|v| [v.x, v.y, v.z]),
        "skyCameraAngles": [sky_angles.x, sky_angles.y, sky_angles.z],
        "skyScale": sky_scale,
        "skyboxProps": skybox_props,
        "materials": materials.len(),
        "drawCalls": draw_calls,
        "triangles": tris,
        "staticProps": prop_count,
        "staticPropsSkipped": prop_skipped,
        "images": images.len(),
        "worldGlbBytes": glb.len(),
        "lightmap": lightmaps.as_ref().map(|a| json!({"width": a.width, "height": a.height, "pages": a.pages.len(),
            "encoding": "RGB = baked indirect+sky light (linear, /4 so 1.0 = 4x), A = sun visibility; sample with TEXCOORD_1"})),
        "dynamicProps": dynamic,
        "dynamicPropModels": dyn_report,
        "seconds": t0.elapsed().as_secs_f32(),
    });
    std::fs::write(out_dir.join("meta.json"), serde_json::to_vec_pretty(&meta)?)?;
    eprintln!("== collision");
    write_collision(&bsp, &paks, &prefer, &out_dir, false, args.prop_physics)?;
    eprintln!(
        "done in {:.0}s: world.glb {:.1} MB, {} draw calls, {} materials, {} images, {tris} triangles, {prop_count} static props",
        t0.elapsed().as_secs_f32(),
        glb.len() as f64 / 1e6,
        draw_calls,
        materials.len(),
        images.len()
    );
    println!("{}", out_dir.display());
    Ok(())
}

// ------------------------------------------------------------ lightmap ----

/// The map's lightmap pages packed side by side into one texture.
struct Atlas {
    width: u32,
    height: u32,
    /// (x offset, width, height) per page.
    pages: Vec<(u32, u32, u32)>,
    rgba: Vec<u8>,
}

impl Atlas {
    fn remap(&self, page: usize, uv: [f32; 2]) -> [f32; 2] {
        let Some(&(x0, w, h)) = self.pages.get(page) else { return [0.0, 0.0] };
        [(x0 as f32 + uv[0] * w as f32) / self.width as f32, uv[1] * h as f32 / self.height as f32]
    }

    fn encode(&self) -> Result<(Vec<u8>, &'static str)> {
        let img = tex::Rgba { width: self.width, height: self.height, pixels: self.rgba.clone() };
        img.encode(true, 90)
    }
}

/// Decodes the type-1 sky pages (two RGBA8 images per page: RGBM baked light,
/// then direction + sun visibility in alpha) into one RGBA8 atlas:
/// RGB = linear light / 4, A = sun visibility. Layout read from r5ed's rbake.
/// BC7 blocks (`w x h`, multiples of 4) to RGBA8.
fn bc7_to_rgba(src: &[u8], w: usize, h: usize) -> Vec<u8> {
    let mut out = vec![0u8; w * h * 4];
    let (bw, bh) = (w / 4, h / 4);
    for by in 0..bh {
        for bx in 0..bw {
            let i = (by * bw + bx) * 16;
            let Some(block) = src.get(i..i + 16) else { return out };
            bcdec_rs::bc7(block, &mut out[(by * 4 * w + bx * 4) * 4..], w * 4);
        }
    }
    out
}

fn build_lightmap_atlas(bsp: &rbsp::Bsp, headers: &[rbsp::lumps::lighting::LightmapHeader]) -> Result<Option<Atlas>> {
    // Retail (BSP v54, page type 11) keeps the two lightmaps BC7-compressed in lump 0x61 (w*h bytes each, lm0 then lm1
    // per page) instead of S21's raw RGBA8 planes in 0x62: decode them into S21's layout and go on as before.
    let retail: Option<Vec<u8>> = headers.iter().any(|h| h.lightmap_type == 11).then(|| {
        let comp = bsp.lump(0x61);
        let mut v = Vec::new();
        let mut at = 0usize;
        for h in headers {
            let (w, hh) = (h.width as usize, h.height as usize);
            for _ in 0..2 {
                let n = w * hh;
                v.extend(bc7_to_rgba(comp.get(at..at + n).unwrap_or(&[]), w, hh));
                at += n;
            }
        }
        v
    });
    let data: &[u8] = retail.as_deref().unwrap_or_else(|| bsp.lump(rbsp::ids::LIGHTMAP_DATA_SKY));
    if headers.is_empty() || data.is_empty() {
        return Ok(None);
    }
    let mut pages = Vec::new();
    let mut at = 0usize;
    let mut x = 0u32;
    let mut height = 0u32;
    let mut slices = Vec::new();
    for h in headers {
        let (w, hh) = (h.width as usize, h.height as usize);
        if h.lightmap_type != 1 && h.lightmap_type != 10 && h.lightmap_type != 11 {
            eprintln!("  lightmap page type {} not decoded; lightmaps skipped", h.lightmap_type);
            return Ok(None);
        }
        let n = w * hh * 8;
        if at + n > data.len() {
            bail!("lightmap pages run past the lump");
        }
        slices.push((at, w, hh));
        pages.push((x, w as u32, hh as u32));
        x += w as u32;
        height = height.max(hh as u32);
        at += n;
    }
    let width = x;
    let mut rgba = vec![0u8; (width * height * 4) as usize];
    for (pi, &(off, w, hh)) in slices.iter().enumerate() {
        let x0 = pages[pi].0 as usize;
        let img0 = &data[off..off + w * hh * 4];
        let img1 = &data[off + w * hh * 4..off + w * hh * 8];
        for y in 0..hh {
            for xx in 0..w {
                let s = (y * w + xx) * 4;
                let m = img0[s + 3] as f32 / 255.0 * 3.984375 + 1.0 / 64.0;
                let d = (y * width as usize + x0 + xx) * 4;
                for c in 0..3 {
                    let lin = img0[s + c] as f32 / 255.0 * m;
                    rgba[d + c] = ((lin / 4.0).clamp(0.0, 1.0) * 255.0).round() as u8;
                }
                rgba[d + 3] = img1[s + 3];
            }
        }
    }
    eprintln!("  lightmap: {} pages -> {width}x{height} atlas", pages.len());
    Ok(Some(Atlas { width, height, pages, rgba }))
}
