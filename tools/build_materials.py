"""One-time: map every world.glb material to the game's own texture files (col/nml/gls/spc/ao)
by reading the extracted material JSONs. Writes <map>/materials.json next to world.glb.
  python tools/build_materials.py [assets_root] [map]"""
import json, struct, sys, os
root = sys.argv[1] if len(sys.argv) > 1 else r'D:\R5Rust\assets'
mapn = sys.argv[2] if len(sys.argv) > 2 else 'mp_rr_canyonlands_staging_mu1'
mdir = os.path.join(root, 'maps', mapn)
f = open(os.path.join(mdir, 'world.glb'), 'rb')
h = f.read(20); L = struct.unpack('<I', h[12:16])[0]; j = json.loads(f.read(L))
ix = json.load(open(os.path.join(root, 'index.json'), encoding='utf-8'))['by_name']
SHADERS = ['wldc', 'rgdp', 'rgdc', 'sknp', 'sknc', 'wldu', 'rgdu', 'skn', 'fix']
out, miss = {}, []
for m in j['materials']:
    name = m['name']; leaf = name.rsplit('/', 1)[-1].lower()
    found = None
    for sh in SHADERS:
        for p in ix.get(f'{leaf}_{sh}', []):
            if p.startswith('materials/') and p.endswith('.json'):
                d = json.load(open(os.path.join(root, p), encoding='utf-8'))
                if d.get('name', '').lower() == name.lower():
                    found = d; break
        if found: break
    if not found:
        miss.append(name); continue
    tex, types = found.get('$textures', {}), found.get('$textureTypes', {})
    slots = {}
    for slot in sorted(tex, key=int):
        t = types.get(slot, '').lstrip('_')
        if t in slots: continue
        stem = tex[slot].replace(chr(92), '/').rsplit('/', 1)[-1].rsplit('.', 1)[0].lower()
        imgs = [p for p in ix.get(stem, []) if p.startswith('images/')]
        if imgs: slots[t] = imgs[0]
    CANON = {'albedo': ['albedoTexture', 'col'], 'normal': ['normalTexture', 'nml'], 'gloss': ['glossTexture', 'gls'],
             'spec': ['specTexture', 'spc'], 'ao': ['aoTexture', 'ao'], 'emissive': ['emissiveTexture', 'ilm']}
    canon = {k: next((slots[a] for a in alts if a in slots), None) for k, alts in CANON.items()}
    out[name] = {'shader': found.get('shaderType'), **{k: v for k, v in canon.items() if v}, 'all': slots}
json.dump(out, open(os.path.join(mdir, 'materials.json'), 'w'), indent=1)
from collections import Counter
c = Counter(k for v in out.values() for k in v if k not in ('shader', 'all'))
print(f'{len(out)} materials mapped, {len(miss)} missing: {miss[:5]}'); print('slots:', dict(c))
