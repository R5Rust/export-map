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
# texture refs given as raw GUIDs: resolve through the asset catalog (guid -> name), else a sibling variant
import csv, glob
cat_path = os.path.join(os.path.dirname(os.path.normpath(root)), '_work', 'catalog.csv')
guid_name = {}
if os.path.exists(cat_path):
    for r in csv.DictReader(open(cat_path, encoding='utf-8')):
        if r['type'] == 'txtr' and not r['asset_name'].lower().startswith('texture' + chr(92) + '0x'):
            guid_name[r['guid'].lower().lstrip('0')] = r['asset_name']
def sibling_textures(leaf, typ):
    for key in ix:
        if key.startswith(leaf + '_') and not key.startswith(leaf + '_avt'):
            for p in ix[key]:
                if p.startswith('materials/') and p.endswith('.json'):
                    try: d = json.load(open(os.path.join(root, p), encoding='utf-8'))
                    except Exception: continue
                    for sl, t in d.get('$textureTypes', {}).items():
                        v = d.get('$textures', {}).get(sl, '')
                        if t == typ and not v.startswith('0x'):
                            return v
    return None
for m in j['materials']:
    name = m['name']; leaf = name.rsplit('/', 1)[-1].lower()
    found = None
    for sh in SHADERS:
        for p in ix.get(f'{leaf}_{sh}', []):
            if p.startswith('materials/') and p.endswith('.json'):
                d = json.load(open(os.path.join(root, p), encoding='utf-8'))
                # skip variants whose slot types weren't exported ('unavailable')
                if d.get('name', '').lower() == name.lower() and any(t != 'unavailable' for t in d.get('$textureTypes', {}).values()):
                    d['_path'] = os.path.join(root, p); found = d; break
        if found: break
    if not found:
        # every variant has 'unavailable' slot types: take one anyway, types inferred from the shader's slot order
        for sh in SHADERS:
            for p in ix.get(f'{leaf}_{sh}', []):
                if p.startswith('materials/') and p.endswith('.json') and not found:
                    d = json.load(open(os.path.join(root, p), encoding='utf-8'))
                    if d.get('name', '').lower() == name.lower():
                        order = ['albedoTexture', 'normalTexture', 'glossTexture', 'specTexture', 'aoTexture']
                        d['$textureTypes'] = {k: (order[int(k)] if int(k) < len(order) else 'unknown') for k in d.get('$textures', {})}
                        d['_path'] = os.path.join(root, p); found = d
    if not found:
        miss.append(name); continue
    tex, types = found.get('$textures', {}), found.get('$textureTypes', {})
    slots = {}
    for slot in sorted(tex, key=int):
        t = types.get(slot, '').lstrip('_')
        if t in slots: continue
        ref = tex[slot]
        if ref.startswith('0x'):
            ref = guid_name.get(ref[2:].lower().lstrip('0')) or sibling_textures(leaf, types.get(slot, '')) or ref
        stem = ref.replace(chr(92), '/').rsplit('/', 1)[-1].rsplit('.', 1)[0].lower()
        imgs = [p for p in ix.get(stem, []) if p.startswith('images/')]
        # still a bare GUID: images/textures/_guid/0x<GUID>.png (RSX export by guid from the shared paks)
        if not imgs and ref.startswith('0x'):
            g = 'images/textures/_guid/0x' + ref[2:].upper() + '.png'
            if os.path.exists(os.path.join(root, g)):
                imgs = [g]
        if imgs: slots[t] = imgs[0]
    CANON = {'albedo': ['albedoTexture', 'col'], 'normal': ['normalTexture', 'nml'], 'gloss': ['glossTexture', 'gls'],
             'spec': ['specTexture', 'spc'], 'ao': ['aoTexture', 'ao'], 'emissive': ['emissiveTexture', 'ilm']}
    canon = {k: next((slots[a] for a in alts if a in slots), None) for k, alts in CANON.items()}
    # per-material constants (CBufUberStatic, exported as text next to the JSON): L0 albedo + emissive tints
    tints = {}
    up = found.get('_path', '').rsplit('.', 1)[0] + '.uber_struct'
    if os.path.exists(up):
        import re
        for line in open(up, encoding='utf-8', errors='ignore'):
            mt = re.search(r'float (\w+)\[(\d)\] = \{([^}]*)\}', line)
            if not mt: continue
            nm, vals = mt.group(1), [float(x) for x in mt.group(3).split(',')]
            for key, field in (('albedo_tint', 'L0_albedoTint'), ('emissive_tint', 'L0_emissiveTint')):
                if nm.endswith(field):
                    tints[key] = vals[-3:] if 'And' in nm else vals[:3]
        for line in open(up, encoding='utf-8', errors='ignore'):
            mt = re.search(r'float c_emissiveEdgeFade(Exponent|Inner|Outer) = ([-0-9.e]+);', line)
            if mt:
                tints.setdefault('emissive_edge', [0.0, 1.0, 1.0])[('Exponent', 'Inner', 'Outer').index(mt.group(1))] = float(mt.group(2))
    out[name] = {'shader': found.get('shaderType'), **{k: v for k, v in canon.items() if v}, **tints, 'all': slots}
json.dump(out, open(os.path.join(mdir, 'materials.json'), 'w'), indent=1)
from collections import Counter
c = Counter(k for v in out.values() for k in v if k not in ('shader', 'all'))
print('non-default tints:', sum(1 for v in out.values() if v.get('albedo_tint', [1,1,1]) != [1,1,1] or v.get('emissive_tint')))
print(f'{len(out)} materials mapped, {len(miss)} missing: {miss[:5]}'); print('slots:', dict(c))
