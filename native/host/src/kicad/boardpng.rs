//! `board_render`: a board or schematic file rendered to PNG in one MCP tool call, on the hub's own
//! plotter (plotgen.rs). No KiCad runs, no window opens: the file's bytes are read, plotted into the
//! same chunks the Board and Schematic viewers draw (their colours and layer order), and painted by a
//! small rasterizer here (capsules, dots, even-odd fills, 2x2 supersampling over the dark view ground,
//! assets/web/style.css `--view`). The PNG is pure Rust: zlib through flate2 (already in the tree), a
//! CRC32 table, three chunks. An agent answers with the file's absolute path, which it can Read and
//! write back to show the person — the viewer build agents used to make by hand (a 176-line PIL
//! renderer in the 2026-11-23 run, about 19.5 M cache-read tokens of Bash/Read round trips).
//! (kicad/render.rs is the 3D part renders' command; this is the 2D tool.)
use super::canonical as kc;
use super::plot::Plot;
use super::plotgen;
use serde_json::{json, Value};

/// One chunk's geometry, decoded once (the client's Chunk::decode, without the pieces): strokes as
/// capsules (ax ay bx by r, micrometres; a dot is a == b, the radius its circle), fills as rings.
struct Geo { color: u32, alpha: f32, caps: Vec<[f32; 5]>, polys: Vec<Vec<f32>> }

struct Varints<'a> { b: &'a [u8], i: usize, ok: bool }
impl Varints<'_> {
    fn next(&mut self) -> u32 {
        let mut acc = 0u32;
        for k in 0..5 {
            let Some(&v) = self.b.get(self.i) else { self.ok = false; return 0 };
            self.i += 1;
            let shift = 7 * k;
            if shift >= 28 && v & 0xF0 != 0 { self.ok = false; return 0 }
            acc |= ((v & 127) as u32) << shift;
            if v < 128 { return acc }
        }
        self.ok = false; 0
    }
    fn signed(&mut self) -> i32 { let z = self.next(); ((z >> 1) as i32) ^ -((z & 1) as i32) }
}

/// The client's decode (screens/plot.rs `Chunk::decode`): delta-coded micrometres, one piece at a
/// time, mode 0 fill (a tint chunk's width packs its own colour: board::tint_code), 1 stroke, 2 dot.
fn decode(c: &super::plot::Chunk) -> Geo {
    let tint = c.mode == 0 && c.width != 0;
    let (mut color, mut alpha) = (c.color, c.alpha as f32 / 255.);
    if tint { color = super::board::tint_color(c.width); alpha = super::board::tint_alpha(c.width) as f32 / 255.; }
    let mut g = Geo { color, alpha, caps: vec![], polys: vec![] };
    let mut v = Varints { b: &c.d, i: 0, ok: true };
    let (mut px, mut py) = (0i32, 0i32);
    let mut point = |v: &mut Varints| { px = px.wrapping_add(v.signed()); py = py.wrapping_add(v.signed()); (px as f32, py as f32) };
    while v.ok && v.i < c.d.len() {
        let _info = v.next();
        if c.mode == 2 { let (x, y) = point(&mut v); g.caps.push([x, y, x, y, c.width as f32 / 2.]); }
        else {
            let n = v.next() as usize;
            let mut pts: Vec<f32> = Vec::with_capacity(n.min(1 << 20) * 2);
            for _ in 0..n.min(1 << 20) { if !v.ok || v.i >= c.d.len() { break } let (x, y) = point(&mut v); pts.extend([x, y]); }
            if c.mode == 0 { if pts.len() >= 6 { g.polys.push(pts) } }
            else if pts.len() == 2 { g.caps.push([pts[0], pts[1], pts[0], pts[1], c.width as f32 / 2.]) }
            else { let mut k = 2; while k + 1 < pts.len() { g.caps.push([pts[k - 2], pts[k - 1], pts[k], pts[k + 1], c.width as f32 / 2.]); k += 2; } }
        }
    }
    g
}

/// The plot's drawing bounds (signed: the wire's u32 is two's complement), as the tool's default window.
fn bounds(r: &super::plot::Rect) -> [f64; 4] { [r.x0 as i32 as f64 / 1000., r.y0 as i32 as f64 / 1000., r.x1 as i32 as f64 / 1000., r.y1 as i32 as f64 / 1000.] }

/// The pixels of `plot` in `window` (mm, x0 y0 x1 y1; y grows downward, as the files' coordinates do),
/// `width` px on the window's long side (at most 2400): RGBA, row 0 the top.
pub fn pixels(plot: &Plot, window: [f64; 4], width: u32, off: &[u32]) -> (Vec<u8>, u32, u32) {
    let (w_mm, h_mm) = ((window[2] - window[0]).max(1e-6), (window[3] - window[1]).max(1e-6));
    let width = width.clamp(64, 2400);
    let (mut w, mut h) = if w_mm >= h_mm { (width, ((width as f64 / w_mm) * h_mm).round() as u32) } else { (((width as f64 / h_mm) * w_mm).round() as u32, width) };
    (w, h) = (w.max(8), h.max(8));
    let scale = w as f64 / w_mm;
    let geo: Vec<Geo> = plot.cs.iter().filter(|c| !off.contains(&c.layer)).map(decode).collect();
    let to_px = |x: f64, y: f64| ((x - window[0]) * scale, (y - window[1]) * scale);
    let mut img = vec![0u8; (w as usize * h as usize * 4).max(4)];
    let ground: [u8; 3] = [0x08, 0x09, 0x0b]; // assets/web/style.css --view, dark
    for px in img.chunks_exact_mut(4) { px[..3].copy_from_slice(&ground); px[3] = 255; }
    const SS: [[f32; 2]; 4] = [[0.25, 0.25], [0.75, 0.25], [0.25, 0.75], [0.75, 0.75]];
    let over = |img: &mut [u8], x: usize, y: usize, c: [u8; 3], a: f32| {
        let p = &mut img[(y * w as usize + x) * 4..(y * w as usize + x) * 4 + 4];
        for k in 0..3 { p[k] = (c[k] as f32 * a + p[k] as f32 * (1. - a)).round() as u8 }
    };
    for g in &geo {
        let col = [(g.color >> 16 & 255) as u8, (g.color >> 8 & 255) as u8, (g.color & 255) as u8];
        for cap in &g.caps {
            let (ax, ay) = to_px(cap[0] as f64 / 1000., cap[1] as f64 / 1000.);
            let (bx, by) = to_px(cap[2] as f64 / 1000., cap[3] as f64 / 1000.);
            let r = (cap[4] as f64 / 1000. * scale).max(0.5);
            let (x0, y0, x1, y1) = (ax.min(bx) - r, ay.min(by) - r, ax.max(bx) + r, ay.max(by) + r);
            let (ix0, iy0, ix1, iy1) = (x0.floor().max(0.) as i64, y0.floor().max(0.) as i64, (x1.ceil() as i64).min(w as i64 - 1), (y1.ceil() as i64).min(h as i64 - 1));
            if ix0 > ix1 || iy0 > iy1 { continue }
            for y in iy0..=iy1 { for x in ix0..=ix1 {
                let mut hit = 0u32;
                for s in SS { let (sx, sy) = (x as f32 + s[0], y as f32 + s[1]);
                    let (dx, dy) = (sx - ax as f32, sy - ay as f32);
                    let (ex, ey) = (bx as f32 - ax as f32, by as f32 - ay as f32);
                    let t = if ex == 0. && ey == 0. { 0. } else { ((dx * ex + dy * ey) / (ex * ex + ey * ey)).clamp(0., 1.) };
                    let ddx = dx - t * ex; let ddy = dy - t * ey;
                    if ddx * ddx + ddy * ddy <= (r as f32) * (r as f32) { hit += 1 }
                }
                if hit > 0 { over(&mut img, x as usize, y as usize, col, g.alpha * hit as f32 / 4.) }
            } }
        }
        for poly in &g.polys {
            let n = poly.len() / 2;
            let xs: Vec<f64> = (0..n).map(|i| to_px(poly[2 * i] as f64 / 1000., 0.).0).collect();
            let ys: Vec<f64> = (0..n).map(|i| to_px(0., poly[2 * i + 1] as f64 / 1000.).1).collect();
            let (x0, x1) = (xs.iter().cloned().fold(f64::MAX, f64::min), xs.iter().cloned().fold(f64::MIN, f64::max));
            let (y0, y1) = (ys.iter().cloned().fold(f64::MAX, f64::min), ys.iter().cloned().fold(f64::MIN, f64::max));
            let (ix0, iy0, ix1, iy1) = (x0.floor().max(0.) as i64, y0.floor().max(0.) as i64, (x1.ceil() as i64).min(w as i64 - 1), (y1.ceil() as i64).min(h as i64 - 1));
            if ix0 > ix1 || iy0 > iy1 { continue }
            for y in iy0..=iy1 { for x in ix0..=ix1 {
                let mut hit = 0u32;
                for s in SS {
                    let (sx, sy) = (x as f64 + s[0] as f64, y as f64 + s[1] as f64);
                    let mut inside = false; let mut j = n - 1;
                    for i in 0..n {
                        if (ys[i] > sy) != (ys[j] > sy) && sx < (xs[j] - xs[i]) * (sy - ys[i]) / (ys[j] - ys[i]) + xs[i] { inside = !inside }
                        j = i;
                    }
                    if inside { hit += 1 } // (even-odd: a ring's hole, drawn as its own poly, is painted over either way)
                }
                if hit > 0 { over(&mut img, x as usize, y as usize, col, g.alpha * hit as f32 / 4.) }
            } }
        }
    }
    (img, w, h)
}

// ---- PNG (RGBA8, filter 0) --------------------------------------------------------------------------------------------------

fn crc32(data: &[u8]) -> u32 {
    let mut t = [0u32; 256];
    for (i, e) in t.iter_mut().enumerate() { let mut c = i as u32; for _ in 0..8 { c = if c & 1 != 0 { 0xEDB88320 ^ (c >> 1) } else { c >> 1 } } *e = c }
    let mut c = 0xFFFFFFFFu32;
    for &b in data { c = t[((c ^ b as u32) & 255) as usize] ^ (c >> 8) }
    !c
}
fn chunk(out: &mut Vec<u8>, ty: &[u8; 4], data: &[u8]) {
    out.extend((data.len() as u32).to_be_bytes()); out.extend(ty); out.extend(data); out.extend(crc32(&[ty.as_slice(), data].concat()).to_be_bytes());
}
/// The PNG bytes of an RGBA image, row 0 first.
pub fn png(img: &[u8], w: u32, h: u32) -> Vec<u8> {
    let mut o = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    let mut ihdr = Vec::new(); ihdr.extend(w.to_be_bytes()); ihdr.extend(h.to_be_bytes()); ihdr.extend([8, 6, 0, 0, 0]);
    chunk(&mut o, b"IHDR", &ihdr);
    let mut raw = Vec::with_capacity(img.len() + h as usize);
    for y in 0..h as usize { raw.push(0); raw.extend(&img[y * w as usize * 4..(y + 1) * w as usize * 4]); }
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    use std::io::Write; let _ = z.write_all(&raw);
    let idat = z.finish().unwrap_or_default();
    chunk(&mut o, b"IDAT", &idat);
    chunk(&mut o, b"IEND", &[]);
    o
}

// ---- the tool ---------------------------------------------------------------------------------------------------------------

pub const MAX_PIXELS: u64 = 2400 * 2400;

/// The tool as an agent is shown it.
pub fn def() -> Value {
    json!({"name": "board_render", "description": "Render a KiCad board or schematic to a PNG image in one call, as Backplane's Board and Schematic tabs draw it (the saved file; no KiCad needed, nothing opens). Answers with the PNG's absolute path: Read it to inspect your work, or write the path bare in a reply so the person sees the picture. Use it to check placement, routing and silkscreen after edits, or to look at a region before changing it.\n\
Window: [x0, y0, x1, y1] in millimetres of the file's own coordinates (left, top, right, bottom; y grows downward); left out: the whole drawing fitted. Layers: \"off\" hides layer numbers (the Board/Schematic tabs' Layers menu numbers: a board's 0 is F.Cu, 1 B.Cu, 31 F.SilkS; a schematic's are its sheet's), \"dim\" draws them at a quarter.",
        "inputSchema": {"type": "object", "properties": {
            "path": {"type": "string", "description": "Absolute path of the .kicad_pcb or .kicad_sch (the saved file is what renders)."},
            "window": {"type": "array", "items": {"type": "number"}, "minItems": 4, "maxItems": 4, "description": "[x0, y0, x1, y1] in mm to render instead of the whole drawing."},
            "off": {"type": "array", "items": {"type": "integer"}, "description": "Layer numbers to hide."},
            "dim": {"type": "array", "items": {"type": "integer"}, "description": "Layer numbers to draw at a quarter strength."},
            "width": {"type": "integer", "description": "Image width in px for the window's long side (default 1024, at most 2400)."}},
            "required": ["path"]}})
}

fn fail(m: &str) -> (bool, Value) { (false, json!({"error": m})) }

impl crate::agents::Agents {
    /// The MCP tool `board_render`.
    pub async fn tool_board_render(self: &std::sync::Arc<Self>, _thread: &str, a: &Value) -> (bool, Value) {
        let path = a["path"].as_str().unwrap_or("");
        if !crate::plat::fs::is_abs(path) { return fail("path must be the absolute path of the .kicad_pcb or .kicad_sch to render") }
        let board = path.ends_with(".kicad_pcb");
        if !board && !path.ends_with(".kicad_sch") { return fail("path must be a .kicad_pcb or a .kicad_sch") }
        let routed = kc::route(path);
        let bytes = match tokio::task::spawn_blocking({ let p = routed.clone(); move || std::fs::read(p) }).await {
            Ok(Ok(b)) => b,
            Ok(Err(e)) => return fail(&format!("could not read {routed}: {e}")),
            Err(e) => return fail(&format!("could not read {routed}: {e}")),
        };
        let (plot, _) = match plotgen::of_bytes(if board { 0 } else { 1 }, &bytes, false) {
            plotgen::Read::Plot(p, t) => (*p, t),
            plotgen::Read::Half => return fail("the file is being saved; ask again in a moment"),
            plotgen::Read::Fail(why) => return fail(&format!("the file could not be read as a KiCad design: {}", why.trim_start_matches("@bp.file-missing:"))),
        };
        let whole = bounds(&plot.b);
        let window: [f64; 4] = match a["window"].as_array() {
            Some(w) if w.len() == 4 => { let g = |i: usize| w[i].as_f64().unwrap_or(0.); [g(0), g(1), g(2), g(3)] }
            _ => whole,
        };
        if window[2] <= window[0] || window[3] <= window[1] { return fail("window is [x0, y0, x1, y1] with x1 > x0 and y1 > y0, in mm") }
        let width = a["width"].as_u64().unwrap_or(1024).min(2400) as u32;
        let off: Vec<u32> = a["off"].as_array().map(|v| v.iter().filter_map(|x| x.as_u64().map(|n| n as u32)).collect()).unwrap_or_default();
        let dim: Vec<u32> = a["dim"].as_array().map(|v| v.iter().filter_map(|x| x.as_u64().map(|n| n as u32)).collect()).unwrap_or_default();
        let (img, w, h) = if dim.is_empty() { pixels(&plot, window, width, &off) }
        else {
            let mut p2 = plot.clone();
            for c in p2.cs.iter_mut().filter(|c| dim.contains(&c.layer)) { c.alpha = c.alpha * 64 / 255; }
            pixels(&p2, window, width, &off)
        };
        if w as u64 * h as u64 > MAX_PIXELS { return fail("the window is too large at this width; narrow the window or lower width") }
        // the PNG in the hub's render cache, named by a hash of the whole ask so the same ask overwrites
        let name: String = std::path::Path::new(&routed).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "render".into());
        let key = format!("{routed}|{window:?}|{width}|{off:?}|{dim:?}");
        let mut hh: u64 = 0xcbf29ce484222325; for b in key.as_bytes() { hh = (hh ^ *b as u64).wrapping_mul(0x100000001b3) }
        let dir = self.hub.cfg.home.join("cache").join("render");
        let out = dir.join(format!("{name}-{:016x}.png", hh));
        let bytes = png(&img, w, h);
        let wrote = tokio::task::spawn_blocking(move || { let _ = std::fs::create_dir_all(&dir); std::fs::write(&out, &bytes).map(|_| out) }).await;
        match wrote {
            Ok(Ok(out)) => (true, json!({"path": out.to_string_lossy(), "width": w, "height": h, "source": path,
                "window": window, "note": "Read the path to see the picture; write the path bare in a reply to show the person."})),
            Ok(Err(e)) => fail(&format!("could not write the PNG: {e}")),
            Err(e) => fail(&format!("could not write the PNG: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn board_with(forms: &[&[u8]]) -> Plot { let (p, _) = plotgen::board_plot(forms); p }
    fn at(p: &[u8], x: u32, y: u32, w: u32) -> [u8; 4] { let i = ((y * w + x) * 4) as usize; [p[i], p[i + 1], p[i + 2], p[i + 3]] }
    const GROUND: [u8; 3] = [0x08, 0x09, 0x0b];
    /// What one chunk's colour at `alpha` paints over the ground (the renderer's blend, one pass).
    fn blend(c: u32, a: f32) -> [u8; 3] { let col = [(c >> 16 & 255) as u8, (c >> 8 & 255) as u8, (c & 255) as u8];
        std::array::from_fn(|k| (col[k] as f32 * a + GROUND[k] as f32 * (1. - a)).round() as u8) }
    /// Whether a chunk's decoded geometry covers the point (a capsule within its radius, a polygon even-odd), as the renderer paints it.
    fn covers(g: &Geo, x: f64, y: f64) -> bool {
        g.caps.iter().any(|c| { let (ax, ay, bx, by, r) = (c[0] as f64 / 1000., c[1] as f64 / 1000., c[2] as f64 / 1000., c[3] as f64 / 1000., c[4] as f64 / 1000.);
            let (ex, ey) = (bx - ax, by - ay); let t = if ex == 0. && ey == 0. { 0. } else { (((x - ax) * ex + (y - ay) * ey) / (ex * ex + ey * ey)).clamp(0., 1.) };
            let (dx, dy) = (x - ax - t * ex, y - ay - t * ey); dx * dx + dy * dy <= r * r })
        || g.polys.iter().any(|p| { let n = p.len() / 2; let mut inside = false; let mut j = n - 1;
            for i in 0..n { let (xi, yi, xj, yj) = (p[2 * i] as f64 / 1000., p[2 * i + 1] as f64 / 1000., p[2 * j] as f64 / 1000., p[2 * j + 1] as f64 / 1000.);
                if (yi > y) != (yj > y) && x < (xj - xi) * (y - yi) / (yj - yi) + xi { inside = !inside } j = i; }
            inside })
    }
    /// The pixel the renderer must give: the ground, or the last covering chunk's blend over whatever is beneath (the test keeps
    /// the fixtures apart so the beneath is the ground, or names the expected stack itself).
    fn near(a: [u8; 3], b: [u8; 3]) -> bool { a.iter().zip(b.iter()).all(|(x, y)| (*x as i32 - *y as i32).abs() <= 1) }

    #[test] fn a_track_and_a_pad_render_where_they_lie() {
        // a 10x10 mm board: a 1 mm track on F.Cu at y 7, a 2 mm round pad centred (5,2), kept apart so each sample's stack is one blend
        let p = board_with(&[b"(gr_line (start 1 7) (end 9 7) (layer \"F.Cu\") (width 1) (uuid \"a\"))", b"(gr_circle (center 5 2) (end 6 2) (layer \"F.Cu\") (width 0) (fill solid) (uuid \"b\"))"]);
        assert!(!p.cs.is_empty());
        let (img, w, h) = pixels(&p, [0., 0., 10., 10.], 100, &[]);
        assert_eq!((w, h), (100, 100));
        assert_eq!(&at(&img, 0, 0, w)[..3], &GROUND, "the dark view ground at the corner");
        // each sample: the last covering chunk's own colour and alpha, blended once over the ground
        for (x_mm, y_mm, x_px, y_px, what) in [(5., 7., 50u32, 70u32, "the track's middle"), (5., 2., 50, 20, "the pad's centre")] {
            let want = p.cs.iter().rev().find(|c| covers(&decode(c), x_mm, y_mm)).map(|c| blend(c.color, c.alpha as f32 / 255.));
            let got = at(&img, x_px, y_px, w);
            assert!(want.map(|w| near(got[..3].try_into().unwrap(), w)).unwrap_or(false), "{what}: {got:?} want {want:?}");
        }
        assert_ne!(at(&img, 20, 60, w)[..3], at(&img, 20, 70, w)[..3], "a millimetre above the track is not the track");
        assert_eq!(&at(&img, 15, 15, w)[..3], &GROUND, "off the drawing");
    }
    #[test] fn a_window_renders_its_region_and_off_hides_its_layer() {
        let p = board_with(&[b"(gr_line (start 0 0) (end 10 10) (layer \"F.Cu\") (width 0.5) (uuid \"a\"))", b"(gr_line (start 0 10) (end 10 0) (layer \"B.Cu\") (width 0.5) (uuid \"b\"))"]);
        assert!(p.cs.len() >= 2, "{} chunks", p.cs.len());
        let (img, w, h) = pixels(&p, [2., 2., 4., 4.], 200, &[]); // 2x2 mm window: 200 x 200 px
        assert_eq!((w, h), (200, 200));
        // the F.Cu diagonal passes (3,3) at the window's middle; the B.Cu one passes (3,7), outside this window
        let want = p.cs.iter().rev().find(|c| covers(&decode(c), 3., 3.)).map(|c| blend(c.color, c.alpha as f32 / 255.));
        let got = at(&img, 100, 100, w);
        assert!(want.map(|w| near(got[..3].try_into().unwrap(), w)).unwrap_or(false), "{got:?} want {want:?}");
        // the covering layer off: the middle is the ground
        let top = p.cs.iter().rev().find(|c| covers(&decode(c), 3., 3.)).unwrap().layer;
        let without = pixels(&p, [2., 2., 4., 4.], 200, &[top]).0;
        assert_eq!(&at(&without, 100, 100, w)[..3], &GROUND, "the covering layer off: the middle is ground");
    }
    #[test] fn a_schematic_renders_with_its_own_look() {
        let bytes = b"(kicad_sch (version 20250114) (generator eeschema) (uuid x)\n(wire (pts (xy 10 10) (xy 40 10)) (stroke (width 0.2)) (uuid w1))\n)";
        let (p, _) = match plotgen::of_bytes(1, bytes, false) { plotgen::Read::Plot(p, t) => (*p, t), _ => panic!("a sheet") };
        assert!(!p.cs.is_empty(), "{} chunks", p.cs.len());
        let (img, w, h) = pixels(&p, [5., 5., 45., 15.], 400, &[]);
        assert_eq!((w, h), (400, 100));
        let want = p.cs.iter().rev().find(|c| covers(&decode(c), 25., 10.)).map(|c| blend(c.color, c.alpha as f32 / 255.));
        let got = at(&img, 200, 50, w);
        assert!(want.map(|w| near(got[..3].try_into().unwrap(), w)).unwrap_or(false), "the wire's middle: {got:?} want {want:?}");
    }
    #[test] fn the_png_is_a_png_and_roundtrips_its_pixels() {
        let (img, w, h) = (vec![1u8; 4 * 6 * 5], 6u32, 5u32);
        let p = png(&img, w, h);
        assert_eq!(&p[..8], &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
        // IHDR: width, height, 8-bit RGBA
        assert_eq!(&p[16..20], &w.to_be_bytes()); assert_eq!(&p[20..24], &h.to_be_bytes()); assert_eq!(&p[24..26], &[8, 6]);
        // the IDAT inflates back to the raw scanlines (filter 0 a row)
        let at = p.windows(4).position(|w| w == b"IDAT").unwrap();
        let len = u32::from_be_bytes([p[at - 4], p[at - 3], p[at - 2], p[at - 1]]) as usize;
        let mut z = flate2::read::ZlibDecoder::new(&p[at + 4..at + 4 + len]);
        use std::io::Read; let mut raw = vec![]; z.read_to_end(&mut raw).unwrap();
        assert_eq!(raw.len(), (1 + w as usize * 4) * h as usize, "a filter byte a row");
        assert_eq!(&raw[5..9], &[1, 1, 1, 1], "row 0's first pixel, filter byte skipped");
    }
    #[test] fn the_definition_says_what_it_does_and_takes_a_path() {
        let d = def();
        assert_eq!(d["name"], "board_render");
        let text = d["description"].as_str().unwrap();
        assert!(text.contains("PNG") && text.contains("window") && text.contains("layers") || text.contains("Layers"));
        assert_eq!(d["inputSchema"]["required"], json!(["path"]));
        assert!(text.len() < 2200, "{} characters", text.len());
    }
    /// A real board (its path in BP_RENDER_PROBE; unset: skipped) renders whole: chunks, a PNG of the
    /// frame, and a painted share a drawn board gives. A scratch check on the person's machine, no
    /// path in the tree.
    #[test] fn a_real_board_renders_whole() {
        let Ok(path) = std::env::var("BP_RENDER_PROBE") else { return };
        let bytes = std::fs::read(&path).unwrap();
        let (plot, _) = match plotgen::of_bytes(0, &bytes, false) { plotgen::Read::Plot(p, t) => (*p, t), _ => panic!("unreadable") };
        assert!(!plot.cs.is_empty(), "{} chunks", plot.cs.len());
        let win = bounds(&plot.b);
        let t0 = std::time::Instant::now();
        let (img, w, h) = pixels(&plot, win, 1024, &[]);
        let p = png(&img, w, h);
        eprintln!("board_render: {} chunks, {w}x{h}, {} B png in {:.0} ms", plot.cs.len(), p.len(), t0.elapsed().as_secs_f64() * 1e3);
        let painted = img.chunks_exact(4).filter(|q| q[..3] != [0x08, 0x09, 0x0b]).count();
        let share = painted as f64 / (w as f64 * h as f64);
        eprintln!("painted share {share:.3}");
        assert!(share > 0.02, "a real board paints more than 2% of the frame");
        assert_eq!(&p[..4], &[0x89, b'P', b'N', b'G']);
        if let Ok(out) = std::env::var("BP_RENDER_OUT") { let _ = std::fs::write(out, &p); }
    }
    #[test] fn tint_chunks_paint_in_their_own_colour() {        // a comparison's tint chunk (mode 0 with the tint packed in width): its unpacked colour, not the chunk's
        let k = crate::files::kicad::board::tint_code(0xff8800, 200);
        // the wire's varints by hand (plotgen::Enc is private): info 0, a ring of 4 points, delta-coded from (0,0)
        let mut num = |d: &mut Vec<u8>, mut z: u32| { while z >= 128 { d.push((z & 127) as u8 | 128); z >>= 7 } d.push(z as u8) };
        let mut d = vec![]; num(&mut d, 0); num(&mut d, 4); // info 0, then the ring's four points
        let (mut px, mut py) = (0i32, 0i32);
        for (x, y) in [(0i32, 0i32), (4000, 0), (4000, 4000), (0, 4000)] { let zz = |v: i32| ((v << 1) ^ (v >> 31)) as u32; num(&mut d, zz(x - px)); num(&mut d, zz(y - py)); px = x; py = y; }
        let c = crate::files::kicad::plot::Chunk { sig: 1, layer: 26, color: 0, alpha: 255, mode: 0, width: k, d, info: String::new() };
        let g = decode(&c);
        // the tint's own 5-6-bit colour, unpacked: the packed code is what the chunk carries (board::tint_code quantizes)
        assert_eq!(g.color, crate::files::kicad::board::tint_color(k));
        assert!((g.alpha - crate::files::kicad::board::tint_alpha(k) as f32 / 255.).abs() < 1e-3);
        assert!((g.alpha - 200. / 255.).abs() < 4. / 255., "200 quantizes within a step of 6-bit alpha");
        assert_eq!(g.polys.len(), 1);
        assert_eq!(g.polys[0].len(), 8, "the ring's four points");
    }
}
