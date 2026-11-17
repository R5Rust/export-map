# export-map

Builds R5Rust's map data from **your own Apex install** (R5Flowstate S21 paks). This repo contains no game files:
everything it writes comes out of the paks on your machine.

## What it makes

Written to the R5Rust asset folder (e.g. `D:\R5Rust\assets\maps\mp_rr_canyonlands_staging_mu1\`):

| File | What |
|---|---|
| `world.glb` + `meta.json` | render geometry per material, prop skins applied, `noShadow` / `skybox` tags, spawns, sky camera |
| `collision.bin` | the game's own collision (BSP BVH4 leaves + static-prop collision trees), with contents masks |
| `bsp\*.bin`, `*.ent` | lighting lumps (lightmaps, light probes, RTL pages, cubemap refs) and entity files |
| `skies\`, `cubemaps.dds`, `*.raw_hdr` | sky cubes, env cubemaps, the colour-grading LUT |
| `materials.json` | (`build_materials.py`) material constants + texture roles for the extracted textures |

## Run

```powershell
cargo run --release -- --out D:\R5Rust\assets\maps\mp_rr_canyonlands_staging_mu1      # full export, ~2 min
cargo run --release -- --collision --out D:\R5Rust\assets\maps\mp_rr_canyonlands_staging_mu1   # collision only, ~20 s
python build_materials.py                                                                  # after every full export
```

Env: `R5_GAME_ROOT` (default `%USERPROFILE%\Games\R5Flowstate`), `R5_RSX` (rsx.exe for texture extraction),
`R5_EXPORT_WORK` (work dir, default `%TEMP%\r5-export-map`). Game formats are read through
[R5Flowstate/r5ed](https://github.com/R5Flowstate/r5ed)'s `rbsp` / `rcomp` / `rpak` crates (pinned git dependency).

Debug dumps: `R5_PROPS_AT=x,y` lists static props near a point with their flags; `R5_MESHFLAGS=1` prints render-mesh
flag values and stops.

## Status

Firing Range only. Planned: run the extraction from the R5Rust launcher against the player's install, so nothing
derived from the game is ever distributed.
