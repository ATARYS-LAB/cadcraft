//! PDFIMPORT: a PDF page's vector content turned into drawing objects, from a PDF underlay (in
//! its place, its scale and rotation, inside its clip boundary, without the PDF layers it hides)
//! or from a PDF file placed like PDFATTACH places it. Stroked paths become polylines with their
//! lineweight (those whose ends meet joined into one, as AutoCAD's default), filled ones solids (half transparent; or solid hatches), runs of text multiline
//! texts in a `PDF <font>` style (0.716 of the font size high, attached middle left), as AutoCAD
//! imports them. Objects go on the PDF's layers (named `PDF_` and the PDF layer's name; a PDF
//! without layers: one layer per kind of object), on one layer per kind of object, or on the
//! current layer; a layer made takes its first object's colour (the nearest ACI colour) and
//! objects of that colour are ByLayer. Only what lies inside the area chosen comes. As in AutoCAD,
//! the underlay is unloaded afterwards unless asked to keep or detach it.

use cadcraft_color::Color;
use cadcraft_doc::{Common, EntityKind, Hatch, HatchLoop, Image, Lineweight, LwPolyline, MText, PdfUnderlay};
use cadcraft_geom::{PolyVertex, Vec2, Vec3};
use cadcraft_render::clip::PolyClip;
use serde_json::{Value, json};

use super::super::{CommandSpec, bad, bool_or, point_value, points_param, str_param, targets};
use super::{PageSize, pdf, read_pdf, underlay_file};
use crate::{Result, Session};

const ID: &str = "pdfimport";
/// Most objects one PDFIMPORT creates.
const MAX_OBJECTS: usize = 200_000;
/// How far apart, in millimetres on the page, two ends may be and still be joined.
const JOIN_TOLERANCE_MM: f64 = 0.01;

pub(super) fn spec() -> CommandSpec {
    CommandSpec::new(ID, "PDF Import", run)
        .menu(&["Insert", "PDF Import"])
        .interactive(|_| Ok(Box::new(ImportM::default())))
        .params("{handle?: a PDF underlay (default the selection) | path: PDF file with page?, at?, scale? (1 = size on paper), rotation?; layers?: \"pdf\" (default: the PDF's layers) | \"object\" (one layer per kind of object) | \"current\"; join?: bool (default true, as AutoCAD: open polylines whose ends meet, with the same properties, joined into one); fills?: bool (default true: filled areas as solids); hatches?: bool (default false: filled areas as solid hatches instead); texts?: bool (default true: text as single-line texts); lineweights?: bool (default true); images?: bool (default false, as AutoCAD: pictures as IMAGEs of PNG files in a \"PDF Images\" folder beside the PDF); area?: [[x,y],[x,y]] opposite corners | [[x,y],...] a polygon (default all); underlay?: \"unload\" (default, as AutoCAD) | \"keep\" | \"detach\" (with handle)} → {created, layers}")
}

/// The prefix of the layers made from the PDF's layers (`layers: "pdf"`).
const PDF_LAYER_PREFIX: &str = "PDF_";
/// Layers for objects of each kind (`layers: "object"`).
const GEOMETRY_LAYER: &str = "PDF_Geometry";
const FILLS_LAYER: &str = "PDF_Solid Fills";
const TEXT_LAYER: &str = "PDF_Text";
const IMAGES_LAYER: &str = "PDF_Images";
/// Where imported pictures go, beside the PDF (AutoCAD's PDFIMPORTIMAGEPATH default).
const IMAGES_FOLDER: &str = "PDF Images";
/// A text's height for a font size of 1, as AutoCAD's PDFIMPORT gives it.
const CAP_HEIGHT: f64 = 0.716;
/// Imported fills' transparency, as AutoCAD's PDFIMPORT gives them (percent).
const FILL_TRANSPARENCY: u8 = 50;

/// The nearest standard lineweight to `mm`.
fn lineweight(mm: f64) -> Lineweight {
    let want = mm * 100.0;
    let best = Lineweight::STANDARD.iter().copied().min_by(|a, b| (f64::from(*a) - want).abs().total_cmp(&(f64::from(*b) - want).abs())).unwrap_or(0);
    Lineweight::Mm100(best)
}

/// A file name in `dir` no file has yet: "images" and eight hexadecimal digits, as AutoCAD
/// names the pictures it takes out of a PDF.
fn free_name(dir: &std::path::Path, key: u128, index: usize) -> Option<std::path::PathBuf> {
    use std::hash::{Hash, Hasher};
    (0u32..10_000).find_map(|n| {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (key, index, n).hash(&mut h);
        let f = dir.join(format!("images{:08x}.png", h.finish() as u32));
        (!f.exists()).then_some(f)
    })
}

/// The standard colour (1 to 7) whose RGB is exactly `c`, black being 7 (black or white on the
/// background). AutoCAD writes these as index colours, any other RGB as a true colour.
fn exact_aci(c: cadcraft_color::Rgb) -> Option<u8> {
    if c == cadcraft_color::Rgb(0, 0, 0) {
        return Some(7);
    }
    (1..=7u8).find(|i| cadcraft_color::aci_rgb(*i) == c)
}

fn rgb(c: [u8; 3]) -> Color {
    Color::True(cadcraft_color::Rgb(c[0], c[1], c[2]))
}

/// The page PDFIMPORT reads and where it goes: an underlay's (and the underlay), or a file's
/// placed like PDFATTACH places it; with the PDF's bytes and file.
type Source = (PdfUnderlay, Option<cadcraft_doc::Handle>, Vec<u8>, std::path::PathBuf);

fn source(s: &Session, p: &Value, hooks: &super::PdfHooks) -> Result<Source> {
    let Some(path) = str_param(p, "path") else {
        let document = s.state().ok().and_then(|st| st.path.clone());
        let hs = targets(s, p)?;
        let d = s.doc()?;
        let (h, u) = hs
            .iter()
            .find_map(|h| match &d.entity(*h)?.kind {
                EntityKind::PdfUnderlay(u) => Some((*h, u.clone())),
                _ => None,
            })
            .ok_or_else(|| bad(ID, "no PDF underlay (select one, or pass `handle` or `path`)"))?;
        let file = underlay_file(&u.path, document.as_deref())?;
        let bytes = read_pdf(&file.to_string_lossy())?;
        return Ok((u, Some(h), bytes, file));
    };
    let page = p.get("page").and_then(Value::as_u64).filter(|n| *n >= 1).and_then(|n| u32::try_from(n).ok()).unwrap_or(1);
    let at = match p.get("at") {
        Some(v) => point_value(v).ok_or_else(|| bad(ID, "`at` must be a point [x, y]"))?,
        None => Vec2::ZERO,
    };
    let scale = match p.get("scale") {
        Some(v) => v.as_f64().filter(|k| k.is_finite() && (1e-9..=1e9).contains(k)).ok_or_else(|| bad(ID, "`scale` must be a positive number"))?,
        None => 1.0,
    };
    // Strict like the other parameters: "90" or true is refused, not taken as 0.
    let rotation = match p.get("rotation") {
        None | Some(Value::Null) => 0.0,
        Some(v) => v.as_f64().filter(|r| r.is_finite()).ok_or_else(|| bad(ID, "`rotation` must be a number of degrees"))?,
    };
    let bytes = read_pdf(path)?;
    let (_, w_mm, h_mm, per_point) = (hooks.page)(&bytes, page).map_err(|e| bad(ID, format!("{path}: {e}")))?;
    let size = PageSize::new(w_mm, h_mm, per_point).ok_or_else(|| bad(ID, format!("{path}: page {page} has no usable size")))?;
    let u = PdfUnderlay {
        insert: Vec3::new(at.x, at.y, 0.0),
        size: size.native,
        scale: size.file_scale(s.doc()?, scale),
        rotation: rotation.to_radians(),
        path: path.to_string(),
        page,
        ..PdfUnderlay::default()
    };
    Ok((u, None, bytes, std::path::PathBuf::from(path)))
}

/// What PDFIMPORT is asked for.
struct Options {
    /// "pdf", "object" or "current".
    layers: String,
    fills: bool,
    texts: bool,
    hatches: bool,
    lineweights: bool,
    join: bool,
    images: bool,
    /// "unload", "keep" or "detach".
    after: String,
    /// The area imported, in drawing units.
    area: Option<PolyClip>,
}

fn options(p: &Value) -> Result<Options> {
    let layers = str_param(p, "layers").unwrap_or("pdf").to_ascii_lowercase();
    if !matches!(layers.as_str(), "pdf" | "object" | "current") {
        return Err(bad(ID, "`layers` must be \"pdf\", \"object\" or \"current\""));
    }
    let after = match (str_param(p, "underlay"), bool_or(p, "detach", false)) {
        (_, true) => "detach".to_string(),
        (Some(a), _) => a.to_ascii_lowercase(),
        (None, _) => "unload".to_string(),
    };
    if !matches!(after.as_str(), "unload" | "keep" | "detach") {
        return Err(bad(ID, "`underlay` must be \"unload\", \"keep\" or \"detach\""));
    }
    let area = match p.get("area") {
        None | Some(Value::Null) => None,
        Some(_) => {
            let pts = points_param(p, "area").ok_or_else(|| bad(ID, "`area` must be two corners or a polygon [[x,y],...]"))?;
            let poly = match pts.as_slice() {
                [a, b] => vec![*a, Vec2::new(b.x, a.y), *b, Vec2::new(a.x, b.y)],
                _ if (3..=10_000).contains(&pts.len()) => pts,
                _ => return Err(bad(ID, "`area` must be two corners or a polygon [[x,y],...]")),
            };
            Some(PolyClip::new(&poly).ok_or_else(|| bad(ID, "the area encloses nothing"))?)
        }
    };
    Ok(Options {
        layers,
        fills: bool_or(p, "fills", true),
        texts: bool_or(p, "texts", true),
        hatches: bool_or(p, "hatches", false),
        lineweights: bool_or(p, "lineweights", true),
        join: bool_or(p, "join", true),
        images: bool_or(p, "images", false),
        after,
        area,
    })
}

/// The underlay's page as PDFIMPORT reads it: where a page point goes in the drawing, and what
/// the underlay shows (inside its clip boundary, outside when inverted, without the PDF layers it
/// hides) within the area asked for.
struct Shown<'a> {
    u: &'a PdfUnderlay,
    /// Page millimetres to the underlay's page units.
    k: f64,
    clip: Option<PolyClip>,
    area: Option<&'a PolyClip>,
    /// The layer objects go on in place of a PDF layer, and the way they are put on layers.
    current: String,
    mode: &'a str,
}

impl Shown<'_> {
    fn place(&self, q: (f64, f64)) -> Vec2 {
        self.u.to_drawing(Vec2::new(q.0 * self.k, q.1 * self.k))
    }
    fn hidden(&self, layer: &Option<String>) -> bool {
        layer.as_ref().is_some_and(|n| self.u.layers.iter().any(|l| &l.name == n && !l.on))
    }
    fn in_area(&self, q: Vec2) -> bool {
        self.area.is_none_or(|a| a.contains(q))
    }
    /// Point `q` is shown: inside the clip boundary (outside when inverted) and the area.
    fn shows(&self, q: Vec2) -> bool {
        self.clip.as_ref().is_none_or(|c| c.contains(q) != self.u.clip_inverted) && self.in_area(q)
    }
    /// Shown whole: nothing of it cut off by the clip boundary or the area.
    fn whole(&self, pts: &[Vec2]) -> bool {
        pts.iter().all(|q| self.shows(*q))
    }
    /// The parts of polyline `pts` shown.
    fn kept(&self, pts: &[Vec2]) -> Vec<Vec<Vec2>> {
        let shown = match &self.clip {
            None => vec![pts.to_vec()],
            Some(c) if self.u.clip_inverted => {
                if pts.iter().all(|q| !c.contains(*q)) {
                    vec![pts.to_vec()]
                } else {
                    Vec::new()
                }
            }
            Some(c) => c.clip_polyline(pts),
        };
        match self.area {
            None => shown,
            Some(a) => shown.iter().flat_map(|piece| a.clip_polyline(piece)).collect(),
        }
    }
    /// The layer an object of PDF layer `layer` goes on; `kind` with one layer per kind.
    fn layer(&self, layer: &Option<String>, kind: &str) -> String {
        match self.mode {
            "current" => self.current.clone(),
            "object" => kind.to_string(),
            _ => layer.as_ref().map(|n| format!("{PDF_LAYER_PREFIX}{n}")).unwrap_or_else(|| self.current.clone()),
        }
    }
}

/// Objects to add: their layer, properties and geometry.
type Objects = Vec<(String, Common, EntityKind)>;

/// Pictures (off by default, as AutoCAD): each a PNG file of its own in a "PDF Images" folder
/// beside the PDF (one per placement, never one already there), placed as an IMAGE under the
/// page's other objects.
fn pictures(s: &Session, sh: &Shown<'_>, images: &[super::PdfImage], pdf_file: std::path::PathBuf, objects: &mut Objects) -> Result<()> {
    let pdf_file = std::path::absolute(&pdf_file).unwrap_or(pdf_file);
    let dir = pdf_file.parent().map(|d| d.join(IMAGES_FOLDER)).ok_or_else(|| bad(ID, "the PDF's folder is unknown"))?;
    let mut dir_made = false;
    for (i, (png, (w, h), at, pu, pv, layer, key)) in images.iter().enumerate() {
        if sh.hidden(layer) || objects.len() >= MAX_OBJECTS || *w == 0 || *h == 0 {
            continue;
        }
        let (wf, hf) = (f64::from(*w), f64::from(*h));
        let o = sh.place(*at);
        let along = |d: &(f64, f64)| sh.place((at.0 + d.0, at.1 + d.1)) - o;
        let (pu, pv) = (along(pu), along(pv));
        // Kept when its middle is shown.
        let middle = o + pu * (wf / 2.0) + pv * (hf / 2.0);
        if !middle.is_finite() || !sh.shows(middle) {
            continue;
        }
        if !dir_made {
            std::fs::create_dir_all(&dir).map_err(|e| bad(ID, format!("{}: {e}", dir.display())))?;
            dir_made = true;
        }
        let file = free_name(&dir, *key, i).ok_or_else(|| bad(ID, format!("{}: no free file name", dir.display())))?;
        std::fs::write(&file, png).map_err(|e| bad(ID, format!("{}: {e}", file.display())))?;
        let image = Image {
            insert: o.to3(0.0),
            u: pu.to3(0.0),
            v: pv.to3(0.0),
            size: Vec2::new(wf, hf),
            path: file.to_string_lossy().into_owned(),
            name: file.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            // Shown, also when turned, with its clip boundary and transparency on.
            display: 15,
            // AutoCAD's default pixel size for these: one over the width.
            pixel_size: Vec2::new(1.0 / wf, 1.0 / wf),
            ..Image::default()
        };
        let l = sh.layer(layer, IMAGES_LAYER);
        objects.push((l.clone(), Common { layer: l, ..s.current_common() }, EntityKind::Image(image)));
    }
    Ok(())
}

/// A filled path's areas shown whole: one solid hatch, or solids (a four-sided area is one;
/// others are cut into triangles, even-odd).
fn fills(rings: &[&Vec<Vec2>], hatches: bool, l: &str, common: &Common, objects: &mut Objects) {
    if rings.is_empty() {
        return;
    }
    if hatches {
        let loops: Vec<HatchLoop> = rings
            .iter()
            .map(|pts| HatchLoop {
                vertices: pts.iter().map(|q| PolyVertex { p: *q, ..PolyVertex::default() }).collect(),
                outer: false,
                sources: Vec::new(),
            })
            .collect();
        let hatch = Hatch {
            pattern: "SOLID".into(),
            solid: true,
            loops,
            scale: 1.0,
            angle: 0.0,
            associative: false,
            style: 0,
            elevation: 0.0,
            gradient: None,
            origin: Vec2::ZERO,
            background: None,
            pattern_lines: Vec::new(),
        };
        objects.push((l.to_string(), common.clone(), EntityKind::Hatch(hatch)));
        return;
    }
    let quads: Vec<[Vec2; 4]> = match rings {
        // SOLID's corners go 1, 2, then 4, 3 (a bow tie otherwise).
        [r] if r.len() == 4 => match r.as_slice() {
            [a, b, c3, d4] => vec![[*a, *b, *d4, *c3]],
            _ => Vec::new(),
        },
        // Each area as triangles: its bounding rectangle cut to it.
        _ => rings
            .iter()
            .filter_map(|r| {
                let region = PolyClip::new(r)?;
                let b = region.bounds();
                let (p0, p1, p2, p3) = (b.min, Vec2::new(b.max.x, b.min.y), b.max, Vec2::new(b.min.x, b.max.y));
                Some(region.clip_triangles(&[p0, p1, p2, p0, p2, p3]))
            })
            .flat_map(|t| t.as_chunks::<3>().0.iter().map(|[a, b, c3]| [*a, *b, *c3, *c3]).collect::<Vec<_>>())
            .collect(),
    };
    for q in quads {
        if objects.len() >= MAX_OBJECTS {
            break;
        }
        objects.push((l.to_string(), common.clone(), EntityKind::Solid(cadcraft_doc::Solid { corners: q.map(|v| v.to3(0.0)) })));
    }
}

/// A stroked path's pieces shown: a piece with arcs shown whole keeps its arcs; the others are
/// polylines cut to what is shown.
fn strokes(sh: &Shown<'_>, pieces: &[(Vec<Vec2>, bool)], arcs: &[Vec<PolyVertex>], l: &str, common: &Common, objects: &mut Objects) {
    for (i, (pts, is_closed)) in pieces.iter().enumerate() {
        if let Some(v) = arcs.get(i).filter(|v| v.len() >= 2 && v.iter().any(|q| q.bulge != 0.0))
            && sh.whole(pts)
            && objects.len() < MAX_OBJECTS
        {
            let mut v = v.clone();
            let ring = v.len() > 2 && v.first().zip(v.last()).is_some_and(|(a, b)| a.p.near(b.p, 1e-9));
            if ring {
                v.pop();
            }
            let pl = LwPolyline { vertices: v, closed: ring || *is_closed, const_width: 0.0, elevation: 0.0, plinegen: false };
            objects.push((l.to_string(), common.clone(), EntityKind::LwPolyline(pl)));
            continue;
        }
        let mut pts = pts.clone();
        if *is_closed && let Some(f) = pts.first().copied() {
            pts.push(f);
        }
        for mut part in sh.kept(&pts) {
            if part.len() < 2 || objects.len() >= MAX_OBJECTS {
                continue;
            }
            let ring = part.len() > 3 && part.first().zip(part.last()).is_some_and(|(a, b)| a.near(*b, 1e-9));
            if ring {
                part.pop();
            }
            let pl = LwPolyline {
                vertices: part.iter().map(|q| PolyVertex { p: *q, ..PolyVertex::default() }).collect(),
                closed: ring,
                const_width: 0.0,
                elevation: 0.0,
                plinegen: false,
            };
            objects.push((l.to_string(), common.clone(), EntityKind::LwPolyline(pl)));
        }
    }
}

/// One path of the page: its fill (when asked) and its stroke.
fn path(s: &Session, sh: &Shown<'_>, o: &Options, path: super::PdfPath, objects: &mut Objects) {
    let (pieces, closed, stroke, fill, layer, arcs) = path;
    if sh.hidden(&layer) || objects.len() >= MAX_OBJECTS {
        return;
    }
    let pieces: Vec<(Vec<Vec2>, bool)> =
        pieces.iter().zip(closed.iter().chain(std::iter::repeat(&false))).map(|(pc, c)| (pc.iter().map(|q| sh.place(*q)).collect(), *c)).collect();
    // The same pieces with their arcs, when read for each of them.
    let arcs: Vec<Vec<PolyVertex>> = if arcs.len() == pieces.len() {
        arcs.iter()
            .map(|pc| pc.iter().map(|(x, y, b)| PolyVertex::with_bulge(sh.place((*x, *y)), if b.is_finite() { *b } else { 0.0 })).collect())
            .collect()
    } else {
        Vec::new()
    };
    if let Some(c) = fill.filter(|_| o.fills) {
        let rings: Vec<&Vec<Vec2>> = pieces.iter().filter(|(pts, _)| pts.len() >= 3 && sh.whole(pts)).map(|(pts, _)| pts).collect();
        let l = sh.layer(&layer, FILLS_LAYER);
        let common =
            Common { layer: l.clone(), color: rgb(c), transparency: cadcraft_doc::Transparency::Percent(FILL_TRANSPARENCY), ..s.current_common() };
        fills(&rings, o.hatches, &l, &common, objects);
    }
    if let Some((c, w)) = stroke {
        let l = sh.layer(&layer, GEOMETRY_LAYER);
        let lw = if o.lineweights { lineweight(w) } else { Lineweight::ByLayer };
        let common = Common { layer: l.clone(), color: rgb(c), lineweight: lw, ..s.current_common() };
        strokes(sh, &pieces, &arcs, &l, &common, objects);
    }
}

/// The runs of text shown, as multiline texts; the `PDF <font>` styles they use (name, font).
fn texts(s: &Session, sh: &Shown<'_>, texts: Vec<super::PdfText>, objects: &mut Objects) -> Vec<(String, String)> {
    let mut styles: Vec<(String, String)> = Vec::new();
    for (value, at, size, rotation, c, layer, font) in texts {
        if sh.hidden(&layer) || objects.len() >= MAX_OBJECTS {
            continue;
        }
        let origin = sh.place(at);
        if !sh.shows(origin) {
            continue;
        }
        let height = size * sh.k * sh.u.scale * CAP_HEIGHT;
        if !(height.is_finite() && height > 0.0) {
            continue;
        }
        let l = sh.layer(&layer, TEXT_LAYER);
        // A `PDF <font>` style for each font, as AutoCAD makes them.
        let style = match &font {
            Some(f) => {
                let name = format!("PDF {f}");
                if !styles.iter().any(|(n, _)| n == &name) {
                    styles.push((name.clone(), f.clone()));
                }
                name
            }
            None => "Standard".to_string(),
        };
        // Attached middle left: half the height above the baseline.
        let angle = rotation + sh.u.rotation;
        let up = Vec2::new(-angle.sin(), angle.cos());
        let mtext = MText {
            insert: (origin + up * (height / 2.0)).to3(0.0),
            height,
            width: 0.0,
            attach: 4,
            rotation: angle,
            style,
            contents: value.replace('\\', "\\\\").replace('{', "\\{").replace('}', "\\}"),
            line_spacing: 1.0,
            line_spacing_exact: false,
            columns: None,
        };
        objects.push((l.clone(), Common { layer: l, color: rgb(c), ..s.current_common() }, EntityKind::MText(mtext)));
    }
    styles
}

/// Add `objects` and the text styles they use to the drawing: a layer made here takes its first
/// object's colour (the nearest ACI colour), and objects of the layer's colour are ByLayer.
/// Returns how many were added and the layers made.
fn write(s: &mut Session, objects: Objects, styles: Vec<(String, String)>, current: &str) -> Result<(usize, Vec<String>)> {
    let space = s.space();
    let d = s.doc_mut()?;
    for (name, font) in styles {
        if d.text_style(&name).is_none() {
            d.text_styles.push(cadcraft_doc::TextStyle { name, font, ..cadcraft_doc::TextStyle::default() });
        }
    }
    let aci = |c: cadcraft_color::Rgb| exact_aci(c).unwrap_or_else(|| cadcraft_color::nearest_aci(c));
    let (mut created, mut layers_used) = (0usize, Vec::new());
    for (layer, mut common, kind) in objects {
        let made = layer != current
            && (layer.starts_with(PDF_LAYER_PREFIX) || [GEOMETRY_LAYER, FILLS_LAYER, TEXT_LAYER, IMAGES_LAYER].contains(&layer.as_str()));
        if d.ensure_layer(&layer) {
            layers_used.push(layer.clone());
            if let (Some(l), Color::True(c)) = (d.layer_mut(&layer), common.color) {
                l.color = Color::Index(aci(c));
            }
        }
        if made
            && let (Some(Color::Index(li)), Color::True(c)) = (d.layer(&layer).map(|l| l.color), common.color)
            && aci(c) == li
        {
            common.color = Color::ByLayer;
        }
        // An index colour's RGB is that index colour, as AutoCAD writes it.
        if let Color::True(c) = common.color
            && let Some(i) = exact_aci(c)
        {
            common.color = Color::Index(i);
        }
        d.add(&space, common, kind)?;
        created += 1;
    }
    Ok((created, layers_used))
}

/// The underlay once imported: detached, or unloaded with every underlay of the same file and
/// page (they share the file's state), or kept.
fn after_import(s: &mut Session, after: &str, underlay: Option<cadcraft_doc::Handle>, u: &PdfUnderlay) -> Result<()> {
    let Some(h) = underlay else { return Ok(()) };
    let d = s.doc_mut()?;
    match after {
        "detach" => {
            d.remove_entity(h);
        }
        "unload" => {
            let stores = std::iter::once(&mut d.model).chain(d.layouts.iter_mut().map(|l| &mut l.entities));
            for st in stores {
                let hs: Vec<_> = st
                    .iter()
                    .filter(|e| matches!(&e.kind, EntityKind::PdfUnderlay(o) if o.path == u.path && o.page == u.page))
                    .map(|e| e.handle)
                    .collect();
                for h in hs {
                    st.modify(h, |e| {
                        if let EntityKind::PdfUnderlay(o) = &mut e.kind {
                            o.unloaded = true;
                        }
                    });
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn run(s: &mut Session, p: &Value) -> Result<Value> {
    let hooks = pdf().ok_or_else(|| bad(ID, "PDF import is not available in this build"))?;
    let (u, underlay, bytes, pdf_file) = source(s, p, hooks)?;
    let o = options(p)?;
    let (_, w_mm, _, _) = (hooks.page)(&bytes, u.page).map_err(|e| bad(ID, format!("{}: {e}", u.path)))?;
    let (paths, page_texts, images) = (hooks.import)(&bytes, u.page).map_err(|e| bad(ID, format!("{}: {e}", u.path)))?;
    // A PDF without layers: one layer per kind of object, as AutoCAD falls back to.
    let has_layers = paths.iter().any(|p| p.4.is_some()) || page_texts.iter().any(|t| t.5.is_some()) || images.iter().any(|i| i.5.is_some());
    let mode = if o.layers == "pdf" && !has_layers { "object" } else { o.layers.as_str() };
    let current = s.current_common().layer;
    let sh = Shown {
        u: &u,
        k: if w_mm > 0.0 { u.size.x / w_mm } else { 0.0 },
        clip: u.clip_outline().and_then(|c| PolyClip::new(&c)),
        area: o.area.as_ref(),
        current: current.clone(),
        mode,
    };
    let mut objects: Objects = Vec::new();
    if o.images && !images.is_empty() {
        pictures(s, &sh, &images, pdf_file, &mut objects)?;
    }
    for pa in paths {
        path(s, &sh, &o, pa, &mut objects);
    }
    if o.join {
        // Ends a hundredth of a millimetre apart on the page meet.
        objects = super::join::join(objects, JOIN_TOLERANCE_MM * sh.k * u.scale);
    }
    // A lone arc is an arc, a ring of arcs of one circle a circle, as AutoCAD imports them.
    objects = objects.into_iter().map(|(l, c, kind)| (l, c, super::join::curve(kind))).collect();
    let styles = if o.texts { texts(s, &sh, page_texts, &mut objects) } else { Vec::new() };
    let (created, layers_used) = write(s, objects, styles, &current)?;
    after_import(s, &o.after, underlay, &u)?;
    Ok(json!({ "created": created, "layers": layers_used }))
}

#[derive(Default, PartialEq)]
enum Stage {
    #[default]
    Select,
    File,
    Page,
    Point,
    Scale,
    Rotation,
    Area,
    Opposite,
    Polygon,
    Layers,
    After,
}

/// PDFIMPORT typed or from a menu: an underlay (or [File]: its name, page, insertion point,
/// scale and rotation), the area to import (a window, a polygon or All; Settings: the layers the
/// objects go on), then whether the underlay is kept, detached or unloaded.
#[derive(Default)]
pub(super) struct ImportM {
    stage: Stage,
    sel: super::super::machines::SelectPhase,
    params: serde_json::Map<String, Value>,
    pts: Vec<Vec2>,
}

impl ImportM {
    fn number(i: &crate::Input) -> Option<f64> {
        match i {
            crate::Input::Text(t) | crate::Input::Keyword(t) => t.trim().parse::<f64>().ok(),
            _ => None,
        }
        .filter(|n| n.is_finite())
    }
    fn finish(&mut self, s: &mut Session) -> Result<crate::Step> {
        let p = Value::Object(std::mem::take(&mut self.params));
        match run(s, &p) {
            Ok(r) => s.echo(format!("{} objects imported.", r["created"].as_u64().unwrap_or(0))),
            Err(e) => s.echo(e.to_string()),
        }
        Ok(crate::Step::Done)
    }
}

impl crate::Interactive for ImportM {
    fn name(&self) -> &'static str {
        "PDFIMPORT"
    }
    fn begin(&mut self, s: &mut Session) -> Result<crate::Step> {
        self.sel = super::super::machines::SelectPhase::single();
        // An underlay already selected is the one imported.
        let d = s.doc()?;
        if let Some(h) = s.selection().into_iter().find(|h| d.entity(*h).is_some_and(|e| matches!(e.kind, EntityKind::PdfUnderlay(_)))) {
            s.set_selection(Vec::new());
            self.params.insert("handle".into(), json!(h.hex()));
            self.stage = Stage::Area;
        }
        Ok(crate::Step::Continue)
    }
    fn prompt(&self, _s: &Session) -> crate::Prompt {
        use crate::{Accept, Prompt};
        match self.stage {
            Stage::Select => Prompt::new("Select PDF underlay", Accept::SELECT).kw(&["File"]).default("File"),
            Stage::File => Prompt::new("Enter file name for PDF import", Accept::TEXT),
            Stage::Page => Prompt::new("Enter page number", Accept::NUMBER).default("1"),
            Stage::Point => Prompt::new("Specify insertion point", Accept::POINT).default("0,0"),
            Stage::Scale => Prompt::new("Specify scale factor", Accept::NUMBER).default("1.0"),
            Stage::Rotation => Prompt::new("Specify rotation", Accept::NUMBER).default("0"),
            Stage::Area => Prompt::new("Specify first corner of area to import", Accept::POINT).kw(&["Polygonal", "All", "Settings"]).default("All"),
            Stage::Opposite => match self.pts.first() {
                Some(a) => Prompt::new("Specify opposite corner", Accept::POINT).corner(*a),
                None => Prompt::new("Specify opposite corner", Accept::POINT),
            },
            Stage::Polygon => {
                let p = Prompt::new(if self.pts.is_empty() { "Specify first point" } else { "Specify next point" }, Accept::POINT)
                    .base_opt(self.pts.last().copied());
                if self.pts.is_empty() { p } else { p.kw(&["Undo"]) }
            }
            Stage::Layers => Prompt::new("Objects go on", Accept::TEXT).kw(&["Pdf layers", "Object layers", "Current layer"]).default("Pdf layers"),
            Stage::After => Prompt::new("Keep, detach or unload the PDF underlay?", Accept::TEXT).kw(&["Keep", "Detach", "Unload"]).default("Unload"),
        }
    }
    fn preview(&self, _s: &Session, cursor: Vec2) -> Vec<EntityKind> {
        let outline: Vec<Vec2> = match (&self.stage, self.pts.first()) {
            (Stage::Opposite, Some(a)) => vec![*a, Vec2::new(cursor.x, a.y), cursor, Vec2::new(a.x, cursor.y)],
            (Stage::Polygon, Some(_)) => self.pts.iter().copied().chain([cursor]).collect(),
            _ => return Vec::new(),
        };
        let n = outline.len();
        (0..n)
            .filter_map(|i| {
                let (a, b) = (outline.get(i)?, outline.get((i + 1) % n)?);
                (a != b).then(|| EntityKind::Line(cadcraft_doc::Line { a: a.to3(0.0), b: b.to3(0.0) }))
            })
            .collect()
    }
    fn input(&mut self, s: &mut Session, i: crate::Input) -> Result<crate::Step> {
        use crate::{Input, Step};
        let text = match &i {
            Input::Keyword(k) | Input::Text(k) => Some(k.trim().to_ascii_lowercase()),
            _ => None,
        };
        match self.stage {
            Stage::Select => match (&i, text.as_deref()) {
                (Input::Enter, _) => self.stage = Stage::File,
                (_, Some(t)) if t.starts_with('f') => self.stage = Stage::File,
                _ => match self.sel.feed(s, &i)? {
                    super::super::machines::SelOutcome::More => {}
                    super::super::machines::SelOutcome::Empty => self.stage = Stage::File,
                    super::super::machines::SelOutcome::Done(hs) => {
                        s.set_selection(Vec::new());
                        let d = s.doc()?;
                        match hs.iter().find(|h| d.entity(**h).is_some_and(|e| matches!(e.kind, EntityKind::PdfUnderlay(_)))) {
                            Some(h) => {
                                self.params.insert("handle".into(), json!(h.hex()));
                                self.stage = Stage::Area;
                            }
                            None => s.echo("Select a PDF underlay's frame."),
                        }
                    }
                },
            },
            Stage::File => match &i {
                Input::Text(t) | Input::Keyword(t) if !t.trim().is_empty() => {
                    self.params.insert("path".into(), json!(t.trim()));
                    self.stage = Stage::Page;
                }
                Input::Enter | Input::Cancel => return Ok(Step::Done),
                _ => {}
            },
            Stage::Page => {
                let page = match &i {
                    Input::Enter => Some(1.0),
                    _ => Self::number(&i).filter(|n| n.fract() == 0.0 && *n >= 1.0),
                };
                match page {
                    Some(n) => {
                        self.params.insert("page".into(), json!(n as u64));
                        self.stage = Stage::Point;
                    }
                    None => s.echo("Requires a page number from 1."),
                }
            }
            Stage::Point => {
                match &i {
                    Input::Point(p) => {
                        self.params.insert("at".into(), json!([p.x, p.y]));
                    }
                    Input::Enter => {}
                    _ => return Ok(Step::Continue),
                }
                self.stage = Stage::Scale;
            }
            Stage::Scale => {
                match (&i, Self::number(&i)) {
                    (Input::Enter, _) => {}
                    (_, Some(k)) if k > 0.0 => {
                        self.params.insert("scale".into(), json!(k));
                    }
                    _ => {
                        s.echo("Requires a positive number.");
                        return Ok(Step::Continue);
                    }
                }
                self.stage = Stage::Rotation;
            }
            Stage::Rotation => {
                match (&i, Self::number(&i)) {
                    (Input::Enter, _) => {}
                    (_, Some(a)) => {
                        self.params.insert("rotation".into(), json!(a));
                    }
                    _ => return Ok(Step::Continue),
                }
                return self.finish(s);
            }
            Stage::Area => match (&i, text.as_deref()) {
                (Input::Point(p), _) => {
                    self.pts = vec![*p];
                    self.stage = Stage::Opposite;
                }
                (Input::Enter, _) | (_, Some("a" | "all")) => self.stage = Stage::After,
                (_, Some(t)) if t.starts_with('p') => {
                    self.pts.clear();
                    self.stage = Stage::Polygon;
                }
                (_, Some(t)) if t.starts_with('s') => self.stage = Stage::Layers,
                _ => s.echo("Invalid option keyword."),
            },
            Stage::Opposite => {
                if let (Input::Point(b), Some(a)) = (&i, self.pts.first()) {
                    self.params.insert("area".into(), json!([[a.x, a.y], [b.x, b.y]]));
                    self.stage = Stage::After;
                }
            }
            Stage::Polygon => match (&i, text.as_deref()) {
                (Input::Point(p), _) => self.pts.push(*p),
                (_, Some("u" | "undo")) => {
                    self.pts.pop();
                }
                (Input::Enter, _) if self.pts.len() >= 3 => {
                    let pts: Vec<[f64; 2]> = self.pts.iter().map(|q| [q.x, q.y]).collect();
                    self.params.insert("area".into(), json!(pts));
                    self.stage = Stage::After;
                }
                (Input::Enter, _) => s.echo("An area needs at least 3 points."),
                _ => {}
            },
            Stage::Layers => {
                let mode = match (&i, text.as_deref()) {
                    (Input::Enter, _) => Some("pdf"),
                    (_, Some(t)) if t.starts_with('p') => Some("pdf"),
                    (_, Some(t)) if t.starts_with('o') => Some("object"),
                    (_, Some(t)) if t.starts_with('c') => Some("current"),
                    _ => None,
                };
                match mode {
                    Some(m) => {
                        self.params.insert("layers".into(), json!(m));
                        self.stage = Stage::Area;
                    }
                    None => s.echo("Invalid option keyword."),
                }
            }
            Stage::After => {
                let after = match (&i, text.as_deref()) {
                    (Input::Enter, _) => Some("unload"),
                    (_, Some(t)) if t.starts_with('k') => Some("keep"),
                    (_, Some(t)) if t.starts_with('d') => Some("detach"),
                    (_, Some(t)) if t.starts_with('u') => Some("unload"),
                    _ => None,
                };
                match after {
                    Some(a) => {
                        self.params.insert("underlay".into(), json!(a));
                        return self.finish(s);
                    }
                    None => s.echo("Invalid option keyword."),
                }
            }
        }
        Ok(Step::Continue)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_standard_colours_are_index_colours_others_true_colours() {
        use cadcraft_color::Rgb;
        assert_eq!(exact_aci(Rgb(0, 0, 255)), Some(5));
        assert_eq!(exact_aci(Rgb(255, 0, 255)), Some(6));
        assert_eq!(exact_aci(Rgb(0, 0, 0)), Some(7));
        assert_eq!(exact_aci(Rgb(255, 255, 255)), Some(7));
        // A green between the standard ones stays a true colour, as AutoCAD writes it.
        assert_eq!(exact_aci(Rgb(0, 153, 0)), None);
    }
}
