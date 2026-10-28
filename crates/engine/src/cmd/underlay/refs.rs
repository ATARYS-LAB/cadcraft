//! The PDF files a drawing's underlays show, as AutoCAD's External References palette lists them:
//! one row per file and page (its definition, named "file - page"), with how many underlays show
//! it and whether it is found or unloaded; and what the palette does to one: detach it (every
//! underlay of it goes), unload or reload it (every underlay of it hides or shows again), and
//! change its path (every underlay of it follows).

use std::collections::BTreeMap;

use cadcraft_doc::{Drawing, EntityKind, Handle, PdfUnderlay};
use serde_json::{Value, json};

use super::super::{CommandSpec, bad, str_param};
use super::{load_lines, stored_path, underlay_file};
use crate::{Result, Session};

pub(super) fn specs() -> Vec<CommandSpec> {
    vec![
        CommandSpec::new("underlay.list", "PDF References", run_list)
            .params("{} → {pdfs: [{name, path, page, foundAt, status: Loaded|Unloaded|Not Found, references}]}")
            .noundo(),
        CommandSpec::new("underlay.detach", "Detach PDF", run_detach).params("{path: the stored path, as listed, page?: the page (default every page of the file)} → {detached}"),
        CommandSpec::new("underlay.unload", "Unload PDF", run_unload).params("{path: the stored path, as listed, page?: the page (default every page of the file)} → {unloaded}"),
        CommandSpec::new("underlay.reload", "Reload PDF", run_reload).params("{path: the stored path, as listed, page?: the page (default every page of the file)} → {reloaded}"),
        CommandSpec::new("underlay.path", "Change PDF Path", run_path)
            .params("{path: the stored path, as listed, page?: the page (default every page of the file), newPath: the file (full), pathType?: full|relative|none (default full)} → {changed, path}"),
    ]
}

/// The definition's name, as AutoCAD names it: the file name without extension, then the page.
pub fn definition_name(path: &str, page: u32) -> String {
    let file = path.trim().rsplit(['/', '\\']).next().unwrap_or("");
    let stem = file.rsplit_once('.').map(|(s, _)| s).unwrap_or(file).trim();
    format!("{} - {page}", if stem.is_empty() { "PDF" } else { stem })
}

/// Every PDF underlay of the drawing (model space, layouts and blocks), by stored path and page.
fn by_definition(d: &Drawing) -> BTreeMap<(String, u32), Vec<(Handle, PdfUnderlay)>> {
    let mut defs: BTreeMap<(String, u32), Vec<(Handle, PdfUnderlay)>> = BTreeMap::new();
    let stores = std::iter::once(&d.model).chain(d.layouts.iter().map(|l| &l.entities)).chain(d.blocks.values().map(|b| &b.entities));
    for e in stores.flat_map(|st| st.iter()) {
        if let EntityKind::PdfUnderlay(u) = &e.kind {
            defs.entry((u.path.clone(), u.page)).or_default().push((e.handle, u.clone()));
        }
    }
    defs
}

fn run_list(s: &mut Session, _p: &Value) -> Result<Value> {
    let document = s.state().ok().and_then(|st| st.path.clone());
    let list: Vec<Value> = by_definition(s.doc()?)
        .into_iter()
        .map(|((path, page), us)| {
            let found = underlay_file(&path, document.as_deref()).ok().map(|f| f.to_string_lossy().into_owned());
            let status = if us.iter().any(|(_, u)| u.unloaded) {
                "Unloaded"
            } else if found.is_some() {
                "Loaded"
            } else {
                "Not Found"
            };
            json!({
                "name": definition_name(&path, page),
                "path": path,
                "page": page,
                "foundAt": found,
                "status": status,
                "references": us.len(),
            })
        })
        .collect();
    Ok(json!({ "pdfs": list }))
}

/// The underlays the call names (`path`, `page?`), or an error naming the command.
fn chosen(s: &Session, p: &Value, cmd: &str) -> Result<Vec<(Handle, PdfUnderlay)>> {
    let path = str_param(p, "path").ok_or_else(|| bad(cmd, "`path` (the stored path, as `underlay.list` gives it) is required"))?;
    let page = match p.get("page") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_u64().and_then(|n| u32::try_from(n).ok()).ok_or_else(|| bad(cmd, "`page` must be a page number"))?),
    };
    let found: Vec<(Handle, PdfUnderlay)> =
        by_definition(s.doc()?).into_iter().filter(|((pa, pg), _)| pa == path && page.is_none_or(|n| n == *pg)).flat_map(|(_, us)| us).collect();
    if found.is_empty() {
        return Err(bad(cmd, format!("no PDF underlay shows {path}")));
    }
    Ok(found)
}

/// Change each of `hs` with `f`, wherever it is (a space or a block definition). Returns how many.
fn change(s: &mut Session, hs: &[Handle], f: impl Fn(&mut PdfUnderlay)) -> Result<usize> {
    let d = s.doc_mut()?;
    let mut n = 0usize;
    for h in hs {
        let set = |e: &mut cadcraft_doc::Entity| {
            if let EntityKind::PdfUnderlay(u) = &mut e.kind {
                f(u);
            }
        };
        if d.modify_entity(*h, set).is_ok() {
            n += 1;
            continue;
        }
        for b in d.blocks.values_mut() {
            if b.entities.get(*h).is_some() {
                std::sync::Arc::make_mut(b).entities.modify(*h, set);
                n += 1;
                break;
            }
        }
    }
    Ok(n)
}

fn run_detach(s: &mut Session, p: &Value) -> Result<Value> {
    let hs: Vec<Handle> = chosen(s, p, "underlay.detach")?.into_iter().map(|(h, _)| h).collect();
    let d = s.doc_mut()?;
    let mut n = 0usize;
    for h in &hs {
        if d.remove_entity(*h).is_some() {
            n += 1;
            continue;
        }
        for b in d.blocks.values_mut() {
            if b.entities.get(*h).is_some() {
                std::sync::Arc::make_mut(b).entities.remove(*h);
                n += 1;
                break;
            }
        }
    }
    Ok(json!({ "detached": n }))
}

fn run_unload(s: &mut Session, p: &Value) -> Result<Value> {
    let hs: Vec<Handle> = chosen(s, p, "underlay.unload")?.into_iter().map(|(h, _)| h).collect();
    let n = change(s, &hs, |u| u.unloaded = true)?;
    Ok(json!({ "unloaded": n }))
}

fn run_reload(s: &mut Session, p: &Value) -> Result<Value> {
    let us = chosen(s, p, "underlay.reload")?;
    let document = s.state().ok().and_then(|st| st.path.clone());
    let hs: Vec<Handle> = us.iter().map(|(h, _)| *h).collect();
    let n = change(s, &hs, |u| u.unloaded = false)?;
    // The file may have changed on disk: its lines read again for object snaps.
    for (_, u) in &us {
        super::forget_lines(&u.path);
        load_lines(&u.path, document.as_deref(), u.page, &u.layer_visibility());
    }
    Ok(json!({ "reloaded": n }))
}

fn run_path(s: &mut Session, p: &Value) -> Result<Value> {
    const ID: &str = "underlay.path";
    let us = chosen(s, p, ID)?;
    let new = str_param(p, "newPath").ok_or_else(|| bad(ID, "`newPath` is required"))?;
    let document = s.state().ok().and_then(|st| st.path.clone());
    let new_stored = stored_path(new, document.as_deref(), str_param(p, "pathType").unwrap_or("full")).map_err(|e| bad(ID, e))?;
    let hs: Vec<Handle> = us.iter().map(|(h, _)| *h).collect();
    let target = new_stored.clone();
    let n = change(s, &hs, move |u| u.path = target.clone())?;
    for (_, u) in &us {
        load_lines(&new_stored, document.as_deref(), u.page, &u.layer_visibility());
    }
    Ok(json!({ "changed": n, "path": new_stored }))
}
