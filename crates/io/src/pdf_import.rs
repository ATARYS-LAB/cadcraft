//! A PDF page's vector content read as geometry, for PDFIMPORT: every stroked or filled path
//! flattened to polylines in millimetres from the page's lower-left corner (as shown after the
//! page's rotation), with its colour, stroke width and fill, and the PDF layer it is on; and every
//! run of visible text with its position, size, direction and colour (invisible text, such as the
//! searchable copy of text a PDF also draws as lines, is left out); and every picture, as a PNG
//! file's bytes with the parallelogram it fills. A path's
//! or text's layer is found by reading the page once with no layer on, then
//! once per layer with that layer alone on ([`crate::pdf_layers::with_layers`]): what a layer's
//! reading adds to the bare one is that layer's. The bytes are hostile: every failure is an error,
//! never a panic, and the amount read is capped.

use std::collections::HashMap;

use hayro::hayro_interpret::{GlyphDrawMode, Paint, PathDrawMode};
use kurbo::{Affine, BezPath, PathEl, Point};

use crate::pdf_read::{MM_PER_POINT, PdfFile};
use crate::{IoError, Result};

/// Most points read from one page.
pub const MAX_IMPORT_POINTS: usize = 2_000_000;
/// Most texts read from one page.
pub const MAX_IMPORT_TEXTS: usize = 100_000;
/// Most pictures read from one page.
pub const MAX_IMPORT_IMAGES: usize = 1_000;
/// Most picture pixels read from one page.
pub const MAX_IMPORT_PIXELS: usize = 64_000_000;
/// Most PDF layers told apart (each costs one more reading of the page).
const MAX_SPLIT_LAYERS: usize = 64;

/// One path of a page, ready to become drawing objects.
#[derive(Clone, Debug, PartialEq)]
pub struct ImportedPath {
    /// Its pieces (one per subpath), in millimetres from the page's lower-left corner.
    pub pieces: Vec<Vec<(f64, f64)>>,
    /// Each piece is closed.
    pub closed: Vec<bool>,
    /// The same pieces with their circular arcs kept: (x, y, bulge to the next point), the
    /// other curves flattened.
    pub arcs: Vec<Vec<(f64, f64, f64)>>,
    /// Stroked: colour and width in millimetres.
    pub stroke: Option<([u8; 3], f64)>,
    /// Filled: colour.
    pub fill: Option<[u8; 3]>,
    /// The PDF layer it is on.
    pub layer: Option<String>,
}

/// One run of text of a page.
#[derive(Clone, Debug, PartialEq)]
pub struct ImportedText {
    pub text: String,
    /// The first character's origin (on the baseline), in millimetres from the page's lower-left
    /// corner.
    pub at: (f64, f64),
    /// The font size (one em) in millimetres.
    pub size: f64,
    /// The baseline's direction, in radians counter-clockwise.
    pub rotation: f64,
    pub color: [u8; 3],
    /// The PDF layer it is on.
    pub layer: Option<String>,
    /// The font's family ("Arial" for ArialMT, ABCDEF+Arial-BoldMT…), when the font tells it.
    pub font: Option<String>,
}

/// One picture of a page.
#[derive(Clone, Debug, PartialEq)]
pub struct ImportedImage {
    /// The picture as a PNG file.
    pub png: Vec<u8>,
    /// Its size in pixels.
    pub size: (u32, u32),
    /// Its lower-left corner, in millimetres from the page's lower-left corner.
    pub at: (f64, f64),
    /// One pixel's width (along a row) and height (up a column), in millimetres.
    pub u: (f64, f64),
    pub v: (f64, f64),
    /// The PDF layer it is on.
    pub layer: Option<String>,
    /// The same for every placement of one picture of the PDF.
    pub key: u128,
}

/// A font's family from its PostScript name: without a subset tag (`ABCDEF+`), a style
/// (`-Bold`, `,Italic`) or an `MT`/`PS`/`PSMT` ending.
fn family(postscript: &str) -> Option<String> {
    let name = postscript.rsplit('+').next().unwrap_or(postscript);
    let name = name.split(['-', ',']).next().unwrap_or(name).trim();
    let name = ["PSMT", "MT", "PS"].iter().find_map(|end| name.strip_suffix(end)).unwrap_or(name);
    (!name.is_empty()).then(|| name.to_string())
}

fn rgb(p: &Paint<'_>) -> [u8; 3] {
    match p {
        Paint::Color(c) => {
            let [r, g, b, _] = c.to_rgba().to_rgba8();
            [r, g, b]
        }
        // Patterns and shadings: their average is unknown here; black.
        _ => [0, 0, 0],
    }
}

/// A point in millimetres.
type Mm = (f64, f64);

/// The circle a cubic Bézier from `p0` follows: its centre, radius and sweep (counter-clockwise
/// positive), when it is tangent to the curve at both ends and every point of the curve lies
/// within a thousandth of the radius of it. `None` for any other curve.
fn circular(p0: Mm, p1: Mm, p2: Mm, p3: Mm) -> Option<(Mm, f64, f64)> {
    let sub = |a: Mm, b: Mm| (a.0 - b.0, a.1 - b.1);
    let cross = |a: Mm, b: Mm| a.0 * b.1 - a.1 * b.0;
    let len = |a: Mm| a.0.hypot(a.1);
    let (t0, t3) = (sub(p1, p0), sub(p3, p2));
    if len(t0) < 1e-9 || len(t3) < 1e-9 || len(sub(p3, p0)) < 1e-9 {
        return None;
    }
    // The centre: where the normals at both ends meet.
    let (n0, n3) = ((-t0.1, t0.0), (t3.1, -t3.0));
    let det = cross(n0, n3);
    if det.abs() < 1e-12 * len(n0) * len(n3) {
        return None;
    }
    let s = cross(sub(p3, p0), n3) / det;
    let c = (p0.0 + n0.0 * s, p0.1 + n0.1 * s);
    let r = len(sub(p0, c));
    if !(r.is_finite() && r > 1e-9) {
        return None;
    }
    let tol = r * 1e-3;
    let at = |t: f64| {
        let u = 1.0 - t;
        let (a, b, cc, d) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
        (a * p0.0 + b * p1.0 + cc * p2.0 + d * p3.0, a * p0.1 + b * p1.1 + cc * p2.1 + d * p3.1)
    };
    if (len(sub(p3, c)) - r).abs() > tol || [0.25, 0.5, 0.75].iter().any(|t| (len(sub(at(*t), c)) - r).abs() > tol) {
        return None;
    }
    let (a, b) = (sub(p0, c), sub(p3, c));
    let mut sweep = cross(a, b).atan2(a.0 * b.0 + a.1 * b.1);
    if cross(a, t0) > 0.0 {
        if sweep <= 0.0 {
            sweep += std::f64::consts::TAU;
        }
    } else if sweep >= 0.0 {
        sweep -= std::f64::consts::TAU;
    }
    // The curve goes the way of the arc: its middle is the arc's.
    let (sn, cs) = (sweep / 2.0).sin_cos();
    let mid = (c.0 + a.0 * cs - a.1 * sn, c.1 + a.0 * sn + a.1 * cs);
    (len(sub(at(0.5), mid)) <= tol).then_some((c, r, sweep))
}

/// `path`'s pieces (one per subpath) in millimetres, each point with the bulge to the next: the
/// cubic curves that follow a circle as arcs (those of one circle in a row as one arc of at most a
/// half turn), the other curves flattened.
fn with_arcs(path: &BezPath, to: &dyn Fn(Point) -> Mm) -> Vec<Vec<(f64, f64, f64)>> {
    let mut out: Vec<Vec<(f64, f64, f64)>> = Vec::new();
    let mut current: Vec<(f64, f64, f64)> = Vec::new();
    // The last arc's circle and sweep, while the piece ends with one.
    let mut arc: Option<(Mm, f64, f64)> = None;
    let (mut at, mut start) = (Point::ORIGIN, Point::ORIGIN);
    let mut points = 0usize;
    let done = |current: &mut Vec<(f64, f64, f64)>, out: &mut Vec<Vec<(f64, f64, f64)>>| {
        if current.len() >= 2 {
            out.push(std::mem::take(current));
        } else {
            current.clear();
        }
    };
    for el in path.iter() {
        if points >= MAX_IMPORT_POINTS {
            break;
        }
        let from = at;
        let mut push = |current: &mut Vec<(f64, f64, f64)>, q: Point| {
            let (x, y) = to(q);
            current.push((x, y, 0.0));
            points += 1;
        };
        match el {
            PathEl::MoveTo(p) => {
                done(&mut current, &mut out);
                push(&mut current, p);
                (at, start, arc) = (p, p, None);
            }
            PathEl::LineTo(p) => {
                push(&mut current, p);
                (at, arc) = (p, None);
            }
            PathEl::CurveTo(a, b, p) => {
                match circular(to(from), to(a), to(b), to(p)) {
                    Some((c, r, sweep)) => {
                        let same = |(c2, r2, s2): (Mm, f64, f64)| {
                            (c.0 - c2.0).hypot(c.1 - c2.1) <= r * 1e-3
                                && (r - r2).abs() <= r * 1e-3
                                && s2.signum() == sweep.signum()
                                && (s2 + sweep).abs() <= std::f64::consts::PI + 1e-9
                        };
                        // The same circle on: one arc.
                        let total = match arc.filter(|x| same(*x)) {
                            Some((_, _, s2)) if current.len() >= 2 => {
                                current.pop();
                                s2 + sweep
                            }
                            _ => sweep,
                        };
                        if let Some(last) = current.last_mut() {
                            last.2 = (total / 4.0).tan();
                        }
                        push(&mut current, p);
                        arc = Some((c, r, total));
                    }
                    None => {
                        kurbo::flatten([PathEl::MoveTo(from), el], 0.05, |e| {
                            if let PathEl::LineTo(q) = e {
                                push(&mut current, q);
                            }
                        });
                        arc = None;
                    }
                }
                at = p;
            }
            PathEl::QuadTo(_, p) => {
                kurbo::flatten([PathEl::MoveTo(from), el], 0.05, |e| {
                    if let PathEl::LineTo(q) = e {
                        push(&mut current, q);
                    }
                });
                (at, arc) = (p, None);
            }
            PathEl::ClosePath => {
                done(&mut current, &mut out);
                (at, arc) = (start, None);
            }
        }
    }
    done(&mut current, &mut out);
    out
}

/// A device keeping each path's geometry, colour and widths.
struct Paths {
    out: Vec<ImportedPath>,
    texts: Vec<ImportedText>,
    images: Vec<ImportedImage>,
    points: usize,
    pixels: usize,
    /// The page's size in points (device space, y down).
    size: (f64, f64),
    /// The last path filled, kept while nothing else is drawn: its geometry and its index in
    /// `out`, so that its stroke (a fill-and-stroke paints twice) joins it.
    filled: Option<(BezPath, Affine, usize)>,
    /// The text being read: glyphs that follow each other on one baseline.
    run: Option<Run>,
}

/// Glyphs read one after the other on a baseline, in device space.
struct Run {
    text: String,
    font: Option<String>,
    origin: Point,
    /// Where the last glyph starts.
    last: Point,
    em: f64,
    rotation: f64,
    color: [u8; 3],
}

impl Paths {
    fn add(&mut self, path: &BezPath, transform: Affine, stroke: Option<([u8; 3], f64)>, fill: Option<[u8; 3]>) {
        if self.points >= MAX_IMPORT_POINTS {
            return;
        }
        let (_, h) = self.size;
        let to = |p: Point| (p.x * MM_PER_POINT, (h - p.y) * MM_PER_POINT);
        let mut pieces: Vec<Vec<(f64, f64)>> = Vec::new();
        let mut closed = Vec::new();
        let mut current: Vec<(f64, f64)> = Vec::new();
        let mut done = |current: &mut Vec<(f64, f64)>, is_closed: bool| {
            if current.len() >= 2 {
                pieces.push(std::mem::take(current));
                closed.push(is_closed);
            } else {
                current.clear();
            }
        };
        let path = transform * path.clone();
        kurbo::flatten(path.iter(), 0.05, |el| match el {
            PathEl::MoveTo(p) => {
                done(&mut current, false);
                current.push(to(p));
            }
            PathEl::LineTo(p) => current.push(to(p)),
            PathEl::ClosePath => done(&mut current, true),
            _ => {}
        });
        done(&mut current, false);
        let arcs = with_arcs(&path, &to);
        let n: usize = pieces.iter().map(Vec::len).sum::<usize>() + arcs.iter().map(Vec::len).sum::<usize>();
        if pieces.is_empty() || self.points.saturating_add(n) > MAX_IMPORT_POINTS {
            return;
        }
        self.points += n;
        // Stroke width scaled by the transform (its mean scale), in millimetres.
        let k = transform.determinant().abs().sqrt();
        let stroke = stroke.map(|(c, w)| (c, (w * k * MM_PER_POINT).max(0.0)));
        self.out.push(ImportedPath { pieces, closed, arcs, stroke, fill, layer: None });
    }
}

impl Paths {
    /// The text being read, ended.
    fn end_text(&mut self) {
        let Some(r) = self.run.take() else { return };
        let text = r.text.trim_end().to_string();
        if text.trim().is_empty() || self.texts.len() >= MAX_IMPORT_TEXTS {
            return;
        }
        let (_, h) = self.size;
        self.texts.push(ImportedText {
            text,
            at: (r.origin.x * MM_PER_POINT, (h - r.origin.y) * MM_PER_POINT),
            size: r.em * MM_PER_POINT,
            rotation: r.rotation,
            color: r.color,
            layer: None,
            font: r.font,
        });
    }

    /// Something other than a glyph is drawn: the text being read ends, and so does a fill a
    /// stroke could join.
    fn other(&mut self) {
        self.end_text();
        self.filled = None;
    }
}

impl<'a> hayro::hayro_interpret::Device<'a> for Paths {
    fn set_soft_mask(&mut self, _: Option<hayro::hayro_interpret::SoftMask<'a>>) {}
    fn set_blend_mode(&mut self, _: hayro::hayro_interpret::BlendMode) {}
    fn draw_path(&mut self, path: &BezPath, transform: Affine, paint: &Paint<'a>, mode: &PathDrawMode) {
        self.end_text();
        let c = rgb(paint);
        match mode {
            PathDrawMode::Fill(_) => {
                let n = self.out.len();
                self.add(path, transform, None, Some(c));
                self.filled = (self.out.len() > n).then(|| (path.clone(), transform, n));
            }
            PathDrawMode::Stroke(st) => {
                let stroke = (c, f64::from(st.line_width));
                // The stroke of the path just filled (fill and stroke): one path with both.
                if let Some((_, _, i)) = self.filled.take().filter(|(p, t, _)| p == path && *t == transform) {
                    let k = transform.determinant().abs().sqrt();
                    if let Some(o) = self.out.get_mut(i) {
                        o.stroke = Some((stroke.0, (stroke.1 * k * MM_PER_POINT).max(0.0)));
                        return;
                    }
                }
                self.add(path, transform, Some(stroke), None);
            }
        }
    }
    fn push_clip_path(&mut self, _: &hayro::hayro_interpret::ClipPath) {}
    fn push_transparency_group(&mut self, _: f32, _: Option<hayro::hayro_interpret::SoftMask<'a>>, _: hayro::hayro_interpret::BlendMode) {}
    fn draw_glyph(
        &mut self,
        glyph: &hayro::hayro_interpret::font::Glyph<'a>,
        transform: Affine,
        glyph_transform: Affine,
        paint: &Paint<'a>,
        mode: &GlyphDrawMode,
    ) {
        self.filled = None;
        if matches!(mode, GlyphDrawMode::Invisible) || self.texts.len() >= MAX_IMPORT_TEXTS {
            return;
        }
        // Glyph outlines are in units of 1/1000 em.
        let t = transform * glyph_transform;
        let o = t * Point::ORIGIN;
        let up = t * Point::new(0.0, 1000.0) - o;
        let along = t * Point::new(1000.0, 0.0) - o;
        // Device space has y down: turn it back up.
        let (em, rotation) = (up.hypot(), (-along.y).atan2(along.x));
        if !(em.is_finite() && em > 0.0 && rotation.is_finite()) {
            return;
        }
        let color = rgb(paint);
        // The same text goes on when the glyph follows on its baseline, close enough.
        let follows = self.run.as_ref().is_some_and(|r| {
            let d = o - r.last;
            let (dx, dy) = (rotation.cos(), -rotation.sin());
            let (on, off) = (d.x * dx + d.y * dy, d.y * dx - d.x * dy);
            r.color == color
                && (r.em - em).abs() <= em * 1e-3
                && (r.rotation - rotation).abs() <= 1e-3
                && on >= -em * 0.01
                && on <= em * 3.0
                && off.abs() <= em * 0.05
        });
        if !follows {
            self.end_text();
            self.run = Some(Run { text: String::new(), font: None, origin: o, last: o, em, rotation, color });
        }
        let Some(r) = self.run.as_mut() else { return };
        r.last = o;
        if r.font.is_none()
            && let hayro::hayro_interpret::font::Glyph::Outline(g) = glyph
        {
            r.font = g.font_data().and_then(|f| f.postscript_name).and_then(|n| family(&n));
        }
        match glyph.as_unicode() {
            Some(hayro::hayro_interpret::hayro_cmap::BfString::Char(c)) => r.text.push(c),
            Some(hayro::hayro_interpret::hayro_cmap::BfString::String(s)) => r.text.push_str(&s),
            None => {}
        }
    }
    fn draw_image(&mut self, image: hayro::hayro_interpret::Image<'a, '_>, transform: Affine) {
        use hayro::hayro_interpret::{CacheKey, Image, ImageData};
        self.other();
        // Stencil masks paint a colour through a shape, as fills do: not pictures.
        let Image::Raster(r) = image else { return };
        if self.images.len() >= MAX_IMPORT_IMAGES || self.pixels >= MAX_IMPORT_PIXELS {
            return;
        }
        let key = r.cache_key();
        let (_, h) = self.size;
        let to = |p: Point| (p.x * MM_PER_POINT, (h - p.y) * MM_PER_POINT);
        let mut got: Option<ImportedImage> = None;
        let left = MAX_IMPORT_PIXELS - self.pixels;
        r.with_rgba(
            |data, alpha| {
                let (w, ht) = (data.width(), data.height());
                let n = (w as usize).saturating_mul(ht as usize);
                if n == 0 || n > left {
                    return;
                }
                let rgb: Vec<u8> = match &data {
                    ImageData::Rgb(d) => d.data.clone(),
                    ImageData::Luma(d) => d.data.iter().flat_map(|g| [*g; 3]).collect(),
                };
                if rgb.len() != n * 3 {
                    return;
                }
                // The alpha, nearest pixel when its size differs.
                let a = |x: u32, y: u32| -> u8 {
                    alpha.as_ref().map_or(255, |m| {
                        let mx = (u64::from(x) * u64::from(m.width) / u64::from(w.max(1))) as usize;
                        let my = (u64::from(y) * u64::from(m.height) / u64::from(ht.max(1))) as usize;
                        m.data.get(my * m.width as usize + mx).copied().unwrap_or(255)
                    })
                };
                let pre = |c: u8, al: u8| ((u16::from(c) * u16::from(al) + 127) / 255) as u8;
                let mut rgba = Vec::with_capacity(n * 4);
                for (i, px) in rgb.as_chunks::<3>().0.iter().enumerate() {
                    let al = a((i % w as usize) as u32, (i / w as usize) as u32);
                    rgba.extend_from_slice(&[pre(px[0], al), pre(px[1], al), pre(px[2], al), al]);
                }
                let Some(png) =
                    tiny_skia::IntSize::from_wh(w, ht).and_then(|size| tiny_skia::Pixmap::from_vec(rgba, size)).and_then(|pm| pm.encode_png().ok())
                else {
                    return;
                };
                // Pixel space (row 0 at the top) onto the page.
                let (sx, sy) = data.scale_factors();
                let t = transform * Affine::scale_non_uniform(f64::from(sx), f64::from(sy));
                let (wf, hf) = (f64::from(w), f64::from(ht));
                let (top, bottom, right) = (to(t * Point::new(0.0, 0.0)), to(t * Point::new(0.0, hf)), to(t * Point::new(wf, hf)));
                let u = ((right.0 - bottom.0) / wf, (right.1 - bottom.1) / wf);
                let v = ((top.0 - bottom.0) / hf, (top.1 - bottom.1) / hf);
                if [bottom.0, bottom.1, u.0, u.1, v.0, v.1].iter().all(|x| x.is_finite()) && (u.0 * v.1 - u.1 * v.0).abs() > 0.0 {
                    got = Some(ImportedImage { png, size: (w, ht), at: bottom, u, v, layer: None, key });
                }
            },
            None,
        );
        if let Some(im) = got {
            self.pixels += (im.size.0 as usize).saturating_mul(im.size.1 as usize);
            self.images.push(im);
        }
    }
    fn pop_clip_path(&mut self) {}
    fn pop_transparency_group(&mut self) {}
}

/// What a page holds: its paths and its texts.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ImportedPage {
    pub paths: Vec<ImportedPath>,
    pub texts: Vec<ImportedText>,
    pub images: Vec<ImportedImage>,
}

/// The paths and texts of page `page` (from 1) of the PDF `bytes`, each without its layer.
fn read_paths(bytes: &[u8], page: u32) -> Result<ImportedPage> {
    let f = PdfFile::open(bytes)?;
    let mut pg = f.with_device(
        page as usize,
        |size| Paths { out: Vec::new(), texts: Vec::new(), images: Vec::new(), points: 0, pixels: 0, size, filled: None, run: None },
        |mut p| {
            p.end_text();
            ImportedPage { paths: p.out, texts: p.texts, images: p.images }
        },
    )?;
    // A text whose font doesn't tell its name (a font the PDF doesn't embed, drawn with an installed
    // one): the page's font when it names only one family.
    if pg.texts.iter().any(|t| t.font.is_none()) {
        let mut families: Vec<String> = f.font_names(page as usize)?.iter().filter_map(|n| family(n)).collect();
        families.sort();
        families.dedup();
        if let [only] = families.as_slice() {
            for t in pg.texts.iter_mut().filter(|t| t.font.is_none()) {
                t.font = Some(only.clone());
            }
        }
    }
    Ok(pg)
}

/// A text's identity across readings of the same page.
fn text_key(t: &ImportedText) -> String {
    format!("{}@{:.3},{:.3}", t.text, t.at.0, t.at.1)
}

/// A picture's identity across readings of the same page.
fn image_key(i: &ImportedImage) -> String {
    format!("{}@{:.3},{:.3}/{:.6},{:.6}", i.key, i.at.0, i.at.1, i.u.0, i.v.1)
}

/// A path's identity across readings of the same page.
fn key(p: &ImportedPath) -> String {
    let mut k = String::new();
    for piece in &p.pieces {
        for (x, y) in piece {
            k.push_str(&format!("{:.3},{:.3};", x, y));
        }
        k.push('|');
    }
    k
}

/// The vector content and text of page `page` (from 1) of the PDF `bytes`, each with the PDF
/// layer it is on (`None` for content on no layer).
pub fn import_page(bytes: &[u8], page: u32) -> Result<ImportedPage> {
    let layers = crate::pdf_layers::layers(bytes)?;
    if layers.is_empty() {
        return read_paths(bytes, page);
    }
    if layers.len() > MAX_SPLIT_LAYERS {
        // Too many layers to tell apart: everything as on no layer.
        return read_paths(&crate::pdf_layers::with_layers(bytes, &layers.iter().map(|l| (l.name.clone(), true)).collect::<Vec<_>>())?, page);
    }
    let only = |on: Option<&str>| -> Vec<(String, bool)> { layers.iter().map(|l| (l.name.clone(), Some(l.name.as_str()) == on)).collect() };
    // Content on no layer: counted, so that each layer's reading gives back only its own.
    let bare = read_paths(&crate::pdf_layers::with_layers(bytes, &only(None))?, page)?;
    let mut common: HashMap<String, usize> = HashMap::new();
    for p in &bare.paths {
        *common.entry(key(p)).or_default() += 1;
    }
    for t in &bare.texts {
        *common.entry(text_key(t)).or_default() += 1;
    }
    for i in &bare.images {
        *common.entry(image_key(i)).or_default() += 1;
    }
    let mut out = bare;
    for l in &layers {
        let mut left = common.clone();
        let read = read_paths(&crate::pdf_layers::with_layers(bytes, &only(Some(&l.name)))?, page)?;
        for mut p in read.paths {
            match left.get_mut(&key(&p)) {
                Some(n) if *n > 0 => *n -= 1,
                _ => {
                    p.layer = Some(l.name.clone());
                    out.paths.push(p);
                }
            }
        }
        for mut t in read.texts {
            match left.get_mut(&text_key(&t)) {
                Some(n) if *n > 0 => *n -= 1,
                _ => {
                    t.layer = Some(l.name.clone());
                    out.texts.push(t);
                }
            }
        }
        for mut i in read.images {
            match left.get_mut(&image_key(&i)) {
                Some(n) if *n > 0 => *n -= 1,
                _ => {
                    i.layer = Some(l.name.clone());
                    out.images.push(i);
                }
            }
        }
    }
    Ok(out)
}

/// [`import_page`] for the engine's PDF hooks: each path as (pieces, closed, stroke colour and
/// width in mm, fill colour, layer, the pieces with their arcs), each text as (text, origin in mm, em size in mm, rotation,
/// colour, layer), each picture as (PNG bytes, size in pixels, lower-left corner in mm, one pixel's
/// width and height vectors in mm, layer, identity).
pub fn import_hook(
    bytes: &[u8],
    page: u32,
) -> std::result::Result<(Vec<ImportedPathTuple>, Vec<ImportedTextTuple>, Vec<ImportedImageTuple>), String> {
    import_page(bytes, page)
        .map(|pg| {
            (
                pg.paths.into_iter().map(|p| (p.pieces, p.closed, p.stroke, p.fill, p.layer, p.arcs)).collect(),
                pg.texts.into_iter().map(|t| (t.text, t.at, t.size, t.rotation, t.color, t.layer, t.font)).collect(),
                pg.images.into_iter().map(|i| (i.png, i.size, i.at, i.u, i.v, i.layer, i.key)).collect(),
            )
        })
        .map_err(|e: IoError| e.to_string())
}

/// A path as the engine's PDF hooks take it.
pub type ImportedPathTuple = (Vec<Vec<(f64, f64)>>, Vec<bool>, Option<([u8; 3], f64)>, Option<[u8; 3]>, Option<String>, Vec<Vec<(f64, f64, f64)>>);
/// A picture as the engine's PDF hooks take it.
pub type ImportedImageTuple = (Vec<u8>, (u32, u32), (f64, f64), (f64, f64), (f64, f64), Option<String>, u128);
/// A text as the engine's PDF hooks take it.
pub type ImportedTextTuple = (String, (f64, f64), f64, f64, [u8; 3], Option<String>, Option<String>);

#[cfg(test)]
mod tests {
    use super::*;
    use cadcraft_doc::{Common, Drawing, EntityKind, Line, Space};
    use cadcraft_geom::Vec3;

    /// A PDF written by CADCraft's own export at 1:1 on A4: a red line on layer MURS, a green one
    /// on COTES.
    fn layered_pdf() -> Vec<u8> {
        let mut d = Drawing::new_metric();
        for (name, color, y) in [("MURS", 1, 50.0), ("COTES", 3, 120.0)] {
            d.layers.push(cadcraft_doc::Layer { name: name.into(), color: cadcraft_color::Color::Index(color), ..cadcraft_doc::Layer::default() });
            let c = Common { layer: name.into(), ..Common::default() };
            d.add(&Space::Model, c, EntityKind::Line(Line { a: Vec3::new(10.0, y, 0.0), b: Vec3::new(280.0, y, 0.0) })).unwrap();
        }
        let o = crate::PdfOptions {
            paper_mm: Some((297.0, 210.0)),
            fit: Some(false),
            scale: Some(1.0),
            center: Some(false),
            offset: Some([0.0, 0.0]),
            ..crate::PdfOptions::default()
        };
        crate::pdf(&d, &Space::Model, &o).unwrap()
    }

    #[test]
    fn a_page_is_read_as_paths_with_their_colour_and_layer() {
        let paths = import_page(&layered_pdf(), 1).unwrap().paths;
        let on = |layer: &str| paths.iter().filter(|p| p.layer.as_deref() == Some(layer)).collect::<Vec<_>>();
        let (murs, cotes) = (on("MURS"), on("COTES"));
        assert_eq!((murs.len(), cotes.len()), (1, 1), "{paths:#?}");
        let red = murs[0].stroke.unwrap().0;
        assert!(red[0] > 200 && red[1] < 60, "{red:?}");
        assert!(cotes[0].stroke.unwrap().0[1] > 200);
        let xs: Vec<f64> = murs[0].pieces[0].iter().map(|p| p.0).collect();
        let ys: Vec<f64> = murs[0].pieces[0].iter().map(|p| p.1).collect();
        let len = (xs.iter().cloned().fold(f64::MIN, f64::max) - xs.iter().cloned().fold(f64::MAX, f64::min)).abs();
        assert!((len - 270.0).abs() < 1.0, "the line keeps its length in mm: {len}");
        assert!(ys.iter().all(|y| (y - ys[0]).abs() < 1e-6), "horizontal");
    }

    /// A PDF from `objs` (object 1 the catalog), with its cross-reference table.
    fn pdf_objects(objs: &[String]) -> Vec<u8> {
        let mut out = String::from("%PDF-1.7\n");
        let mut offsets = Vec::new();
        for (i, o) in objs.iter().enumerate() {
            offsets.push(out.len());
            out.push_str(&format!("{} 0 obj\n{o}\nendobj\n", i + 1));
        }
        let xref = out.len();
        out.push_str(&format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1));
        for o in offsets {
            out.push_str(&format!("{o:010} 00000 n \n"));
        }
        out.push_str(&format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objs.len() + 1));
        out.into_bytes()
    }

    /// An A4 landscape page with red text "ABC" in a Type 3 font (its glyphs drawn by the PDF
    /// itself, with a ToUnicode map: no installed font needed), 36 pt, from (100, 200) pt, "CAB"
    /// turned 90° from (400, 100) pt, and an invisible "ABC".
    fn text_pdf() -> Vec<u8> {
        let content =
            "1 0 0 rg BT /F1 36 Tf 100 200 Td (ABC) Tj ET BT /F1 36 Tf 0 1 -1 0 400 100 Tm (CAB) Tj ET BT 3 Tr /F1 12 Tf 50 50 Td (ABC) Tj ET";
        let glyph = "1000 0 0 0 800 800 d1 0 0 800 800 re f";
        let cmap = "/CIDInit /ProcSet findresource begin 12 dict begin begincmap /CMapName /T3 def 1 begincodespacerange <00> <FF> endcodespacerange 1 beginbfrange <41> <43> <0041> endbfrange endcmap CMapName currentdict /CMap defineresource pop end end";
        pdf_objects(&[
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 842 595] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_string(),
            format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()),
            "<< /Type /Font /Subtype /Type3 /BaseFont /ArialMT /FontBBox [0 0 1000 1000] /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs << /g 6 0 R >> /Encoding << /Type /Encoding /Differences [65 /g /g /g] >> /FirstChar 65 /LastChar 67 /Widths [1000 1000 1000] /Resources << >> /ToUnicode 7 0 R >>".to_string(),
            format!("<< /Length {} >>\nstream\n{glyph}\nendstream", glyph.len()),
            format!("<< /Length {} >>\nstream\n{cmap}\nendstream", cmap.len()),
        ])
    }

    #[test]
    fn text_runs_are_read_with_their_place_size_direction_and_colour() {
        let pg = import_page(&text_pdf(), 1).unwrap();
        let texts: Vec<&ImportedText> = pg.texts.iter().collect();
        assert_eq!(texts.iter().map(|t| t.text.as_str()).collect::<Vec<_>>(), vec!["ABC", "CAB"], "the invisible text is left out: {texts:#?}");
        let mm = |pt: f64| pt * 25.4 / 72.0;
        let abc = texts[0];
        assert!((abc.at.0 - mm(100.0)).abs() < 1e-6 && (abc.at.1 - mm(200.0)).abs() < 1e-6, "{abc:?}");
        assert!((abc.size - mm(36.0)).abs() < 1e-6 && abc.rotation.abs() < 1e-9 && abc.color == [255, 0, 0], "{abc:?}");
        assert_eq!(abc.font.as_deref(), Some("Arial"), "the page's only font family");
        let cab = texts[1];
        assert!((cab.rotation - std::f64::consts::FRAC_PI_2).abs() < 1e-9 && (cab.at.0 - mm(400.0)).abs() < 1e-6, "{cab:?}");
    }

    #[test]
    fn a_fonts_family_is_its_postscript_name_without_subset_style_or_ending() {
        assert_eq!(family("ArialMT").as_deref(), Some("Arial"));
        assert_eq!(family("ABCDEF+Arial-BoldMT").as_deref(), Some("Arial"));
        assert_eq!(family("TimesNewRomanPSMT").as_deref(), Some("TimesNewRoman"));
        assert_eq!(family("Calibri,Bold").as_deref(), Some("Calibri"));
        assert_eq!(family("+"), None);
    }

    /// An A4 landscape page with a 2 x 2 picture (red, green / blue, white from the top left)
    /// 200 x 100 pt at (100, 300) pt, the same picture again turned a quarter turn at (500, 100), and
    /// a 1 x 1 picture with an alpha mask.
    fn image_pdf() -> Vec<u8> {
        let content = "q 200 0 0 100 100 300 cm /A Do Q q 0 100 -50 0 500 100 cm /A Do Q q 10 0 0 10 50 50 cm /B Do Q";
        let bytes = [255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255];
        let mut objs: Vec<Vec<u8>> = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 842 595] /Resources << /XObject << /A 5 0 R /B 6 0 R >> >> /Contents 4 0 R >>".to_string(),
            format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()),
        ]
        .map(String::into_bytes)
        .to_vec();
        let image = |dict: &str, data: &[u8]| {
            let mut o = format!("<< /Type /XObject /Subtype /Image {dict} /BitsPerComponent 8 /Length {} >>\nstream\n", data.len()).into_bytes();
            o.extend_from_slice(data);
            o.extend_from_slice(b"\nendstream");
            o
        };
        objs.push(image("/Width 2 /Height 2 /ColorSpace /DeviceRGB", &bytes));
        objs.push(image("/Width 1 /Height 1 /ColorSpace /DeviceRGB /SMask 7 0 R", &[0, 0, 255]));
        objs.push(image("/Width 1 /Height 1 /ColorSpace /DeviceGray", &[128]));
        let mut out = b"%PDF-1.7\n".to_vec();
        let mut offsets = Vec::new();
        for (i, o) in objs.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
            out.extend_from_slice(o);
            out.extend_from_slice(b"\nendobj\n");
        }
        let xref = out.len();
        out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes());
        for o in offsets {
            out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objs.len() + 1).as_bytes());
        out
    }

    #[test]
    fn pictures_are_read_as_png_files_with_the_parallelogram_they_fill() {
        let images = import_page(&image_pdf(), 1).unwrap().images;
        assert_eq!(images.len(), 3);
        let mm = |pt: f64| pt * 25.4 / 72.0;
        let near = |a: (f64, f64), b: (f64, f64)| (a.0 - b.0).abs() < 1e-6 && (a.1 - b.1).abs() < 1e-6;
        let a = &images[0];
        assert_eq!(a.size, (2, 2));
        // Lower-left corner, then one pixel: 100 pt wide, 50 pt high.
        assert!(near(a.at, (mm(100.0), mm(300.0))) && near(a.u, (mm(100.0), 0.0)) && near(a.v, (0.0, mm(50.0))), "{a:?}");
        let pm = tiny_skia::Pixmap::decode_png(&a.png).unwrap();
        let rgba = |x: u32, y: u32| pm.pixel(x, y).map(|p| p.demultiply()).map(|c| [c.red(), c.green(), c.blue(), c.alpha()]);
        // Row 0 is the top: red at the top left, white at the bottom right.
        assert_eq!((rgba(0, 0), rgba(1, 1)), (Some([255, 0, 0, 255]), Some([255, 255, 255, 255])));
        // The same picture turned: its rows go up the page, the same identity.
        let b = &images[1];
        assert!(near(b.at, (mm(500.0), mm(100.0))) && near(b.u, (0.0, mm(50.0))) && near(b.v, (mm(-25.0), 0.0)), "{b:?}");
        assert_eq!(a.key, b.key);
        // The alpha mask goes with the picture.
        let c = tiny_skia::Pixmap::decode_png(&images[2].png).unwrap();
        let p = c.pixel(0, 0).unwrap();
        assert!((126..=130).contains(&p.alpha()), "{p:?}");
    }

    #[test]
    fn curves_that_follow_a_circle_are_arcs_the_others_flattened() {
        use kurbo::Shape;
        let mm = |p: Point| (p.x, p.y);
        // A circle drawn as four Béziers: two half turns, back to the start.
        let circle = kurbo::Circle::new((50.0, 40.0), 10.0).to_path(0.01);
        let pieces = with_arcs(&circle, &mm);
        assert_eq!(pieces.len(), 1);
        let v = &pieces[0];
        assert_eq!(v.len(), 3, "{v:?}");
        assert!(v[..2].iter().all(|q| (q.2.abs() - 1.0).abs() < 1e-9), "half turns: {v:?}");
        assert!((v[0].0 - v[2].0).abs() < 1e-9 && (v[0].1 - v[2].1).abs() < 1e-9);
        // A quarter turn then a straight line.
        let mut p = BezPath::new();
        p.move_to((10.0, 0.0));
        let k = 0.552_284_75 * 10.0;
        p.curve_to((10.0, k), (k, 10.0), (0.0, 10.0));
        p.line_to((-5.0, 10.0));
        let v = &with_arcs(&p, &mm)[0];
        assert_eq!(v.len(), 3);
        assert!((v[0].2 - (std::f64::consts::FRAC_PI_2 / 4.0).tan()).abs() < 1e-9 && v[1].2 == 0.0, "{v:?}");
        // The other way round: clockwise, a negative bulge.
        let mut p = BezPath::new();
        p.move_to((0.0, 10.0));
        p.curve_to((k, 10.0), (10.0, k), (10.0, 0.0));
        assert!(with_arcs(&p, &mm)[0][0].2 < 0.0);
        // A curve off any circle: flattened, no bulge.
        let mut p = BezPath::new();
        p.move_to((0.0, 0.0));
        p.curve_to((5.0, 20.0), (15.0, -20.0), (20.0, 0.0));
        let v = &with_arcs(&p, &mm)[0];
        assert!(v.len() > 4 && v.iter().all(|q| q.2 == 0.0), "{v:?}");
        // Degenerate curves are no arcs.
        assert!(circular((0.0, 0.0), (0.0, 0.0), (1.0, 1.0), (2.0, 0.0)).is_none());
        assert!(circular((0.0, 0.0), (1.0, 0.0), (2.0, 0.0), (3.0, 0.0)).is_none());
        assert!(circular((0.0, 0.0), (f64::NAN, 0.0), (2.0, 1.0), (3.0, 0.0)).is_none());
    }

    #[test]
    fn hostile_bytes_are_errors() {
        assert!(import_page(b"not a pdf", 1).is_err());
        assert!(import_page(&layered_pdf(), 9).is_err());
    }
}
