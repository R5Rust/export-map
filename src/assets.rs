//! The game's paks, read through RSX (the R5Flowstate fork r5ed uses, which is
//! the one with `--exportguids` / `--exportsetting`).
//!
//! RSX never sees a file inside the game install: every pak it reads is a copy
//! in `<work>/mirror/paks/Win64`, refreshed when the install's size or mtime
//! changes. (A misplaced `--list <path>` once overwrote a real rpak.) Starpaks
//! a pak streams from are copied on demand: RSX names the ones it cannot open.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use anyhow::{anyhow, bail, Context, Result};

pub struct Config {
    pub game_root: PathBuf,
    pub rsx: PathBuf,
    pub work: PathBuf,
}

/// One asset row of an RSX listing.
#[derive(Clone, Debug)]
pub struct Row {
    pub kind: String,
    pub guid: u64,
    /// The pak's base file name (`common.rpak`), patches folded in.
    pub pak: String,
}

pub struct Paks {
    pub cfg: Config,
    /// Base pak name -> the file RSX is handed (the highest patch, which
    /// pulls its base in through patch_master).
    pub load: BTreeMap<String, PathBuf>,
    /// Lower-case `/` asset path -> rows (one per pak that carries it).
    pub by_path: BTreeMap<String, Vec<Row>>,
    /// guid -> (pak base name, lower-case `/` asset path) for every pak that carries it.
    pub by_guid: BTreeMap<u64, Vec<(String, String)>>,
}

/// Respawn's rpak StringToGuid: an asset's guid is this hash of its path (`mdl/props/x.rmdl`). Port of r5ed-compile's
/// `string_to_guid`; checked against the asset catalog (material/world/concrete/dam_concrete_02_stained_bm_wldc.rpak
/// -> 0xE779D0F273761152).
pub fn string_to_guid(name: &str) -> u64 {
    let mut b: Vec<u8> = name.bytes().collect();
    b.extend_from_slice(&[0u8; 8]);
    let word = |at: usize| u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]);
    let mut acc: u64 = 0;
    let mut at = 0usize;
    let (v5, v12, v11) = loop {
        let w = word(at);
        let v4 = !w & w.wrapping_sub(0x0101_0101) & 0x8080_8080;
        let v5 = v4 ^ v4.wrapping_sub(1);
        let v6 = (v5 & w) ^ 0x5C5C_5C5C;
        let v7 = !v6 & v6.wrapping_sub(0x0101_0101) & 0x8080_8080;
        let mut v8 = v7 & v7.wrapping_neg();
        if v7 != v8 {
            let mut v9: u32 = 0xFF00_0000;
            loop {
                let prev = v9;
                if v9 & v6 == 0 {
                    v8 |= v9 & 0x8080_8080;
                }
                v9 >>= 8;
                if prev < 0x100 {
                    break;
                }
            }
        }
        let v11 = 0x633D_5F1u64.wrapping_mul(acc);
        let inner = (v5 & w).wrapping_sub(45u32.wrapping_mul(v8 >> 7)) & 0xDFDF_DFDF;
        let v12 = 0xFB8C_4D96_501u64.wrapping_mul(u64::from(inner)) >> 24;
        if v4 != 0 {
            break (v5, v12, v11);
        }
        at += 4;
        let sum = v11.wrapping_add(v12);
        acc = (sum >> 61) ^ sum;
    };
    let high = if v5 == 0 { 0i64 } else { i64::from(31 - v5.leading_zeros()) };
    let index = (at as u32).wrapping_add((high / 8) as u32);
    v12.wrapping_add(v11).wrapping_sub(0xAE50_2812_AA73_33u64.wrapping_mul(u64::from(index)))
}

/// The by_path key RSX gives an unnamed asset whose name is `path`: `<first folder>/0x<guid>` (no zero padding).
pub fn unnamed_key(path: &str) -> String {
    let folder = path.split('/').next().unwrap_or("");
    format!("{folder}/0x{:x}", string_to_guid(path))
}

pub fn mirror_dir(cfg: &Config) -> PathBuf {
    cfg.work.join("mirror").join("paks").join("Win64")
}

/// Where a pak file lives: the install's own paks first, then every enabled mod's
/// `paks/Win64` (heirloom mods like KainzYT.Pencil ship their models there).
fn pak_src(cfg: &Config, name: &str) -> Option<PathBuf> {
    let own = cfg.game_root.join("paks").join("Win64").join(name);
    if own.is_file() {
        return Some(own);
    }
    enabled_mod_dirs(cfg).into_iter().map(|d| d.join("paks").join("Win64").join(name)).find(|p| p.is_file())
}

/// Folders of the mods switched on in `mods/mods.vdf` (matched by the `id` in each mod.vdf).
pub fn enabled_mod_dirs(cfg: &Config) -> Vec<PathBuf> {
    let mods = cfg.game_root.join("mods");
    let Ok(vdf) = std::fs::read_to_string(mods.join("mods.vdf")) else { return Vec::new() };
    let on: BTreeSet<String> = vdf
        .lines()
        .filter_map(|l| {
            let q: Vec<&str> = l.split('"').collect();
            (q.len() >= 5 && q[3] == "1").then(|| q[1].to_string())
        })
        .collect();
    let Ok(rd) = std::fs::read_dir(&mods) else { return Vec::new() };
    rd.filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|d| {
            std::fs::read_to_string(d.join("mod.vdf"))
                .ok()
                .and_then(|v| v.lines().find(|l| l.trim_start().starts_with("\"id\"")).map(|l| l.split('"').nth(3).unwrap_or("").to_string()))
                .is_some_and(|id| on.contains(&id))
        })
        .collect()
}

/// Base names of the rpaks the enabled mods ship (their own families, not patches).
pub fn mod_paks(cfg: &Config) -> Vec<String> {
    let mut out = Vec::new();
    for d in enabled_mod_dirs(cfg) {
        if let Ok(rd) = std::fs::read_dir(d.join("paks").join("Win64")) {
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().to_string();
                if n.ends_with(".rpak") && !n.contains('(') && !out.contains(&n) {
                    out.push(n);
                }
            }
        }
    }
    out
}

/// Copies `name` from the install into the mirror unless an identical copy
/// (same size and mtime) is already there. Returns false if the install has
/// no such file.
fn mirror_file(cfg: &Config, name: &str) -> Result<bool> {
    let Some(src) = pak_src(cfg, name) else { return Ok(false) };
    let Ok(meta) = std::fs::metadata(&src) else { return Ok(false) };
    let dst = mirror_dir(cfg).join(name);
    if let Ok(m) = std::fs::metadata(&dst) {
        if m.len() == meta.len() && m.modified().ok() == meta.modified().ok() {
            return Ok(true);
        }
    }
    std::fs::create_dir_all(mirror_dir(cfg))?;
    let t = Instant::now();
    std::fs::copy(&src, &dst).with_context(|| format!("copying {name} into the mirror"))?;
    // Keep the install's mtime so the next run sees the copy as current.
    if let Ok(mt) = meta.modified() {
        let f = std::fs::File::options().write(true).open(&dst)?;
        f.set_modified(mt)?;
    }
    eprintln!("  mirrored {name} ({:.0} MB, {:.1}s)", meta.len() as f64 / 1e6, t.elapsed().as_secs_f32());
    Ok(true)
}

/// Every file of a pak family: the base and its `(NN)` patches, sorted.
fn family(cfg: &Config, base: &str) -> Vec<String> {
    let stem = base.trim_end_matches(".rpak");
    let mut out = Vec::new();
    if pak_src(cfg, base).is_some() {
        out.push(base.to_string());
    }
    for n in 1..100 {
        let name = format!("{stem}({n:02}).rpak");
        if pak_src(cfg, &name).is_some() {
            out.push(name);
        }
    }
    out
}

/// A starpak and its `(NN)` patches, as they exist in the install or a mod.
fn family_starpak(cfg: &Config, base: &str) -> Vec<String> {
    let stem = base.trim_end_matches(".starpak");
    let mut out = Vec::new();
    if pak_src(cfg, base).is_some() {
        out.push(base.to_string());
    }
    for n in 1..100 {
        let name = format!("{stem}({n:02}).starpak");
        if pak_src(cfg, &name).is_some() {
            out.push(name);
        }
    }
    out
}

/// Runs RSX headless with `args` (flags first, then pak paths) in `cwd`.
/// RSX crashes now and then on a run that succeeds when repeated (seen as
/// exit 0xC0000409 on the same model twice in a row, then fine), so a failed
/// run is retried twice before giving up.
pub fn run_rsx(cfg: &Config, args: &[std::ffi::OsString], cwd: &Path) -> Result<String> {
    let mut last = None;
    for attempt in 0..3 {
        match run_rsx_once(cfg, args, cwd) {
            Ok(t) => return Ok(t),
            Err(e) => {
                if attempt < 2 {
                    eprintln!("  rsx failed ({e}); retrying");
                }
                last = Some(e);
            }
        }
    }
    Err(last.unwrap())
}

fn run_rsx_once(cfg: &Config, args: &[std::ffi::OsString], cwd: &Path) -> Result<String> {
    std::fs::create_dir_all(cwd)?;
    let t = Instant::now();
    // Model exports can go through a second RSX (R5_RSX_MODELS): retail Apex models are mdl v21, which the r5ed
    // bundle's RSX 1.2.1 can't read. D:\R5Rust\tools\rsx-src (2.4 + upstream's v21 cases) can, and writes the same raw
    // .rmdl/.vg pair when asked for format 3 (it defaults to Cast).
    // "--cast" (ours, not RSX's): keep RSX 2.4's default model format, Cast.
    let cast = args.iter().any(|a| a == "--cast");
    let args: Vec<&std::ffi::OsString> = args.iter().filter(|a| *a != "--cast").collect();
    let models = args.windows(2).any(|w| *w[0] == "--exporttypes" && *w[1] == "mdl_");
    let modern = models.then(|| std::env::var_os("R5_RSX_MODELS").map(PathBuf::from)).flatten();
    let mut cmd = Command::new(modern.as_ref().unwrap_or(&cfg.rsx));
    if let Some(rsx) = &modern {
        // R5Flowstate's RSX fork (github.com/R5Flowstate/rsx, what tools/install.ps1 uses) picks formats with
        // `--exportsetting mdl_=N` (0 Cast, 2 raw RMDL, its default); our old patched 2.4 build took `--exportformat`.
        if is_rsx_fork(rsx) {
            cmd.args(["--exportsetting", if cast { "mdl_=0" } else { "mdl_=2" }]);
        } else if !cast {
            cmd.args(["--exportformat", "mdl_=3"]);
        }
    }
    let out = cmd
        .arg("-nogui")
        .arg("-nocachedb")
        .args(args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .output()
        .with_context(|| format!("starting {}", cfg.rsx.display()))?;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    if std::env::var("EXPORT_MAP_TRACE").as_deref() == Ok("2") {
        eprintln!("  rsx {:.1}s full output:\n{}", t.elapsed().as_secs_f32(), text.lines().filter(|l| !l.trim().is_empty()).rev().take(60).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n"));
    } else if std::env::var_os("EXPORT_MAP_TRACE").is_some() {
        eprintln!("  rsx {:.1}s: {}", t.elapsed().as_secs_f32(), text.lines().filter(|l| l.starts_with("EXPORT") || l.contains("WARN")).collect::<Vec<_>>().join(" | "));
    }
    if !out.status.success() {
        bail!("rsx exited with {:?}: {}", out.status.code(), text.trim());
    }
    if cwd.join("latest_crash").is_dir() {
        let _ = std::fs::remove_dir_all(cwd.join("latest_crash"));
        bail!("rsx crashed (see its output): {}", text.lines().rev().take(5).collect::<Vec<_>>().join(" | "));
    }
    Ok(text)
}

/// True for R5Flowstate's RSX fork (r5ed's bundled rsx.exe is an older build of it): the only builds that know
/// `--exportpak` (upstream and our old patched 2.4 don't). Checked once per exe, by its bytes.
fn is_rsx_fork(rsx: &Path) -> bool {
    use std::sync::{Mutex, OnceLock};
    static SEEN: OnceLock<Mutex<BTreeMap<PathBuf, bool>>> = OnceLock::new();
    let seen = SEEN.get_or_init(Default::default);
    if let Some(&v) = seen.lock().unwrap().get(rsx) {
        return v;
    }
    let v = std::fs::read(rsx).map(|b| b.windows(11).any(|w| w == b"--exportpak")).unwrap_or(false);
    seen.lock().unwrap().insert(rsx.to_path_buf(), v);
    v
}

/// Starpak names RSX reported it could not open (the `.opt` ones are
/// optional high-res streams this install does not ship).
fn missing_starpaks(text: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for line in text.lines() {
        if let Some(i) = line.find("Failed to open StarPak file \"") {
            let rest = &line[i + 29..];
            if let Some(end) = rest.find('"') {
                out.insert(rest[..end].to_string());
            }
        }
    }
    out
}

impl Paks {
    /// Mirrors the named pak families, makes sure the starpaks they stream
    /// from are mirrored too, and indexes every asset.
    pub fn open(cfg: Config, bases: &[String]) -> Result<Self> {
        if !cfg.rsx.is_file() {
            bail!("RSX not found at {} (set R5_RSX to the rsx.exe that ships with r5ed)", cfg.rsx.display());
        }
        mirror_file(&cfg, "patch_master.rpak")?;
        let mut load = BTreeMap::new();
        for base in bases {
            let fam = family(&cfg, base);
            if fam.is_empty() {
                continue;
            }
            for f in &fam {
                mirror_file(&cfg, f)?;
            }
            // a pak's own starpak next to it (RePak-built mod paks stream their models from
            // <name>.starpak, and RSX doesn't name it when a model export needs it)
            for sp in family_starpak(&cfg, &base.replace(".rpak", ".starpak")) {
                mirror_file(&cfg, &sp)?;
            }
            load.insert(base.clone(), mirror_dir(&cfg).join(fam.last().unwrap()));
        }
        let mut paks = Paks { cfg, load, by_path: BTreeMap::new(), by_guid: BTreeMap::new() };
        let index_dir = paks.cfg.work.join("index");
        std::fs::create_dir_all(&index_dir)?;
        for (base, file) in paks.load.clone() {
            let meta = std::fs::metadata(&file)?;
            let mt = meta.modified().ok().and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
            let csv = index_dir.join(format!("{}.{}.{mt}.csv", base.trim_end_matches(".rpak"), meta.len()));
            if !csv.is_file() {
                let tmp = index_dir.join("listing.csv");
                let _ = std::fs::remove_file(&tmp);
                let args: Vec<std::ffi::OsString> =
                    vec!["--list".into(), tmp.clone().into(), "--listformat".into(), "csv".into(), file.clone().into()];
                let text = run_rsx(&paks.cfg, &args, &index_dir)?;
                // A starpak the pak names but the mirror lacks: copy it in.
                for sp in missing_starpaks(&text) {
                    if !sp.contains(".opt.") {
                        mirror_file(&paks.cfg, &sp)?;
                    }
                }
                std::fs::rename(&tmp, &csv).with_context(|| format!("RSX wrote no listing for {base}"))?;
            }
            // Retail lists newer formats with a minor version in rsxver ("v23.2" for v23 materials); r5ed's parser
            // takes a whole number, so keep the major part. S21 listings never have one.
            let text = std::fs::read_to_string(&csv)?;
            let text = text
                .lines()
                .map(|l| {
                    let mut f: Vec<&str> = l.splitn(4, ',').collect();
                    if f.len() == 4 && f[2].starts_with('v') {
                        f[2] = f[2].split('.').next().unwrap_or(f[2]);
                    }
                    f.join(",")
                })
                .collect::<Vec<_>>()
                .join("\n");
            let rows = rpak::parse_asset_list(&text).map_err(|e| anyhow!("{}: {e}", csv.display()))?;
            for r in rows {
                let path = r.path.replace('\\', "/").to_ascii_lowercase();
                paks.by_guid.entry(r.guid).or_default().push((base.clone(), path.clone()));
                paks.by_path.entry(path).or_default().push(Row { kind: r.kind, guid: r.guid, pak: base.clone() });
            }
        }
        // Make sure the streaming starpaks are present before anything is
        // exported: one listing run per pak above only parses what it needs,
        // so ask each pak to export nothing and collect what it misses.
        for (base, file) in paks.load.clone() {
            // The marker lists the starpaks this pak needed; it only counts while they are all
            // still in the mirror (a scratch cleanup once removed them and left the marker).
            let marker = mirror_dir(&paks.cfg).join(format!("{}.starpaks-ok", base.trim_end_matches(".rpak")));
            if let Ok(list) = std::fs::read_to_string(&marker) {
                if list.lines().filter(|l| !l.is_empty()).all(|sp| mirror_dir(&paks.cfg).join(sp).is_file()) && !list.is_empty() {
                    continue;
                }
            }
            let probe = paks.cfg.work.join("probe");
            let args: Vec<std::ffi::OsString> = vec![
                "-export".into(),
                "--exporttypes".into(),
                "none".into(),
                "--exportdir".into(),
                probe.clone().into(),
                file.clone().into(),
            ];
            let text = run_rsx(&paks.cfg, &args, &probe).unwrap_or_default();
            let mut have = vec![String::from("patch_master.rpak")];
            for sp in missing_starpaks(&text) {
                if !sp.contains(".opt.") {
                    if mirror_file(&paks.cfg, &sp)? {
                        have.push(sp);
                    } else {
                        eprintln!("  warning: {base} streams from {sp}, which the install does not have");
                    }
                }
            }
            // RSX only names a starpak it cannot open when it gets as far as streaming from it;
            // common.rpak's models stream from pc_all, so mirror that family outright for it.
            if base.starts_with("common") {
                for sp in family_starpak(&paks.cfg, "pc_all.starpak") {
                    if mirror_file(&paks.cfg, &sp)? {
                        have.push(sp);
                    }
                }
            }
            let _ = std::fs::remove_dir_all(&probe);
            std::fs::write(&marker, have.join("\n"))?;
        }
        Ok(paks)
    }

    /// The first row of `kind` at `path` (lower-case, `/`), preferring the
    /// order of `prefer` when several paks carry it.
    pub fn find(&self, path: &str, kind: &str, prefer: &[&str]) -> Option<&Row> {
        let key = path.replace('\\', "/").to_ascii_lowercase();
        // Retail paks carry no asset names (`mdl\0x4D874CEE9B0508A7`): the guid is the hash of the name, so look the
        // name up by its hash under the same folder.
        let rows = self.by_path.get(&key).or_else(|| self.by_path.get(&unnamed_key(&key)))?;
        let mut best: Option<&Row> = None;
        let rank = |p: &str| prefer.iter().position(|q| *q == p).unwrap_or(usize::MAX);
        for r in rows.iter().filter(|r| r.kind == kind) {
            if best.is_none_or(|b| rank(&r.pak) < rank(&b.pak)) {
                best = Some(r);
            }
        }
        best
    }

    /// Exports assets of `kind` by guid in ONE RSX run with every pak loaded,
    /// so cross-pak references resolve (a material's texture roles come from
    /// its shader set, which usually lives in another pak).
    pub fn export_together(&self, kind: &str, extra: &[&str], guids: &[u64], out: &Path) -> Result<()> {
        if guids.is_empty() {
            return Ok(());
        }
        std::fs::create_dir_all(out)?;
        let t = Instant::now();
        let gf = out.join(format!("guids_{kind}.txt"));
        std::fs::write(&gf, guids.iter().map(|g| format!("{g:016x}\n")).collect::<String>())?;
        let mut args: Vec<std::ffi::OsString> = vec!["-export".into()];
        args.extend(extra.iter().map(Into::into));
        args.extend(["--exporttypes".into(), kind.into(), "--exportguids".into(), gf.clone().into(), "--exportdir".into(), out.into()]);
        args.extend(self.load.values().map(|p| p.clone().into_os_string()));
        run_rsx(&self.cfg, &args, out)?;
        let _ = std::fs::remove_file(&gf);
        eprintln!("  {kind}: {} with all paks loaded in {:.1}s", guids.len(), t.elapsed().as_secs_f32());
        Ok(())
    }

    /// The asset path of a guid, if any indexed pak carries it.
    pub fn path_of(&self, guid: u64) -> Option<&str> {
        self.by_guid.get(&guid).and_then(|v| v.first()).map(|p| p.1.as_str())
    }

    /// Exports assets of `kind` by guid, one RSX run per pak that carries
    /// them (each run decompresses its pak once). Returns guids no pak had.
    pub fn export(&self, kind: &str, extra: &[&str], guids: &[u64], out: &Path, prefer: &[&str]) -> Result<Vec<u64>> {
        let mut by_pak: BTreeMap<String, Vec<u64>> = BTreeMap::new();
        let mut missing = Vec::new();
        let rank = |p: &str| prefer.iter().position(|q| *q == p).unwrap_or(usize::MAX);
        for &g in guids {
            match self.by_guid.get(&g).and_then(|ps| ps.iter().min_by_key(|p| rank(&p.0))) {
                Some(p) => by_pak.entry(p.0.clone()).or_default().push(g),
                None => missing.push(g),
            }
        }
        std::fs::create_dir_all(out)?;
        for (pak, list) in by_pak {
            let t = Instant::now();
            let gf = out.join(format!("guids_{kind}.txt"));
            std::fs::write(&gf, list.iter().map(|g| format!("{g:016x}\n")).collect::<String>())?;
            let mut args: Vec<std::ffi::OsString> = vec!["-export".into()];
            args.extend(extra.iter().map(Into::into));
            args.extend(["--exporttypes".into(), kind.into(), "--exportguids".into(), gf.clone().into(), "--exportdir".into(), out.into()]);
            args.push(self.load[&pak].clone().into());
            run_rsx(&self.cfg, &args, out)?;
            let _ = std::fs::remove_file(&gf);
            eprintln!("  {kind}: {} from {pak} in {:.1}s", list.len(), t.elapsed().as_secs_f32());
        }
        Ok(missing)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn guid_matches_the_catalog() {
        assert_eq!(super::string_to_guid("material/world/concrete/dam_concrete_02_stained_bm_wldc.rpak"), 0xE779_D0F2_7376_1152);
        assert_eq!(super::string_to_guid("mdl/props/charge_pylon/charge_pylon_01_cells.rmdl"), 0xB269_9222_3525_B5FD);
        assert_eq!(super::unnamed_key("mdl/props/charge_pylon/charge_pylon_01_cells.rmdl"), "mdl/0xb26992223525b5fd");
    }
}
