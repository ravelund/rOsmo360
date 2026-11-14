// Real-time port of stitch.rs: screen ray -> user view -> stabilised rig ray
// (re-aimed per sensor row) -> both lenses -> feathered blend of NV12 pictures.

struct Lens {
    // cam_from_rig, rows.
    r0: vec4f,
    r1: vec4f,
    r2: vec4f,
    // ThetaPoly k1..k4.
    k: vec4f,
    // k5, p1, p2, skew.
    k5p: vec4f,
    // fx, fy, cx, cy.
    fc: vec4f,
    // Calibration pixels -> texture coordinates: rows (a, b, t); .w of a0 is
    // the mirror flag, .w of a1 the texture slot.
    a0: vec4f,
    a1: vec4f,
    // fov_limit, blend limit, blend width, unused.
    lim: vec4f,
    // calib width, calib height, unused, unused.
    calib: vec4f,
}

struct Uniforms {
    // world_from_screen-ray rotation of the user's free look, rows.
    user0: vec4f,
    user1: vec4f,
    user2: vec4f,
    // width, height, zoom (tan of half the vertical FOV), then the mapping:
    // 0 perspective to 1 little planet, negative for equirect.
    screen: vec4f,
    // rolling-shutter steps, 1 if the surface re-encodes to sRGB, then the
    // view's top-left corner in framebuffer pixels.
    misc: vec4f,
    lenses: array<Lens, 2>,
    // rig_from_view at instants spread over the readout, three rows each.
    views: array<vec4f, 99>,
    // Exposure matching (exposure.rs): the first lens's axis and two ring
    // directions, fade-in angles in .w of the first two; then per lens, per
    // azimuth bin, RGB gains.
    seam: array<vec4f, 3>,
    gains: array<vec4f, 64>,
    // Seam alignment shifts (align.rs), radians, four cells per vector.
    field: array<vec4f, 224>,
    // Disagreement cut strength per azimuth bin (align.rs), four per vector.
    cut: array<vec4f, 32>,
    // Colour effects (grade.rs, `Grade::uniforms`).
    grade: array<vec4f, 3>,
    // An ordinary video: .x 1 if it is one, .y its picture's width over
    // height, .z 1 if it came as 10-bit, .w HDR (1 HLG, 2 PQ); then its crop (view.rs `Crop`): centre x, y, half height, roll.
    flat: vec4f,
    crop: vec4f,
    // .x 1 to dither the last rounding to 8 bits, .y the frame's number.
    dither: vec4f,
    // The LUT (lut.rs): .x 1 if on, .y strength, .z size; its domain's low
    // and high ends.
    lut: array<vec4f, 3>,
}

const BINS: u32 = 32u;
// Must match align.rs.
const CELLS_PHI: u32 = 128u;
const CELLS_ALPHA: u32 = 7u;
const CELL_ALPHA_LO: f32 = 84.0;
const CELL_ALPHA_HI: f32 = 96.0;
const TAPER_LO: f32 = 83.0;
const TAPER_HI: f32 = 97.0;
const TAPER_WIDTH: f32 = 12.0;

@group(0) @binding(0) var<uniform> u: Uniforms;
@group(0) @binding(1) var samp: sampler;
@group(0) @binding(2) var y0: texture_2d<f32>;
@group(0) @binding(3) var uv0: texture_2d<f32>;
@group(0) @binding(4) var y1: texture_2d<f32>;
@group(0) @binding(5) var uv1: texture_2d<f32>;
@group(0) @binding(6) var mask0: texture_2d<f32>;
@group(0) @binding(7) var mask1: texture_2d<f32>;
// A 10-bit (P010) picture's planes, per slot: 16-bit integers, the 10 bits
// in the high ones (see deep_y).
@group(0) @binding(8) var y0d: texture_2d<u32>;
@group(0) @binding(9) var uv0d: texture_2d<u32>;
@group(0) @binding(10) var y1d: texture_2d<u32>;
@group(0) @binding(11) var uv1d: texture_2d<u32>;
@group(0) @binding(12) var lut3d: texture_3d<f32>;
// The fine seam field (align.rs `flow`), a shift per texel.
@group(0) @binding(13) var flow: texture_2d<f32>;

const PI: f32 = 3.14159265358979;

@vertex
fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4f {
    let p = vec2f(f32((i << 1u) & 2u), f32(i & 2u));
    return vec4f(p * 2.0 - 1.0, 0.0, 1.0);
}

fn rows(a: vec4f, b: vec4f, c: vec4f, v: vec3f) -> vec3f {
    return vec3f(dot(a.xyz, v), dot(b.xyz, v), dot(c.xyz, v));
}

fn view_at(i: u32, v: vec3f) -> vec3f {
    return rows(u.views[i * 3u], u.views[i * 3u + 1u], u.views[i * 3u + 2u], v);
}

struct Hit {
    // Texture coordinates, then calibration-space row fraction.
    uv: vec2f,
    row: f32,
    theta: f32,
    // How far inside the masks (stitch.rs `edge`), scaling the blend weight.
    edge: f32,
    ok: bool,
}

fn mask_at(i: u32, p: vec2i) -> f32 {
    if i == 0u {
        return textureLoad(mask0, p, 0).r;
    }
    return textureLoad(mask1, p, 0).r;
}

fn project(i: u32, d: vec3f) -> Hit {
    let l = u.lenses[i];
    var h: Hit;
    h.ok = false;
    let c = rows(l.r0, l.r1, l.r2, d);
    let rho = length(c.xy);
    let theta = atan2(rho, c.z);
    h.theta = theta;
    if theta > l.lim.x {
        return h;
    }
    let t2 = theta * theta;
    let td = theta * (1.0 + t2 * (l.k.x + t2 * (l.k.y + t2 * (l.k.z + t2 * (l.k.w + t2 * l.k5p.x)))));
    var x = 0.0;
    var y = 0.0;
    if rho > 1e-9 {
        x = td * c.x / rho;
        y = td * c.y / rho;
    }
    let td2 = td * td;
    let p1 = l.k5p.y;
    let p2 = l.k5p.z;
    let xd = x + 2.0 * p1 * x * y + p2 * (td2 + 2.0 * x * x);
    let yd = y + p1 * (td2 + 2.0 * y * y) + 2.0 * p2 * x * y;
    var cu = l.fc.x * xd + l.k5p.w * yd + l.fc.z;
    let cv = l.fc.y * yd + l.fc.w;
    if l.a0.w > 0.5 {
        cu = 2.0 * l.fc.z - cu;
    }
    if cu < 0.0 || cv < 0.0 || cu >= l.calib.x || cv >= l.calib.y {
        return h;
    }
    h.edge = mask_at(i, vec2i(i32(cu), i32(cv)));
    if h.edge <= 0.0 {
        return h;
    }
    h.uv = vec2f(dot(l.a0.xy, vec2f(cu, cv)) + l.a0.z, dot(l.a1.xy, vec2f(cu, cv)) + l.a1.z);
    h.row = cv / l.calib.y;
    h.ok = true;
    return h;
}

// A 16-bit integer plane at `uv`, filtered by hand as the sampler would
// (texel centres at +0.5, clamped at the edges): integer textures can't be
// filtered. On the 8-bit planes' scale, so the same limited-range constants
// hold: 10-bit 64 and 940 land where 8-bit 16 and 235 do (/ 65280).
fn deep_at(t: texture_2d<u32>, uv: vec2f) -> vec2f {
    let size = vec2i(textureDimensions(t));
    let p = uv * vec2f(size) - 0.5;
    let i = vec2i(floor(p));
    let f = p - floor(p);
    let a = mix(deep_load(t, i, size), deep_load(t, i + vec2i(1, 0), size), f.x);
    let b = mix(deep_load(t, i + vec2i(0, 1), size), deep_load(t, i + vec2i(1, 1), size), f.x);
    return mix(a, b, f.y) / 65280.0;
}

fn deep_load(t: texture_2d<u32>, p: vec2i, size: vec2i) -> vec2f {
    return vec2f(textureLoad(t, clamp(p, vec2i(0), size - 1), 0).rg);
}

// The picture in `slot` at `uv` as Y′ (0..1) and Cb, Cr (−0.5..0.5), from
// limited range: slots 0 and 1 are 8-bit NV12, 2 and 3 the same slots'
// 10-bit planes.
fn ycc(slot: f32, uv: vec2f) -> vec3f {
    var yy: f32;
    var c: vec2f;
    if slot > 2.5 {
        yy = deep_at(y1d, uv).x;
        c = deep_at(uv1d, uv);
    } else if slot > 1.5 {
        yy = deep_at(y0d, uv).x;
        c = deep_at(uv0d, uv);
    } else if slot < 0.5 {
        yy = textureSampleLevel(y0, samp, uv, 0.0).r;
        c = textureSampleLevel(uv0, samp, uv, 0.0).rg;
    } else {
        yy = textureSampleLevel(y1, samp, uv, 0.0).r;
        c = textureSampleLevel(uv1, samp, uv, 0.0).rg;
    }
    return vec3f((yy - 16.0 / 255.0) * (255.0 / 219.0), (c.x - 128.0 / 255.0) * (255.0 / 224.0), (c.y - 128.0 / 255.0) * (255.0 / 224.0));
}

// BT.709 Y′CbCr as display RGB.
fn nv12(slot: f32, uv: vec2f) -> vec3f {
    let c = ycc(slot, uv);
    return clamp(vec3f(c.x + 1.5748 * c.z, c.x - 0.1873 * c.y - 0.4681 * c.z, c.x + 1.8556 * c.y), vec3f(0.0), vec3f(1.0));
}

// Mirrors hdr.rs (`bt2020_rgb`, `to_sdr`): a BT.2020 HDR signal (`hdr` 1
// HLG, 2 PQ) as display-encoded sRGB BT.709, as a 1000-nit display shows
// it, Hable's curve on the brightest channel, 100 nits as white.
fn hdr_to_sdr(c: vec3f, hdr: f32) -> vec3f {
    let e = clamp(vec3f(c.x + 1.4746 * c.z, c.x - 0.16455 * c.y - 0.57135 * c.z, c.x + 1.8814 * c.y), vec3f(0.0), vec3f(1.0));
    var nits: vec3f;
    if hdr < 1.5 {
        let a = 0.17883277;
        let b = 0.28466892;
        let cc = 0.5599107;
        let scene = select((exp((e - cc) / a) + b) / 12.0, e * e / 3.0, e <= vec3f(0.5));
        let ys = dot(scene, vec3f(0.2627, 0.6780, 0.0593));
        nits = 1000.0 * pow(max(ys, 1e-6), 0.2) * scene;
    } else {
        let p = pow(e, vec3f(1.0 / 78.84375));
        nits = 10000.0 * pow(max(p - 0.8359375, vec3f(0.0)) / (18.851563 - 18.6875 * p), vec3f(1.0 / 0.15930176));
    }
    let l = nits / 100.0;
    let lin = max(vec3f(
        1.6605 * l.x - 0.5876 * l.y - 0.0728 * l.z,
        -0.1246 * l.x + 1.1329 * l.y - 0.0083 * l.z,
        -0.0182 * l.x - 0.1006 * l.y + 1.1187 * l.z,
    ), vec3f(0.0));
    let sig = max(max(lin.x, lin.y), max(lin.z, 1e-6));
    let v = clamp(lin * (hable(sig) / hable(10.0) / sig), vec3f(0.0), vec3f(1.0));
    return select(1.055 * pow(v, vec3f(1.0 / 2.4)) - 0.055, 12.92 * v, v <= vec3f(0.0031308));
}

fn hable(x: f32) -> f32 {
    return (x * (0.15 * x + 0.05) + 0.004) / (x * (0.15 * x + 0.5) + 0.06) - 0.02 / 0.3;
}

// Matches Camera::ray in view.rs. The azimuthal mappings put the ray at θ
// from the view axis where s = sin θ / (d + cos θ): d = 0 is perspective,
// d = 1 the little planet (stereographic), and between them a morph.
fn screen_ray(pos: vec2f) -> vec3f {
    let aspect = u.screen.x / u.screen.y;
    let ndc = vec2f(pos.x / u.screen.x * 2.0 - 1.0, pos.y / u.screen.y * 2.0 - 1.0);
    let d = u.screen.w;
    if d < 0.0 {
        // The whole sphere, flat.
        let lon = ndc.x * PI;
        let lat = -ndc.y * PI / 2.0;
        return vec3f(cos(lat) * sin(lon), -sin(lat), cos(lat) * cos(lon));
    }
    let q = vec2f(ndc.x * aspect, ndc.y) * u.screen.z;
    let s = max(length(q), 1e-9);
    let th = atan(s) + asin(clamp(s * d / sqrt(1.0 + s * s), -1.0, 1.0));
    return vec3f(sin(th) * q / s, cos(th));
}

fn field_cell(x: i32, y: u32) -> f32 {
    let c = min(y, CELLS_ALPHA - 1u) * CELLS_PHI + u32((x + i32(CELLS_PHI)) % i32(CELLS_PHI));
    return u.field[c / 4u][c % 4u];
}

fn flow_cell(x: i32, y: i32, size: vec2i) -> f32 {
    return textureLoad(flow, vec2i((x + size.x) % size.x, min(y, size.y - 1)), 0).r;
}

// The shift at (phi, a degrees): Field::at in align.rs, on the coarse cells
// or the fine field's texture.
fn field_at(phi: f32, a: f32) -> f32 {
    let fine = u.seam[2].w > 0.5;
    let size = select(vec2i(i32(CELLS_PHI), i32(CELLS_ALPHA)), vec2i(textureDimensions(flow)), fine);
    let x = fract(phi / (2.0 * PI) + 1.0) * f32(size.x) - 0.5;
    let y = clamp((a - CELL_ALPHA_LO) / (CELL_ALPHA_HI - CELL_ALPHA_LO) * f32(size.y - 1), 0.0, f32(size.y - 1));
    let x0 = floor(x);
    let y0 = floor(y);
    let fx = x - x0;
    let fy = y - y0;
    let xi = i32(x0);
    let yi = i32(y0);
    if fine {
        let top = mix(flow_cell(xi, yi, size), flow_cell(xi + 1, yi, size), fx);
        let bot = mix(flow_cell(xi, yi + 1, size), flow_cell(xi + 1, yi + 1, size), fx);
        return mix(top, bot, fy);
    }
    let top = mix(field_cell(xi, u32(yi)), field_cell(xi + 1, u32(yi)), fx);
    let bot = mix(field_cell(xi, u32(yi) + 1u), field_cell(xi + 1, u32(yi) + 1u), fx);
    return mix(top, bot, fy);
}

// Matches Aligner::warp in align.rs: where lens `lens` looks for output
// direction `d`.
fn align_warp(lens: u32, d: vec3f) -> vec3f {
    let axis = u.seam[0].xyz;
    let ca = clamp(dot(d, axis), -1.0, 1.0);
    let alpha = acos(ca);
    let a = degrees(alpha);
    let taper = max(1.0 - max(max(TAPER_LO - a, a - TAPER_HI), 0.0) / TAPER_WIDTH, 0.0);
    if taper <= 0.0 {
        return d;
    }
    let phi = atan2(dot(d, u.seam[2].xyz), dot(d, u.seam[1].xyz));
    let half = 0.5 * field_at(phi, a) * taper;
    let shifted = select(alpha + half, alpha - half, lens == 0u);
    let ring = vec3f(cos(phi) * u.seam[1].xyz + sin(phi) * u.seam[2].xyz);
    return cos(shifted) * axis + sin(shifted) * ring;
}

// Matches stitch::cut_share: where the lenses still disagree after
// alignment, cut to the lens that owns that side of the seam.
const CUT_FROM: f32 = 0.06;
const CUT_FULL: f32 = 0.18;
const CUT_EDGE: f32 = 0.08;

fn cut_bin(i: i32) -> f32 {
    let b = u32((i + i32(CELLS_PHI)) % i32(CELLS_PHI));
    return u.cut[b / 4u][b % 4u];
}

// Matches Field::cut_at: how far the cut may act at direction `d`.
fn cut_strength(d: vec3f) -> f32 {
    let phi = atan2(dot(d, u.seam[2].xyz), dot(d, u.seam[1].xyz));
    let x = fract(phi / (2.0 * PI) + 1.0) * f32(CELLS_PHI) - 0.5;
    let i = floor(x);
    return mix(cut_bin(i32(i)), cut_bin(i32(i) + 1), x - i);
}

fn cut_share(share: f32, a: vec3f, b: vec3f, strength: f32) -> f32 {
    let d = abs(a - b);
    let diff = max(d.x, max(d.y, d.z));
    let cut = smoothstep(0.5 - CUT_EDGE, 0.5 + CUT_EDGE, share);
    return mix(share, cut, smoothstep(CUT_FROM, CUT_FULL, diff) * strength);
}

// Matches Seam::gain in exposure.rs.
fn exposure_gain(lens: u32, d: vec3f, theta: f32) -> vec3f {
    let phi = atan2(dot(d, u.seam[2].xyz), dot(d, u.seam[1].xyz));
    let x = fract(phi / (2.0 * PI) + 1.0) * f32(BINS) - 0.5;
    let i = floor(x);
    let f = x - i;
    let b0 = u32(i32(i) + i32(BINS)) % BINS;
    let b1 = (b0 + 1u) % BINS;
    let g = mix(u.gains[lens * BINS + b0].rgb, u.gains[lens * BINS + b1].rgb, f);
    let fade = smoothstep(u.seam[0].w, u.seam[1].w, theta);
    return mix(vec3f(1.0), g, fade);
}

// The colour effects (`grade_by` in grade.wgsl, appended to this source)
// with this picture's values.
fn grade(rgb: vec3f, at: vec2f, aspect: f32) -> vec3f {
    return grade_by(u.grade[0], u.grade[1], u.grade[2], rgb, at, aspect);
}

// Mirrors `Camera::flat_uv` in view.rs: where a screen pixel falls in an
// ordinary video's picture, outside 0..1 off its edge.
fn flat_uv(pos: vec2f) -> vec2f {
    let ndc = vec2f(pos.x / u.screen.x * 2.0 - 1.0, pos.y / u.screen.y * 2.0 - 1.0);
    let q = vec2f(ndc.x * u.screen.x / u.screen.y * u.crop.z, ndc.y * u.crop.z);
    let s = sin(u.crop.w);
    let k = cos(u.crop.w);
    let p = vec2f(k * q.x - s * q.y, s * q.x + k * q.y);
    return vec2f(0.5 + (u.crop.x + p.x) / u.flat.y, 0.5 + u.crop.y + p.y);
}

@fragment
fn fs(@builtin(position) pos: vec4f) -> @location(0) vec4f {
    if u.flat.x > 0.5 {
        let uv = flat_uv(pos.xy - u.misc.zw);
        if any(uv < vec2f(0.0)) || any(uv > vec2f(1.0)) {
            return vec4f(0.0, 0.0, 0.0, 1.0);
        }
        let slot = 2.0 * u.flat.z;
        if u.flat.w > 0.5 {
            return finish(hdr_to_sdr(ycc(slot, uv), u.flat.w), pos);
        }
        return finish(nv12(slot, uv), pos);
    }
    let v = rows(u.user0, u.user1, u.user2, screen_ray(pos.xy - u.misc.zw));
    let steps = u32(u.misc.x);
    let mid = view_at(steps / 2u, v);
    var col = array<vec3f, 2>(vec3f(0.0), vec3f(0.0));
    var wt = array<f32, 2>(0.0, 0.0);
    for (var i = 0u; i < 2u; i++) {
        var h = project(i, align_warp(i, mid));
        if !h.ok {
            continue;
        }
        if steps > 1u {
            let k = u32(round(clamp(h.row, 0.0, 1.0) * f32(steps - 1u)));
            h = project(i, align_warp(i, view_at(k, v)));
            if !h.ok {
                continue;
            }
        }
        let l = u.lenses[i];
        let w = clamp((l.lim.y - h.theta) / max(l.lim.z, 1e-6), 0.0, 1.0) * h.edge;
        if w <= 0.0 {
            continue;
        }
        col[i] = nv12(l.a1.w, h.uv) * exposure_gain(i, mid, h.theta);
        wt[i] = w;
    }
    let wsum = wt[0] + wt[1];
    if wsum <= 0.0 {
        return vec4f(0.0, 0.0, 0.0, 1.0);
    }
    var share = wt[0] / wsum;
    if wt[0] > 0.0 && wt[1] > 0.0 {
        share = cut_share(share, col[0], col[1], cut_strength(mid));
    }
    return finish(col[0] * share + col[1] * (1.0 - share), pos);
}

// The colour effects, then light as the surface wants it.
// Two uniform numbers in 0..1 from a pixel and a frame (Jarzynski and
// Olano's pcg3d hash).
fn noise(p: vec3u) -> vec2f {
    var v = p * 1664525u + 1013904223u;
    v.x += v.y * v.z;
    v.y += v.z * v.x;
    v.z += v.x * v.y;
    v ^= v >> vec3u(16u);
    v.x += v.y * v.z;
    v.y += v.z * v.x;
    v.z += v.x * v.y;
    return vec2f(v.xy) / 4294967296.0;
}

// The LUT, mirroring `Lut::blend`: the hardware's trilinear filtering
// between texel centres is `Lut::apply`'s interpolation.
fn apply_lut(c: vec3f) -> vec3f {
    if u.lut[0].x < 0.5 {
        return c;
    }
    let n = u.lut[0].z;
    let x = clamp((c - u.lut[1].xyz) / (u.lut[2].xyz - u.lut[1].xyz), vec3f(0.0), vec3f(1.0));
    let to = textureSampleLevel(lut3d, samp, (x * (n - 1.0) + 0.5) / n, 0.0).rgb;
    return c + (to - c) * u.lut[0].y;
}

fn finish(picture: vec3f, pos: vec4f) -> vec4f {
    var rgb = apply_lut(picture);
    if u.grade[2].z > 0.5 {
        let p = pos.xy - u.misc.zw;
        rgb = grade(rgb, vec2f(p.x / u.screen.x * 2.0 - 1.0, p.y / u.screen.y * 2.0 - 1.0), u.screen.x / u.screen.y);
    }
    if u.dither.x > 0.5 {
        // Triangular noise of ±1 level, the same on each channel (grain in
        // brightness, not colour speckle), new each frame: the 8-bit target
        // rounds a smooth sky into steps without it.
        let n = noise(vec3u(vec2u(pos.xy), u32(u.dither.y)));
        rgb += vec3f((n.x + n.y - 1.0) / 255.0);
    }
    if u.misc.y > 0.5 {
        // The surface will sRGB-encode; hand it linear light.
        rgb = select(pow((rgb + 0.055) / 1.055, vec3f(2.4)), rgb / 12.92, rgb <= vec3f(0.04045));
    }
    return vec4f(rgb, 1.0);
}
