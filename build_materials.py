"""One-time: map every world.glb material to the game's own texture files (col/nml/gls/spc/ao)
by reading the extracted material JSONs, plus each material's UV routing read from its own pixel shader
(needs Windows' d3dcompiler_47.dll to disassemble it; docs/apex-lighting.md 1.1). Writes <map>/materials.json next to world.glb.
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
# ---- per-texture UV routing from the pixel shader (docs/apex-lighting.md section 1.1)
# Every texture of a world/prop PS samples one UV: the mesh UV (v0.xy) or a c_uvN transform (CBufUberStatic
# c_uvNRotScaleX/Y + c_uvNTranslate) of either the mesh UV or world position (x, -y). The routing differs per
# shader (layer 1 of one sand blend is on the mesh UV, of another on world uv2), so read it from the DXBC.
UVX = {(0, 'x'): 1, (1, 'z'): 2, (3, 'x'): 3, (4, 'z'): 4, (6, 'x'): 5}   # cb0 position of c_uvNRotScaleX
_ps_cache = {}
def disassemble(path):
    import ctypes
    from ctypes import c_void_p, c_size_t, c_uint, POINTER, byref
    d3d = ctypes.WinDLL('d3dcompiler_47.dll')
    f = d3d.D3DDisassemble; f.argtypes = [c_void_p, c_size_t, c_uint, ctypes.c_char_p, POINTER(c_void_p)]; f.restype = ctypes.c_long
    data = open(path, 'rb').read(); i = data.find(b'DXBC')
    if i < 0: return None
    data = data[i:]; out = c_void_p()
    if f(data, len(data), 0, None, byref(out)) != 0: return None
    vt = ctypes.cast(ctypes.cast(out, POINTER(c_void_p))[0], POINTER(c_void_p))
    ptr = ctypes.WINFUNCTYPE(c_void_p, c_void_p)(vt[3])(out); n = ctypes.WINFUNCTYPE(c_size_t, c_void_p)(vt[4])(out)
    return ctypes.string_at(ptr, n).decode('latin1')
def uv_route(text):
    """texture name -> [uv index (0 = untransformed), 'planar' | 'mesh'] for every material texture the PS samples"""
    import re
    names = {int(m.group(2)): m.group(1) for m in re.finditer(r'^// (\w+)\s+texture\s+\S+\s+2d\s+t(\d+)\s', text, re.M)}
    tag, out = {}, {}
    for line in text.splitlines():
        s = line.strip()
        if not s or s.startswith('//') or s.startswith('dcl_') or ' ' not in s: continue
        op, rest = s.split(None, 1)
        parts = [p.strip() for p in re.split(r',\s*(?![^()]*\))', rest)]
        d = re.match(r'(r\d+)\.(\w+)', parts[0])
        if op.startswith('sample'):
            tm = re.match(r't(\d+)', parts[2]); cm = re.match(r'([rv]\d+)\.(\w\w)', parts[1])
            if tm and int(tm.group(1)) in names and cm:
                reg, sw = cm.groups()
                if sw == 'xy' and tag.get(reg, ('',))[0] == 'uv': res = list(tag[reg][1:])
                elif reg == 'v0' and sw == 'xy': res = [0, 'mesh']
                else: res = None
                if res: out.setdefault(names[int(tm.group(1))], res)
        elif d and op == 'add' and d.group(2) == 'xy':
            src = parts[1]
            tag[d.group(1)] = ('pre', 'planar' if src.startswith('r0.xw') else 'mesh' if src.startswith('v0.xy') else None)
            continue
        elif d and op == 'dp2' and d.group(2) in 'xy':
            sm = re.match(r'(r\d+)\.xyxx', parts[1]); cm = re.match(r'cb0\[(\d+)\]\.(\w)', parts[2])
            if d.group(2) == 'y' and tag.get(d.group(1), ('',))[0] == 'uv': continue
            if sm and cm and tag.get(sm.group(1), ('',))[0] == 'pre' and tag[sm.group(1)][1] and d.group(2) == 'x':
                uv = UVX.get((int(cm.group(1)), cm.group(2)))
                if uv:
                    tag[d.group(1)] = ('uv', uv, tag[sm.group(1)][1]); continue
        if d and any(c in d.group(2) for c in 'xy'):
            tag.pop(d.group(1), None)
    return out
def ps_uv_route(shader_set):
    """(route, rot) for a shader set's PS: route = uv_route(), rot = c_uvN slots the PS reads in rotation form"""
    if not shader_set: return None, set()
    if shader_set in _ps_cache: return _ps_cache[shader_set]
    r, rot = None, set()
    try:
        b = open(os.path.join(root, 'shaders', 'sets', shader_set + '.msw'), 'rb').read()
        ps = os.path.join(root, 'shaders', 'programs', '0x%X_0.fxc' % struct.unpack('<Q', b[5:13])[0])
        if os.path.exists(ps):
            t = disassemble(ps)
            if t:
                import re
                r = uv_route(t)
                # rotation form (foliage, scrolling screens): c_uvNRotScaleX = (angle, scaleU), RotScaleY = (centre as
                # two packed f16, scaleV), Translate = scroll speed: sincos(angle) instead of a 2x2 matrix
                rot = {UVX[(int(k), c)] for k, c in re.findall(r'sincos r\d+\.\w+, r\d+\.\w+, cb0\[(\d+)\]\.(\w)', t) if (int(k), c) in UVX}
    except OSError:
        pass
    _ps_cache[shader_set] = (r, rot)
    return r, rot
def rot_to_affine(tints, n):
    """rewrite c_uvN given in rotation form as the equivalent RotScaleX/Y + Translate (scroll dropped)"""
    import math
    x, y = tints.get(f'uv{n}RotScaleX'), tints.get(f'uv{n}RotScaleY')
    if not x or not y: return
    a, su, packed, sv = x[0], x[1], y[0], y[1]
    bits = struct.unpack('<I', struct.pack('<f', packed))[0]
    cx, cy = (struct.unpack('<e', struct.pack('<H', h))[0] for h in (bits >> 16, bits & 0xFFFF))
    m = [[su * math.cos(a), -su * math.sin(a)], [sv * math.sin(a), sv * math.cos(a)]]
    det = m[0][0] * m[1][1] - m[0][1] * m[1][0]
    # out = M (p - c) + c = M (p + t)  ->  t = M^-1 c - c
    inv_c = [(m[1][1] * cx - m[0][1] * cy) / det, (-m[1][0] * cx + m[0][0] * cy) / det] if abs(det) > 1e-12 else [cx, cy]
    tints[f'uv{n}RotScaleX'], tints[f'uv{n}RotScaleY'] = m[0], m[1]
    tints[f'uv{n}Translate'] = [inv_c[0] - cx, inv_c[1] - cy]
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
             'spec': ['specTexture', 'spc'], 'ao': ['aoTexture', 'ao'], 'emissive': ['emissiveTexture', 'ilm'],
             # second layer of two-layer blend materials + blend mask + layer-0 detail normal
             'albedo2': ['albedo2Texture'], 'normal2': ['normal2Texture'], 'gloss2': ['gloss2Texture'], 'spec2': ['spec2Texture'],
             'blend': ['layerBlendTexture', 'bm'], 'detail_normal': ['detailNormalTexture'],
             # detail albedo, overlaid on layer 0 / layer 1 albedo in the blend shaders (128 = neutral)
             'detail': ['detailTexture', 'det'], 'detail2': ['detail2Texture']}
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
            mt = re.search(r'float (c_\w+)(?:\[\d\])? = (\{[^}]*\}|[-0-9.e]+);', line)
            if mt:
                nm, raw = mt.group(1), mt.group(2).strip('{} ')
                vals = [float(x) for x in raw.split(',')]
                if nm == 'c_layerBlendRamp': tints['blend_ramp'] = vals[0]
                elif nm == 'c_L0_perfGlossAndL1_albedoTint': tints['perf_gloss'] = vals[0]; tints['albedo2_tint'] = vals[1:4]
                elif nm == 'c_L1_perfGlossAndL1_emissiveTint': tints['perf_gloss2'] = vals[0]
                elif nm == 'c_L1_perfSpecColorAndSplineMinPixelPercent': tints['perf_spec2'] = vals[:3]
                elif nm in ('c_L0_perfSpecColorX', 'c_L0_perfSpecColorY', 'c_L0_perfSpecColorZ'):
                    tints.setdefault('perf_spec', [0.04, 0.04, 0.04])['XYZ'.index(nm[-1])] = vals[0]
                elif re.fullmatch(r'c_uv[1-5](RotScaleX|RotScaleY|Translate)', nm):
                    tints[nm[2:]] = vals[:2]
            mt = re.search(r'uint32_t c_uberFeatureFlags = (\d+);', line)
            if mt:
                tints['feature_flags'] = int(mt.group(1))
        for line in open(up, encoding='utf-8', errors='ignore'):
            mt = re.search(r'float c_emissiveEdgeFade(Exponent|Inner|Outer) = ([-0-9.e]+);', line)
            if mt:
                tints.setdefault('emissive_edge', [0.0, 1.0, 1.0])[('Exponent', 'Inner', 'Outer').index(mt.group(1))] = float(mt.group(2))
    # old-style translucent blends (_col/_nml/_gls + an unnamed or _bm 4th slot): the 4th slot is the blend mask
    old_style = 'col' in slots
    if old_style and not canon.get('blend') and m.get('alphaMode') == 'BLEND' and 'unavailable' in slots:
        canon['blend'] = slots['unavailable']
        tints['blend_inferred'] = True   # a guessed mask (decals, sand_blend_firing_range): keeps the opacity = blend rule
    if old_style:
        tints['old_style'] = True
    # which UV each texture is sampled with, read from the material's own pixel shader
    route, rot = ps_uv_route(found.get('shaderSet'))
    for n in rot:
        rot_to_affine(tints, n)
    if route:
        tints['uv_route'] = {k: route[a] for k, alts in CANON.items() for a in alts if a in route}
    # render state the client needs: rasterizerFlags & 0x70 picks the depth bias class (0x10 = decal, docs/apex-lighting.md 9),
    # depthStencilFlags bit 0 depth test, bits 1-3 compare, bit 4 depth write
    for key, field in (('raster_flags', 'rasterizerFlags'), ('depth_flags', 'depthStencilFlags')):
        if isinstance(found.get(field), str):
            tints[key] = int(found[field], 16)
    out[name] = {'shader': found.get('shaderType'), **{k: v for k, v in canon.items() if v}, **tints, 'all': slots}
json.dump(out, open(os.path.join(mdir, 'materials.json'), 'w'), indent=1)
from collections import Counter
c = Counter(k for v in out.values() for k in v if k not in ('shader', 'all'))
print('non-default tints:', sum(1 for v in out.values() if v.get('albedo_tint', [1,1,1]) != [1,1,1] or v.get('emissive_tint')))
print(f'{len(out)} materials mapped, {len(miss)} missing: {miss[:5]}'); print('slots:', dict(c))
