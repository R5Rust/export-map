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
# R5_DXC: any dxc.exe (Windows SDK, or DirectXShaderCompiler's release); tools/install.ps1 sets it
DXC = os.environ.get("R5_DXC") or r'C:\Program Files (x86)\Windows Kits\10\bin\10.0.22621.0\x64\dxc.exe'
def dxil_text(ps_path):
    """dxc -dumpbin of a DXIL (SM6, retail Apex) program, cached in <root>/_dxc"""
    import subprocess
    cdir = os.path.join(root, '_dxc'); os.makedirs(cdir, exist_ok=True)
    out = os.path.join(cdir, os.path.basename(ps_path).rsplit('_', 1)[0] + '.txt')
    if not os.path.exists(out):
        if not os.path.exists(DXC): return None
        r = subprocess.run([DXC, '-dumpbin', ps_path], capture_output=True, text=True)
        open(out, 'w', encoding='utf-8').write(r.stdout)
    return open(out, encoding='utf-8').read()
def dxil_uv_route(text):
    """uv_route() for DXIL: each Sample's coordinates traced through the SSA. A coordinate made by
    dot2(base + c_uvNTranslate, c_uvNRotScaleX/Y) is uv N; the base is the mesh UV when it comes from input TEXCOORD0,
    else world-planar (position). Texture names via createHandle's register and the resource table."""
    import re
    reg_tex = {int(m.group(2)): m.group(1) for m in re.finditer(r'^; (\w+)\s+texture\s+\S+\s+\S+\s+T\d+\s+t(\d+)\s', text, re.M)}
    sig = []   # input signature element ids in order: (name, index)
    m = re.search(r'; Input signature:.*?-+\n(.*?)\n;\n', text, re.S)
    if m:
        for line in m.group(1).splitlines():
            p = line[1:].split()
            if len(p) >= 2 and p[1].isdigit(): sig.append((p[0], int(p[1])))
    # cbuffer field by byte offset (CBufUberStatic)
    i = text.find('struct struct.CBufUberStatic'); fields = {}
    if i >= 0:
        em = re.compile(r'\n;\s+\}').search(text, i)
        for fm in re.finditer(r';\s+\w+\s+(\w+)(?:\[\d+\])?;\s+; Offset:\s+(\d+)', text[i:em.start() if em else i + 20000]):
            fields[int(fm.group(2))] = fm.group(1)
    defs = {}
    for line in text.splitlines():
        dm = re.match(r'\s*(%\d+) = (.*)', line)
        if dm: defs[dm.group(1)] = dm.group(2)
    handle_reg = {}
    for v, d in defs.items():
        hm = re.search(r'@dx\.op\.createHandle\(i32 57, i8 0, i32 \d+, i32 (\d+)', d)
        if hm: handle_reg[v] = int(hm.group(1))
    def operands(d): return re.findall(r'%\d+', d)
    def cb_field(v):
        """%x = extractvalue CBufRet %load, comp -> field name at reg*16+comp*4"""
        d = defs.get(v, '')
        em = re.match(r'extractvalue %dx\.types\.CBufRet\.\w+ (%\d+), (\d+)', d)
        if not em: return None
        lm = re.search(r'cbufferLoadLegacy\.\w+\(i32 59, %dx\.types\.Handle %\d+, i32 (\d+)\)', defs.get(em.group(1), ''))
        if not lm: return None
        off = int(lm.group(1)) * 16 + int(em.group(2)) * 4
        best = max((o for o in fields if o <= off), default=None)
        return fields.get(best) if best is not None else None
    def base_kind(v, depth=0, seen=None):
        seen = seen if seen is not None else set()
        if depth > 12 or v in seen: return None
        seen.add(v)
        d = defs.get(v, '')
        lm = re.search(r'@dx\.op\.loadInput\.f32\(i32 4, i32 (\d+),', d)
        if lm:
            sid = int(lm.group(1))
            return 'mesh' if sid < len(sig) and sig[sid] == ('TEXCOORD', 0) else 'planar'
        for o in operands(d):
            if cb_field(o): continue
            k = base_kind(o, depth + 1, seen)
            if k: return k
        return None
    def rot_field(v):
        """rotation form (Nemesis energy mask): dot2(b, (cos a, -sin a)) with a = c_uvNRotScaleX.x, so the dot's
        constant operand is Sin / Cos (dx.op.unary 13 / 12) of the field, maybe negated"""
        d = defs.get(v, '')
        nm = re.match(r'fsub fast float -0\.000000e\+00, (%\d+)', d)
        if nm: d = defs.get(nm.group(1), '')
        sm = re.search(r'@dx\.op\.unary\.f32\(i32 1[23], float (%\d+)\)', d)
        return cb_field(sm.group(1)) if sm else None
    def coord_route(v, depth=0):
        d = defs.get(v, '')
        if depth > 8: return None
        dm = re.search(r'@dx\.op\.dot2\.f32\(i32 54, float (%\d+), float (%\d+), float (%\d+), float (%\d+)\)', d)
        if dm:
            f = cb_field(dm.group(3)) or rot_field(dm.group(3)) or rot_field(dm.group(4)) or ''
            um = re.match(r'c_uv(\d)RotScale', f)
            if um:
                return [int(um.group(1)), base_kind(dm.group(1)) or 'mesh']
        for o in operands(d):
            r = coord_route(o, depth + 1)
            if r: return r
        return None
    out = {}
    for d in defs.values():
        sm = re.search(r'@dx\.op\.sample\w*\.f32\(i32 \d+, %dx\.types\.Handle (%\d+), %dx\.types\.Handle %\d+, float (%\d+), float (%\d+)', d)
        if not sm or sm.group(1) not in handle_reg: continue
        name = reg_tex.get(handle_reg[sm.group(1)])
        if not name or name in out: continue
        r = coord_route(sm.group(2))
        if r is None:
            k = base_kind(sm.group(2))
            if k: r = [0, k]
        if r: out[name] = r
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
            if not t:
                # retail (DXIL): d3dcompiler can't read it; trace the dxc disassembly instead
                dt = dxil_text(ps)
                if dt and 'dx.op.sample' in dt:
                    r = dxil_uv_route(dt)
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
_vt_cache = {}


_bind_cache = {}


def ps_bindings(shader_set):
    """{texture register: resource name} of a shader set's PS (its resource table: albedoTexture t0, emissiveTexture t4,
    emissiveMultiplyTexture t7, albedo2Texture t1, layerBlendTexture t14, ...). A material's texture slot N is bound
    to tN, so this names every slot exactly; RSX's "_col"/"_ilm" suffixes give two slots one name (emissive +
    emissive-multiply, layer 0 + layer 1 emissive)."""
    if not shader_set:
        return {}
    if shader_set in _bind_cache:
        return _bind_cache[shader_set]
    res = {}
    try:
        import re
        b = open(os.path.join(root, 'shaders', 'sets', shader_set + '.msw'), 'rb').read()
        ps = os.path.join(root, 'shaders', 'programs', '0x%X_0.fxc' % struct.unpack('<Q', b[5:13])[0])
        if os.path.exists(ps):
            t = dxil_text(ps) or ''
            res = {int(m.group(2)): m.group(1) for m in re.finditer(r'^; (\w+)\s+texture\s+\S+\s+2d\s+T\d+\s+t(\d+)\s', t, re.M)}
    except OSError:
        pass
    _bind_cache[shader_set] = res
    return res


_rot_cache = {}


def ps_rot_uvs_dxil(shader_set):
    """c_uvN transforms a retail (DXIL) PS evaluates in rotation form: it takes sin / cos (dx.op.unary 13 / 12) of
    c_uvNRotScaleX.x. The Nemesis' emissiveMultiplyTexture: uv0 + c_uv1Translate * time, rotated by c_uv1RotScaleX.x
    about the f16 centre packed in c_uv1RotScaleY.x, scaled by RotScaleX.y / RotScaleY.y."""
    if not shader_set:
        return set()
    if shader_set in _rot_cache:
        return _rot_cache[shader_set]
    res = set()
    try:
        import re
        b = open(os.path.join(root, 'shaders', 'sets', shader_set + '.msw'), 'rb').read()
        ps = os.path.join(root, 'shaders', 'programs', '0x%X_0.fxc' % struct.unpack('<Q', b[5:13])[0])
        t = dxil_text(ps) if os.path.exists(ps) else None
        if t:
            i = t.find('struct struct.CBufUberStatic'); fields = {}
            if i >= 0:
                em = re.compile(r'\n;\s+\}').search(t, i)
                for fm in re.finditer(r';\s+\w+\s+(\w+)(?:\[\d+\])?;\s+; Offset:\s+(\d+)', t[i:em.start() if em else i + 20000]):
                    fields[int(fm.group(2))] = fm.group(1)
            defs = {}
            for line in t.splitlines():
                dm = re.match(r'\s*(%\d+) = (.*)', line)
                if dm: defs[dm.group(1)] = dm.group(2)
            # the static uber cbuffer's handle: createHandle(..., i8 2 (CBV), i32 <range>, i32 0 (cb0), ...)
            hs = {v for v, d in defs.items() if re.search(r'@dx\.op\.createHandle\(i32 57, i8 2, i32 \d+, i32 0,', d)}
            for v, d in defs.items():
                um = re.match(r'call float @dx\.op\.unary\.f32\(i32 1[23], float (%\d+)\)', d)
                if not um: continue
                em2 = re.match(r'extractvalue %dx\.types\.CBufRet\.f32 (%\d+), (\d+)', defs.get(um.group(1), ''))
                if not em2: continue
                lm = re.match(r'call %dx\.types\.CBufRet\.f32 @dx\.op\.cbufferLoadLegacy\.f32\(i32 59, %dx\.types\.Handle (%\d+), i32 (\d+)\)', defs.get(em2.group(1), ''))
                if not lm or lm.group(1) not in hs: continue
                fm2 = re.match(r'c_uv(\d)RotScaleX$', fields.get(int(lm.group(2)) * 16 + int(em2.group(2)) * 4, ''))
                if fm2: res.add(int(fm2.group(1)))
    except OSError:
        pass
    _rot_cache[shader_set] = res
    return res


def ps_vertex_tint(shader_set):
    """Whether a shader set's PS multiplies albedo by the vertex colour. Retail prop / world VSs pass
    pow(|COLOR.rgb|, 2.2) as TEXCOORD3; a PS that multiplies a texture sample by it tints albedo per vertex
    (the "*_vert_color" props: the white pipe / crate / tower paint takes its colour from the model). Read from
    the DXIL: the input-signature row of TEXCOORD 3, its loadInput values, and an fmul of one with a sample."""
    if not shader_set:
        return False
    if shader_set in _vt_cache:
        return _vt_cache[shader_set]
    res = False
    try:
        import re
        b = open(os.path.join(root, 'shaders', 'sets', shader_set + '.msw'), 'rb').read()
        ps = os.path.join(root, 'shaders', 'programs', '0x%X_0.fxc' % struct.unpack('<Q', b[5:13])[0])
        t = dxil_text(ps) if os.path.exists(ps) else None
        if t:
            sig = t[t.find('Input signature'):]
            sig = sig[:sig.find('Output signature')]
            row = None
            for m in re.finditer(r'^;\s+(\w+)\s+(\d+)\s+\w+\s+(\d+)\s+', sig, re.M):
                if m.group(1) == 'TEXCOORD' and m.group(2) == '3':
                    row = m.group(3)
            if row is not None:
                ins = set(re.findall(r'(%\d+) = call float @dx\.op\.loadInput\.f32\(i32 4, i32 ' + row + r', ', t))
                samp = set()
                for m in re.finditer(r'(%\d+) = call %dx\.types\.ResRet\.f32 @dx\.op\.sample\.f32', t):
                    samp.add(m.group(1))
                ext = set(m.group(1) for m in re.finditer(r'(%\d+) = extractvalue %dx\.types\.ResRet\.f32 (%\d+), [0-2]', t) if m.group(2) in samp)
                for m in re.finditer(r'= fmul fast float (%\d+), (%\d+)', t):
                    a, c = m.group(1), m.group(2)
                    if (a in ins and c in ext) or (c in ins and a in ext):
                        res = True
                        break
    except OSError:
        pass
    _vt_cache[shader_set] = res
    return res


def ps_reads_field(t, field):
    """Whether a DXIL listing loads a CBufUberStatic field (declared offset -> legacy register + component)."""
    import re
    m = re.search(r';\s+(?:float|uint32_t|uint|int)\d?\s+' + field + r'(?:\[\d\])?;\s+; Offset:\s+(\d+)', t)
    if not m:
        return False
    off = int(m.group(1))
    reg, comp = off // 16, (off % 16) // 4
    loads = set(re.findall(r'(%\d+) = call %dx\.types\.CBufRet\.\w+ @dx\.op\.cbufferLoadLegacy\.\w+\(i32 59, %dx\.types\.Handle %\d+, i32 ' + str(reg) + r'\)', t))
    return any(m2.group(1) in loads for m2 in re.finditer(r'= extractvalue %dx\.types\.CBufRet\.\w+ (%\d+), ' + str(comp) + r'\b', t))


_vao_cache = {}
def ps_vertex_ao(shader_set):
    """Whether a shader set's PS takes AO = vertex colour R (TEXCOORD3.x, the VS's pow(|COLOR|, 2.2)) * aoTexture.r:
    the foliage PS (tree_fir_01_leaves 0x8BFA55EB7A9AA9D: fmul(TEXCOORD3.x * 255, ao) packed into the AO byte). Read
    from the DXIL: an fmul of the aoTexture sample with TEXCOORD3.x, directly or through one constant scale."""
    if not shader_set:
        return False
    if shader_set in _vao_cache:
        return _vao_cache[shader_set]
    res = False
    try:
        import re
        b = open(os.path.join(root, 'shaders', 'sets', shader_set + '.msw'), 'rb').read()
        ps = os.path.join(root, 'shaders', 'programs', '0x%X_0.fxc' % struct.unpack('<Q', b[5:13])[0])
        t = dxil_text(ps) if os.path.exists(ps) else None
        if t and 'aoTexture' in t:
            sig = t[t.find('Input signature'):]
            sig = sig[:sig.find('Output signature')]
            row = None
            for m in re.finditer(r'^;\s+(\w+)\s+(\d+)\s+\w+\s+(\d+)\s+', sig, re.M):
                if m.group(1) == 'TEXCOORD' and m.group(2) == '3':
                    row = m.group(3)
            texs = [m.group(1) for m in re.finditer(r'^; (\w+)\s+texture\s', t, re.M)]
            if row is not None and 'aoTexture' in texs:
                ins = set(re.findall(r'(%\d+) = call float @dx\.op\.loadInput\.f32\(i32 4, i32 ' + row + r', i32 0, i8 0,', t))
                ins |= set(m.group(1) for m in re.finditer(r'(%\d+) = fmul fast float (%\d+), [-0-9.e+]+\s*$', t, re.M) if m.group(2) in ins)
                hs = set(re.findall(r'(%\d+) = call %dx\.types\.Handle @dx\.op\.createHandle\(i32 57, i8 0, i32 ' + str(texs.index('aoTexture')) + r', ', t))
                samp = set(m.group(1) for m in re.finditer(r'(%\d+) = call %dx\.types\.ResRet\.f32 @dx\.op\.sample\w*\.f32\(i32 \d+, %dx\.types\.Handle (%\d+),', t) if m.group(2) in hs)
                ext = set(m.group(1) for m in re.finditer(r'(%\d+) = extractvalue %dx\.types\.ResRet\.f32 (%\d+), 0', t) if m.group(2) in samp)
                for m in re.finditer(r'= fmul fast float (%\d+), (%\d+)', t):
                    a, c = m.group(1), m.group(2)
                    if (a in ins and c in ext) or (c in ins and a in ext):
                        res = True
                        break
    except OSError:
        pass
    _vao_cache[shader_set] = res
    return res


_af_cache = {}
def ps_alpha_terms(shader_set):
    """(samples opacityMultiplyTexture, reads c_opacity, distance fade, edge fade) for a shader set's PS. Retail uber PS
    (sand_blend_firing_range 0x2163CFE0DEAAB092, decal_grunge_tile_01 0xD351C90369EFC2E9): alpha = albedo.a * vertex
    alpha * opacityMultiplyTexture.r * diffuseModulation.w * c_opacity [* sat(dist * c_alphaDistanceFadeScale + Bias)]
    [* max(0, (1 - |N.V|)^c_alphaEdgeFadeExponent * (Outer - Inner) + Inner)]."""
    if not shader_set:
        return None
    if shader_set in _af_cache:
        return _af_cache[shader_set]
    res = None
    try:
        b = open(os.path.join(root, 'shaders', 'sets', shader_set + '.msw'), 'rb').read()
        ps = os.path.join(root, 'shaders', 'programs', '0x%X_0.fxc' % struct.unpack('<Q', b[5:13])[0])
        t = dxil_text(ps) if os.path.exists(ps) else None
        if t and 'opacityMultiplyTexture' in t:
            res = (True, ps_reads_field(t, 'c_opacity'), ps_reads_field(t, 'c_alphaDistanceFadeScale'), ps_reads_field(t, 'c_alphaEdgeFadeExponent'))
    except OSError:
        pass
    _af_cache[shader_set] = res
    return res


_tp_cache = {}
def ps_triplanar2(shader_set):
    """Whether a shader set's PS is the zUp-blend rock PS (retail rgdp 0xD7462DFDD63CF478 and kin): layer 1 is mapped
    triplanar from world position (albedo2Texture sampled 3 times, one per projection plane) and blended in by the
    surface's up-facing (c_zUpBlending*). Read from the DXIL: the sample calls on albedo2Texture's handle."""
    if not shader_set:
        return False
    if shader_set in _tp_cache:
        return _tp_cache[shader_set]
    res = False
    try:
        import re
        b = open(os.path.join(root, 'shaders', 'sets', shader_set + '.msw'), 'rb').read()
        ps = os.path.join(root, 'shaders', 'programs', '0x%X_0.fxc' % struct.unpack('<Q', b[5:13])[0])
        t = dxil_text(ps) if os.path.exists(ps) else None
        if t:
            texs = [m.group(1) for m in re.finditer(r'^; (\w+)\s+texture\s', t, re.M)]
            if 'albedo2Texture' in texs:
                rid = texs.index('albedo2Texture')
                hs = set(re.findall(r'(%\d+) = call %dx\.types\.Handle @dx\.op\.createHandle\(i32 57, i8 0, i32 ' + str(rid) + r', ', t))
                n = sum(1 for m in re.finditer(r'@dx\.op\.sample\w*\.f32\(i32 \d+, %dx\.types\.Handle (%\d+),', t) if m.group(1) in hs)
                res = n >= 3 and 'c_zUpBlendingMinAngleCos' in t
    except OSError:
        pass
    _tp_cache[shader_set] = res
    return res


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
    binds = ps_bindings(found.get('shaderSet'))
    slots = {}
    used_stems = set()
    for slot in sorted(tex, key=int):
        # the PS's own name for register t<slot> when it has one (exact role), else RSX's type suffix
        t = binds.get(int(slot)) or types.get(slot, '').lstrip('_')
        if t in slots: continue
        ref = tex[slot]
        if ref.startswith('0x'):
            # the GUID's own export (images/textures/_guid/0x<GUID>.png) is the exact texture; the sibling fallback is
            # a guess that picks variant materials (s2s_metal_trims_02_vc took _phase's blue albedo from it)
            gfile = 'images/textures/_guid/0x' + ref[2:].upper() + '.png'
            own = guid_name.get(ref[2:].lower().lstrip('0'))
            if not own and os.path.exists(os.path.join(root, gfile)):
                own = ref
            ref = own or sibling_textures(leaf, types.get(slot, '')) or ref
        stem = ref.replace(chr(92), '/').rsplit('/', 1)[-1].rsplit('.', 1)[0].lower()
        # two slots with one semantic name (nemesis emissive t4 + emissive-multiply t7 are both "_ilm"): the RSX fork
        # exports the later one as <name>_<slot> (rsx-fs material.cpp ParseMaterialTextureExportInfo, HANDOFF.md)
        if stem in used_stems and ix.get(f'{stem}_{slot}'):
            stem = f'{stem}_{slot}'
        used_stems.add(stem)
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
             'detail': ['detailTexture', 'det'], 'detail2': ['detail2Texture'],
             # emissive = emissiveTexture x emissiveMultiplyTexture x L0 emissive tint (retail uber PS); layer-1 emissive
             'emissive_mult': ['emissiveMultiplyTexture'], 'emissive2': ['emissive2Texture'], 'ao2': ['ao2Texture'],
             'opacity_mult': ['opacityMultiplyTexture'],
             'cavity': ['cavityTexture', 'cav']}
    canon = {k: next((slots[a] for a in alts if a in slots), None) for k, alts in CANON.items()}
    # an HDR texture exported as DDS next to its PNG (BC6H: the PNG export clamps it to 8 bits) wins
    for k, v in list(canon.items()):
        if v and v.endswith('.png') and os.path.exists(os.path.join(root, v[:-4] + '.dds')):
            canon[k] = v[:-4] + '.dds'
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
                elif nm in ('c_opacity', 'c_alphaEdgeFadeExponent', 'c_alphaEdgeFadeInner', 'c_alphaEdgeFadeOuter', 'c_alphaDistanceFadeScale', 'c_alphaDistanceFadeBias'):
                    tints.setdefault('alpha_consts', {})[nm[2:]] = vals[0]
                elif nm == 'c_zUpBlendingMinAngleCos': tints.setdefault('zup', [0.0, 0.0, 0.0])[0] = vals[0]
                elif nm == 'c_zUpBlendingMaxAngleCos': tints.setdefault('zup', [0.0, 0.0, 0.0])[1] = vals[0]
                elif nm == 'c_zUpBlendingVertexAlphaAndL0_albedoTint': tints.setdefault('zup', [0.0, 0.0, 0.0])[2] = vals[0]
                elif nm == 'c_L0_perfGlossAndL1_albedoTint': tints['perf_gloss'] = vals[0]; tints['albedo2_tint'] = vals[1:4]
                elif nm == 'c_L1_perfGlossAndL1_emissiveTint': tints['perf_gloss2'] = vals[0]; tints['emissive2_tint'] = vals[1:4]
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
    # retail (DXIL) rotation-form transforms stay raw: the client evaluates them like the PS does, scroll included
    # (uv = rot(a) * (uv0 + translate * time - centre) * scale + centre)
    drot = ps_rot_uvs_dxil(found.get('shaderSet'))
    if drot:
        tints['uv_rot'] = sorted(drot)
    if route:
        tints['uv_route'] = {k: route[a] for k, alts in CANON.items() for a in alts if a in route}
    if ps_vertex_tint(found.get('shaderSet')):
        tints['vertex_tint'] = True
    if ps_vertex_ao(found.get('shaderSet')):
        tints['vertex_ao'] = True
    # opacity mask + alpha fades, as the PS reads them (only those terms its DXIL loads)
    at = ps_alpha_terms(found.get('shaderSet'))
    ac = tints.pop('alpha_consts', {})
    if at and canon.get('opacity_mult'):
        tints['opacity'] = [ac.get('opacity', 1.0) if at[1] else 1.0,
                            ac.get('alphaDistanceFadeScale', 0.0) if at[2] else 0.0, ac.get('alphaDistanceFadeBias', 1.0) if at[2] else 1.0,
                            ac.get('alphaEdgeFadeExponent', 1.0), ac.get('alphaEdgeFadeInner', 1.0) if at[3] else 1.0, ac.get('alphaEdgeFadeOuter', 1.0) if at[3] else 1.0]
    # zUp rock blend: layer 1 triplanar from world position, blended by up-facing (world.wgsl zup_blend)
    if ps_triplanar2(found.get('shaderSet')):
        tints['triplanar2'] = True
    elif 'zup' in tints:
        del tints['zup']
    # unlit: the PS binds no shadow map, cubemap or light resource (the 3D skybox's HDR sky dome, rgdc
    # Skydome_HDR_*: out = albedo * c_L0_albedoTint, then fog). The client skips all lighting for these.
    # Only albedo-only PSs: unlit screens / glows also bind emissive textures and shade differently.
    lit_res = {'CSMDepthAtlasTexture', 't_shadowMaps', 'staticShadowTexture', 'IndirectSpecularTextureCubeArray', 'ssaoTexture'}
    if binds and not (set(binds.values()) & lit_res) and set(binds.values()) <= {'albedoTexture'}:
        tints['unlit'] = True
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
