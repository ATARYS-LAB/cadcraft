//! What AutoCAD's Properties palette changes on a PDF underlay besides its adjustment (PDFADJUST)
//! and common properties: its position, rotation, scale, width or height (the extents along the
//! axes: either one scales the underlay uniformly), whether the page is shown and whether its
//! clip boundary is applied.

use cadcraft_doc::{EntityKind, Handle, PdfUnderlay};
use cadcraft_geom::Vec3;
use serde_json::{Value, json};

use super::super::{CommandSpec, bad};
use super::targets;
use crate::{Result, Session};

const ID: &str = "underlay.set";

pub(super) fn spec() -> CommandSpec {
    CommandSpec::new(ID, "PDF Underlay Properties", run).params(
        "{handles?: PDF underlays (default the selection), position?: [x,y] or [x,y,z], rotation?: degrees, scale?: > 0, width?: > 0, height?: > 0 (the extents along X or Y: the underlay scales uniformly to them), shown?: bool (the page shown), showClip?: bool (its clip boundary applied)} → {changed}",
    )
}

/// The width and height of underlay `u`'s page along the drawing's X and Y axes.
pub fn extents(u: &PdfUnderlay) -> (f64, f64) {
    let c = u.corners();
    let span = |f: fn(&cadcraft_geom::Vec2) -> f64| {
        let (lo, hi) = c.iter().map(f).fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), x| (lo.min(x), hi.max(x)));
        hi - lo
    };
    (span(|p| p.x), span(|p| p.y))
}

fn positive(p: &Value, key: &str) -> Result<Option<f64>> {
    match p.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => {
            v.as_f64().filter(|x| x.is_finite() && *x > 0.0).map(Some).ok_or_else(|| bad(ID, format!("`{key}` must be a number greater than 0")))
        }
    }
}

fn flag(p: &Value, key: &str) -> Result<Option<bool>> {
    match p.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v.as_bool().map(Some).ok_or_else(|| bad(ID, format!("`{key}` must be true or false"))),
    }
}

fn run(s: &mut Session, p: &Value) -> Result<Value> {
    let position = match p.get("position") {
        None | Some(Value::Null) => None,
        Some(Value::Array(a)) if (2..=3).contains(&a.len()) => {
            let n: Option<Vec<f64>> = a.iter().map(|v| v.as_f64().filter(|x| x.is_finite())).collect();
            let n = n.ok_or_else(|| bad(ID, "`position` must be [x,y] or [x,y,z] numbers"))?;
            Some((n.first().copied().unwrap_or(0.0), n.get(1).copied().unwrap_or(0.0), n.get(2).copied()))
        }
        Some(_) => return Err(bad(ID, "`position` must be [x,y] or [x,y,z]")),
    };
    let rotation = match p.get("rotation") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_f64().filter(|x| x.is_finite()).ok_or_else(|| bad(ID, "`rotation` must be a number of degrees"))?.to_radians()),
    };
    let (scale, width, height) = (positive(p, "scale")?, positive(p, "width")?, positive(p, "height")?);
    let (shown, show_clip) = (flag(p, "shown")?, flag(p, "showClip")?);
    let hs = targets(s, p)?;
    let d = s.doc()?;
    let ids: Vec<Handle> = hs
        .iter()
        .copied()
        .filter(|h| d.entity(*h).is_some_and(|e| matches!(e.kind, EntityKind::PdfUnderlay(_)) && !d.layer(&e.common.layer).is_some_and(|l| l.locked)))
        .collect();
    if ids.is_empty() {
        return Err(bad(ID, "no PDF underlay to change (select one, or pass `handles`; not on a locked layer)"));
    }
    let d = s.doc_mut()?;
    for h in &ids {
        d.modify_entity(*h, |e| {
            if let EntityKind::PdfUnderlay(u) = &mut e.kind {
                if let Some((x, y, z)) = position {
                    u.insert = Vec3::new(x, y, z.unwrap_or(u.insert.z));
                }
                if let Some(r) = rotation {
                    u.rotation = r;
                }
                if let Some(k) = scale {
                    u.scale = k;
                }
                // A width or height asked: the scale that gives it, kept uniform.
                let (w, h) = extents(u);
                let k = match (width, height) {
                    (Some(want), _) if w > 0.0 => Some(u.scale * want / w),
                    (None, Some(want)) if h > 0.0 => Some(u.scale * want / h),
                    _ => None,
                };
                if let Some(k) = k.filter(|k| k.is_finite() && *k > 0.0) {
                    u.scale = k;
                }
                if let Some(on) = shown {
                    u.hidden = !on;
                }
                if let Some(on) = show_clip {
                    u.clipping = on;
                }
            }
        })?;
    }
    Ok(json!({ "changed": ids.len() }))
}
