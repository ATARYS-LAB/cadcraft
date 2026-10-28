//! PDFIMPORT's "Join line and arc segments" (on by default, as in AutoCAD): open polylines whose
//! ends meet, with the same properties (layer, colour, lineweight…), become one polyline, closed
//! when the chain comes back to its start. PDFs drawn by CAD programs often stroke each segment
//! on its own; joined, a wall or a frame is picked as one object. Then, as AutoCAD imports
//! them, a lone arc is an ARC and a ring of arcs of one circle a CIRCLE.

use std::collections::HashMap;

use cadcraft_doc::{Arc, Circle, Common, EntityKind, LwPolyline};
use cadcraft_geom::{PolyVertex, Vec2, Vec3, bulge_to_arc};

/// One end of polyline `index`: its start (`false`) or its end (`true`).
type End = (usize, bool);

/// Join the open polylines among `objects` (layer, properties, object) whose ends lie within
/// `tol` of each other and whose properties are the same. Other objects are left as they are,
/// in their order; a joined polyline takes the place of the first of its pieces.
pub(super) fn join(objects: Vec<(String, Common, EntityKind)>, tol: f64) -> Vec<(String, Common, EntityKind)> {
    if !(tol.is_finite() && tol > 0.0) {
        return objects;
    }
    let open = |k: &EntityKind| matches!(k, EntityKind::LwPolyline(p) if !p.closed && p.vertices.len() >= 2);
    // Ends on a grid of `tol`, looked up with their neighbouring cells.
    let cell = |p: Vec2| ((p.x / tol).floor() as i64, (p.y / tol).floor() as i64);
    let ends = |k: &EntityKind| match k {
        EntityKind::LwPolyline(p) => p.vertices.first().zip(p.vertices.last()).map(|(a, b)| (a.p, b.p)),
        _ => None,
    };
    let mut grid: HashMap<(i64, i64), Vec<End>> = HashMap::new();
    for (i, (_, _, k)) in objects.iter().enumerate() {
        if open(k)
            && let Some((a, b)) = ends(k)
        {
            grid.entry(cell(a)).or_default().push((i, false));
            grid.entry(cell(b)).or_default().push((i, true));
        }
    }
    let mut used = vec![false; objects.len()];
    // The unused open polyline with the same properties as `of` with an end near `p`.
    let find = |p: Vec2, of: usize, used: &[bool], objects: &[(String, Common, EntityKind)]| -> Option<End> {
        let (cx, cy) = cell(p);
        let (layer, common) = objects.get(of).map(|(l, c, _)| (l, c))?;
        for dx in -1..=1 {
            for dy in -1..=1 {
                for &(j, at_end) in grid.get(&(cx + dx, cy + dy)).into_iter().flatten() {
                    if j == of || used.get(j).copied().unwrap_or(true) {
                        continue;
                    }
                    let Some((l, c, k)) = objects.get(j) else { continue };
                    let Some((a, b)) = ends(k) else { continue };
                    let q = if at_end { b } else { a };
                    if l == layer && c == common && q.dist(p) <= tol {
                        return Some((j, at_end));
                    }
                }
            }
        }
        None
    };
    let points = |k: &EntityKind| -> Vec<PolyVertex> {
        match k {
            EntityKind::LwPolyline(p) => p.vertices.iter().map(|v| PolyVertex::with_bulge(v.p, v.bulge)).collect(),
            _ => Vec::new(),
        }
    };
    let mut joined: Vec<Option<Vec<PolyVertex>>> = vec![None; objects.len()];
    // Pieces taken into another polyline's chain.
    let mut taken = vec![false; objects.len()];
    for i in 0..objects.len() {
        if used.get(i).copied().unwrap_or(true) || !objects.get(i).is_some_and(|(_, _, k)| open(k)) {
            continue;
        }
        if let Some(u) = used.get_mut(i) {
            *u = true;
        }
        let mut chain = objects.get(i).map(|(_, _, k)| points(k)).unwrap_or_default();
        let mut grew = false;
        // Forward from the chain's end, then backward from its start.
        for forward in [true, false] {
            loop {
                let tip = if forward { chain.last() } else { chain.first() };
                let Some(tip) = tip.map(|v| v.p) else { break };
                // Closed already: stop.
                if chain.len() > 2 && chain.first().zip(chain.last()).is_some_and(|(a, b)| a.p.dist(b.p) <= tol) {
                    break;
                }
                let Some((j, at_end)) = find(tip, i, &used, &objects) else { break };
                if let Some(u) = used.get_mut(j) {
                    *u = true;
                }
                if let Some(t) = taken.get_mut(j) {
                    *t = true;
                }
                let mut next = objects.get(j).map(|(_, _, k)| points(k)).unwrap_or_default();
                match (forward, at_end) {
                    // Appended: it must start at the tip.
                    (true, true) => reverse(&mut next),
                    // Prepended: it must end at the tip.
                    (false, false) => reverse(&mut next),
                    _ => {}
                }
                if forward {
                    // The tip goes on as the piece's start does.
                    if let (Some(t), Some(n)) = (chain.last_mut(), next.first()) {
                        t.bulge = n.bulge;
                    }
                    chain.extend(next.into_iter().skip(1));
                } else {
                    next.pop();
                    next.extend(chain);
                    chain = next;
                }
                grew = true;
            }
        }
        if grew && let Some(slot) = joined.get_mut(i) {
            *slot = Some(chain);
        }
    }
    let mut out = Vec::with_capacity(objects.len());
    for (i, (layer, common, kind)) in objects.into_iter().enumerate() {
        match joined.get_mut(i).and_then(Option::take) {
            Some(mut pts) => {
                // Two arcs make a ring; straight pieces at least three sides.
                let closed = (pts.len() > 3 || (pts.len() == 3 && pts.iter().any(|q| q.bulge != 0.0)))
                    && pts.first().zip(pts.last()).is_some_and(|(a, b)| a.p.dist(b.p) <= tol);
                if closed {
                    pts.pop();
                }
                let pl = LwPolyline { vertices: pts, closed, const_width: 0.0, elevation: 0.0, plinegen: false };
                out.push((layer, common, EntityKind::LwPolyline(pl)));
            }
            // A piece taken into another's chain goes; everything else stays.
            None if taken.get(i).copied().unwrap_or(false) => {}
            None => out.push((layer, common, kind)),
        }
    }
    out
}

/// `v` the other way round: each bulge goes with the other end of its segment, turned.
fn reverse(v: &mut [PolyVertex]) {
    let bulges: Vec<f64> = v.iter().map(|q| q.bulge).collect();
    v.reverse();
    let n = v.len();
    for (i, q) in v.iter_mut().enumerate() {
        // Segment i of the reversed polyline is segment n-2-i of the original.
        q.bulge = n.checked_sub(i + 2).and_then(|j| bulges.get(j)).map_or(0.0, |b| -b);
    }
}

/// `kind` as AutoCAD's PDFIMPORT leaves it: an open polyline of one arc an ARC, a closed one of
/// arcs of one circle a CIRCLE; anything else as it is.
pub(super) fn curve(kind: EntityKind) -> EntityKind {
    let EntityKind::LwPolyline(p) = &kind else { return kind };
    let v = &p.vertices;
    let z = p.elevation;
    match (p.closed, v.as_slice()) {
        (false, [a, b]) => match bulge_to_arc(a.p, b.p, a.bulge) {
            Some((arc, _)) => {
                EntityKind::Arc(Arc { center: Vec3::new(arc.center.x, arc.center.y, z), radius: arc.radius, start: arc.start, end: arc.end })
            }
            None => kind,
        },
        (true, [first, ..]) if v.len() >= 2 => {
            let arcs: Option<Vec<cadcraft_geom::Arc>> = v
                .iter()
                .zip(v.iter().cycle().skip(1))
                .map(|(a, b)| (a.bulge.signum() == first.bulge.signum()).then(|| bulge_to_arc(a.p, b.p, a.bulge)).flatten().map(|x| x.0))
                .collect();
            let Some(arcs) = arcs else { return kind };
            let Some(c) = arcs.first().copied() else { return kind };
            let tol = c.radius * 1e-3;
            if arcs.iter().all(|x| x.center.dist(c.center) <= tol && (x.radius - c.radius).abs() <= tol) {
                EntityKind::Circle(Circle { center: Vec3::new(c.center.x, c.center.y, z), radius: c.radius })
            } else {
                kind
            }
        }
        _ => kind,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cadcraft_color::Color;

    fn seg(a: (f64, f64), b: (f64, f64), color: Color) -> (String, Common, EntityKind) {
        let pl = LwPolyline {
            vertices: [a, b].map(|(x, y)| PolyVertex { p: Vec2::new(x, y), ..PolyVertex::default() }).to_vec(),
            closed: false,
            const_width: 0.0,
            elevation: 0.0,
            plinegen: false,
        };
        ("PDF_Geometry".into(), Common { color, ..Common::default() }, EntityKind::LwPolyline(pl))
    }

    fn polyline(o: &(String, Common, EntityKind)) -> (&[PolyVertex], bool) {
        match &o.2 {
            EntityKind::LwPolyline(p) => (&p.vertices, p.closed),
            _ => (&[], false),
        }
    }

    #[test]
    fn segments_drawn_one_by_one_become_one_polyline() {
        let red = Color::Index(1);
        // A rectangle in four strokes, the third drawn backwards.
        let out = join(
            vec![
                seg((0.0, 0.0), (10.0, 0.0), red),
                seg((10.0, 0.0), (10.0, 5.0), red),
                seg((0.0, 5.0), (10.0, 5.0), red),
                seg((0.0, 5.0), (0.0, 0.0), red),
            ],
            0.01,
        );
        assert_eq!(out.len(), 1);
        let (v, closed) = polyline(&out[0]);
        assert!(closed && v.len() == 4, "{v:?}");
        // An open chain, met from both ends of its first piece.
        let out = join(vec![seg((5.0, 0.0), (10.0, 0.0), red), seg((10.0, 0.0), (10.0, 5.0), red), seg((0.0, 0.0), (5.0, 0.0), red)], 0.01);
        assert_eq!(out.len(), 1);
        let (v, closed) = polyline(&out[0]);
        assert!(!closed && v.len() == 4 && v[0].p == Vec2::new(0.0, 0.0) && v[3].p == Vec2::new(10.0, 5.0), "{v:?}");
    }

    #[test]
    fn arcs_join_with_lines_and_a_lone_arc_or_a_ring_of_arcs_is_one() {
        let white = Color::Index(7);
        let quarter = (std::f64::consts::FRAC_PI_2 / 4.0).tan();
        let mut arc = seg((10.0, 0.0), (0.0, 10.0), white);
        if let EntityKind::LwPolyline(p) = &mut arc.2 {
            p.vertices[0].bulge = quarter;
        }
        // The line drawn towards the arc's end: the arc turned to follow it keeps its bulge, turned.
        let out = join(vec![seg((-5.0, 10.0), (0.0, 10.0), white), arc.clone()], 0.01);
        assert_eq!(out.len(), 1);
        let (v, closed) = polyline(&out[0]);
        assert!(!closed && v.len() == 3 && v[0].bulge == 0.0 && (v[1].bulge + quarter).abs() < 1e-12, "{v:?}");
        // Alone: an ARC, counter-clockwise from 0° to 90°.
        match curve(arc.2.clone()) {
            EntityKind::Arc(a) => assert!(
                a.center.xy().near(Vec2::ZERO, 1e-9)
                    && (a.radius - 10.0).abs() < 1e-9
                    && a.start.abs() < 1e-9
                    && (a.end - std::f64::consts::FRAC_PI_2).abs() < 1e-9,
                "{a:?}"
            ),
            k => panic!("{k:?}"),
        }
        // Two half turns of one circle, closed: a CIRCLE.
        let ring = LwPolyline {
            vertices: vec![PolyVertex::with_bulge(Vec2::new(10.0, 0.0), 1.0), PolyVertex::with_bulge(Vec2::new(-10.0, 0.0), 1.0)],
            closed: true,
            const_width: 0.0,
            elevation: 0.0,
            plinegen: false,
        };
        assert!(
            matches!(curve(EntityKind::LwPolyline(ring.clone())), EntityKind::Circle(c) if (c.radius - 10.0).abs() < 1e-9 && c.center.xy().near(Vec2::ZERO, 1e-9))
        );
        // A rounded slot (straight sides) and a straight piece stay polylines.
        let mut slot = ring;
        slot.vertices.insert(1, PolyVertex::new(Vec2::new(10.0, 5.0)));
        assert!(matches!(curve(EntityKind::LwPolyline(slot)), EntityKind::LwPolyline(_)));
        assert!(matches!(curve(seg((0.0, 0.0), (1.0, 0.0), white).2), EntityKind::LwPolyline(_)));
    }

    #[test]
    fn only_touching_ends_with_the_same_properties_join() {
        let (red, blue) = (Color::Index(1), Color::Index(5));
        // Another colour, or a gap wider than the tolerance: kept apart.
        assert_eq!(join(vec![seg((0.0, 0.0), (1.0, 0.0), red), seg((1.0, 0.0), (1.0, 1.0), blue)], 0.01).len(), 2);
        assert_eq!(join(vec![seg((0.0, 0.0), (1.0, 0.0), red), seg((1.05, 0.0), (1.0, 1.0), red)], 0.01).len(), 2);
        // A gap within it: joined.
        assert_eq!(join(vec![seg((0.0, 0.0), (1.0, 0.0), red), seg((1.005, 0.0), (1.0, 1.0), red)], 0.01).len(), 1);
        // Other objects stay, in their place.
        let text = ("PDF_Text".to_string(), Common::default(), EntityKind::Point(cadcraft_doc::Point { p: Default::default(), angle: 0.0 }));
        let out = join(vec![text.clone(), seg((0.0, 0.0), (1.0, 0.0), red), seg((1.0, 0.0), (2.0, 0.0), red)], 0.01);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], text);
        // No tolerance: nothing joined.
        assert_eq!(join(vec![seg((0.0, 0.0), (1.0, 0.0), red), seg((1.0, 0.0), (2.0, 0.0), red)], f64::NAN).len(), 2);
    }
}
