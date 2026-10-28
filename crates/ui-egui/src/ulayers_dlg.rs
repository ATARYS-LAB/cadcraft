//! The Underlay Layers dialog (ULAYERS from a menu or typed with a PDF underlay selected): the
//! layers of the underlay's PDF, sorted by name and filtered by a search, each shown or hidden on
//! that underlay. Every change goes
//! through the `ulayers` command; the list is read again only after one.

use serde_json::{Value, json};

use crate::CadApp;
use crate::i18n::t;

/// The layers as last read: (name, shown), or why they couldn't be.
type Listing = Result<Vec<(String, bool)>, String>;

fn id(handle: &str) -> egui::Id {
    egui::Id::unique(("cc_ulayers_dialog", handle.to_string()))
}

fn listing(r: &Result<Value, String>) -> Listing {
    let r = r.as_ref().map_err(Clone::clone)?;
    Ok(r["layers"]
        .as_array()
        .map(|a| a.iter().map(|l| (l["name"].as_str().unwrap_or_default().to_string(), l["on"].as_bool().unwrap_or(true))).collect())
        .unwrap_or_default())
}

/// The dialog, opened as `ulayers:<handle>` on the underlay chosen.
pub fn dialog(app: &mut CadApp, ctx: &egui::Context, handle: &str, open: &mut bool) {
    let listed: Listing = match ctx.data(|d| d.get_temp::<Listing>(id(handle))) {
        Some(l) => l,
        None => {
            let l = listing(&app.session.execute("ulayers", &json!({ "handle": handle })).map_err(|e| e.to_string()));
            ctx.data_mut(|d| d.insert_temp(id(handle), l.clone()));
            l
        }
    };
    let mut params: Option<Value> = None;
    egui::Window::new(t("Underlay Layers")).open(open).resizable(true).collapsible(false).default_width(320.0).show(ctx, |ui| match &listed {
        Err(e) => {
            ui.label(e.as_str());
        }
        Ok(layers) if layers.is_empty() => {
            ui.label(t("The PDF has no layers."));
        }
        Ok(layers) => {
            let search_id = id(handle).with("search");
            let mut search: String = ui.data(|d| d.get_temp(search_id)).unwrap_or_default();
            ui.horizontal(|ui| {
                ui.label(t("Search layer"));
                if ui.text_edit_singleline(&mut search).changed() {
                    ui.data_mut(|d| d.insert_temp(search_id, search.clone()));
                }
            });
            let mut shown_layers: Vec<&(String, bool)> =
                layers.iter().filter(|(n, _)| n.to_lowercase().contains(&search.trim().to_lowercase())).collect();
            shown_layers.sort_by_key(|(n, _)| n.to_lowercase());
            ui.horizontal(|ui| {
                if ui.button(t("Show all")).clicked() {
                    params = Some(json!({ "handle": handle, "on": ["*"] }));
                }
                if ui.button(t("Hide all")).clicked() {
                    params = Some(json!({ "handle": handle, "off": ["*"] }));
                }
            });
            ui.separator();
            egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                for (name, on) in shown_layers {
                    let mut shown = *on;
                    if ui.checkbox(&mut shown, name.as_str()).changed() {
                        let key = if shown { "on" } else { "off" };
                        params = Some(json!({ "handle": handle, key: [name] }));
                    }
                }
            });
        }
    });
    if let Some(p) = params {
        let l = listing(&app.run("ulayers", p).map_err(|e| e.to_string()));
        ctx.data_mut(|d| d.insert_temp(id(handle), l));
    }
    if !*open {
        ctx.data_mut(|d| d.remove::<Listing>(id(handle)));
    }
}
