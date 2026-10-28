//! ULAYERS: the layers of a PDF underlay's PDF, each shown or hidden on that underlay (as in
//! AutoCAD, per underlay: another underlay of the same PDF keeps its own). Hidden layers leave
//! the page on screen, on plots and for object snaps.

use cadcraft_doc::{EntityKind, Handle, PdfUnderlay, UnderlayLayer};
use serde_json::{Value, json};

use super::super::machines::{SelOutcome, SelectPhase};
use super::super::qselect::wildcard;
use super::super::{CommandSpec, bad, targets};
use super::{load_lines, pdf, read_pdf, underlay_file};
use crate::{Accept, Input, Interactive, Prompt, Result, Session, Step};

const ID: &str = "ulayers";

pub(super) fn spec() -> CommandSpec {
    CommandSpec::new(ID, "Underlay Layers...", run)
        .menu(&["Modify", "Object", "PDF Underlay", "Layers..."])
        .params("{handle?|handles?: a PDF underlay (default the selection), on?: [layer names or patterns], off?: [layer names or patterns]} → {layers: [{name, on}]} (with neither: the layers and their state)")
        .interactive(|_| Ok(Box::new(LayersM::default())))
}

/// The PDF layers of underlay `u` as it shows them: (name, shown).
fn shown(u: &PdfUnderlay, document: Option<&str>) -> Result<Vec<(String, bool)>> {
    let hooks = pdf().ok_or_else(|| bad(ID, "PDF underlays are not available in this build"))?;
    let path = underlay_file(&u.path, document)?;
    let bytes = read_pdf(&path.to_string_lossy())?;
    let defaults = (hooks.layers)(&bytes).map_err(|e| bad(ID, format!("{}: {e}", u.path)))?;
    Ok(defaults
        .into_iter()
        .map(|(name, on)| {
            let on = u.layers.iter().find(|l| l.name == name).map_or(on, |l| l.on);
            (name, on)
        })
        .collect())
}

fn listing(layers: &[(String, bool)]) -> Value {
    Value::Array(layers.iter().map(|(name, on)| json!({ "name": name, "on": on })).collect())
}

/// The first PDF underlay among `hs`, with the drawing's path.
fn underlay(s: &Session, hs: &[Handle]) -> Option<(Handle, PdfUnderlay, Option<String>)> {
    let st = s.state().ok()?;
    hs.iter().find_map(|h| match &st.doc.entity(*h)?.kind {
        EntityKind::PdfUnderlay(u) => Some((*h, u.clone(), st.path.clone())),
        _ => None,
    })
}

/// Turn the layers matching `on` / `off` (names or wildcard patterns, case ignored) on or off
/// on underlay `h`. Returns the layers as now shown.
fn set(s: &mut Session, h: Handle, on: &[String], off: &[String]) -> Result<Vec<(String, bool)>> {
    let (_, u, document) = underlay(s, &[h]).ok_or_else(|| bad(ID, "not a PDF underlay"))?;
    let d = s.doc()?;
    if d.entity(h).is_some_and(|e| d.layer(&e.common.layer).is_some_and(|l| l.locked)) {
        return Err(bad(ID, "the PDF underlay is on a locked layer"));
    }
    let matches = |pats: &[String], name: &str| pats.iter().any(|p| wildcard(&p.to_uppercase(), &name.to_uppercase()));
    let now: Vec<(String, bool)> = shown(&u, document.as_deref())?
        .into_iter()
        .map(|(n, v)| {
            (
                n.clone(),
                if matches(off, &n) {
                    false
                } else if matches(on, &n) {
                    true
                } else {
                    v
                },
            )
        })
        .collect();
    let defaults = shown(&PdfUnderlay { layers: Vec::new(), ..u.clone() }, document.as_deref())?;
    // Keep only what differs from the PDF's own default.
    let layers: Vec<UnderlayLayer> = now
        .iter()
        .filter(|(n, v)| defaults.iter().find(|(dn, _)| dn == n).is_none_or(|(_, dv)| dv != v))
        .map(|(n, v)| UnderlayLayer { name: n.clone(), on: *v })
        .collect();
    let set = layers.clone();
    s.doc_mut()?.modify_entity(h, |e| {
        if let EntityKind::PdfUnderlay(u) = &mut e.kind {
            u.layers = set;
        }
    })?;
    let visibility: Vec<(String, bool)> = layers.iter().map(|l| (l.name.clone(), l.on)).collect();
    load_lines(&u.path, document.as_deref(), u.page, &visibility);
    Ok(now)
}

fn names(p: &Value, key: &str) -> Result<Vec<String>> {
    match p.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(a)) => {
            a.iter().map(|v| v.as_str().map(str::to_string).ok_or_else(|| bad(ID, format!("`{key}` must be a list of layer names")))).collect()
        }
        Some(Value::String(t)) => Ok(t.split(',').map(|n| n.trim().to_string()).filter(|n| !n.is_empty()).collect()),
        Some(_) => Err(bad(ID, format!("`{key}` must be a list of layer names"))),
    }
}

fn run(s: &mut Session, p: &Value) -> Result<Value> {
    let hs = targets(s, p)?;
    let (h, u, document) = underlay(s, &hs).ok_or_else(|| bad(ID, "no PDF underlay (select one, or pass `handle`)"))?;
    let (on, off) = (names(p, "on")?, names(p, "off")?);
    let layers = if on.is_empty() && off.is_empty() { shown(&u, document.as_deref())? } else { set(s, h, &on, &off)? };
    Ok(json!({ "layers": listing(&layers) }))
}

#[derive(Default, PartialEq)]
enum Stage {
    #[default]
    Select,
    Option,
    Names(bool),
}

/// ULAYERS typed: select an underlay, then ? (list), ON or OFF and the layer names (patterns
/// allowed, comma separated), until Enter.
#[derive(Default)]
struct LayersM {
    stage: Stage,
    sel: SelectPhase,
    target: Option<Handle>,
}

impl LayersM {
    fn selected(&mut self, s: &mut Session, hs: Vec<Handle>) -> Result<Step> {
        s.set_selection(Vec::new());
        match underlay(s, &hs) {
            Some((h, _, _)) => {
                self.target = Some(h);
                self.stage = Stage::Option;
                Ok(Step::Continue)
            }
            None => {
                s.echo("No PDF underlay selected.");
                Ok(Step::Done)
            }
        }
    }
    fn list(&self, s: &mut Session) {
        let Some((_, u, document)) = self.target.and_then(|h| underlay(s, &[h])) else { return };
        match shown(&u, document.as_deref()) {
            Ok(l) if l.is_empty() => s.echo("The PDF has no layers."),
            Ok(l) => {
                for (name, on) in l {
                    s.echo(format!("  {name}: {}", if on { "on" } else { "off" }));
                }
            }
            Err(e) => s.echo(e.to_string()),
        }
    }
}

impl Interactive for LayersM {
    fn name(&self) -> &'static str {
        "ULAYERS"
    }
    fn begin(&mut self, s: &mut Session) -> Result<Step> {
        self.sel = SelectPhase::single();
        let pre = s.selection();
        if !pre.is_empty() {
            return self.selected(s, pre);
        }
        Ok(Step::Continue)
    }
    fn prompt(&self, _s: &Session) -> Prompt {
        match self.stage {
            Stage::Select => self.sel.prompt(),
            Stage::Option => Prompt::new("Enter an option", Accept::TEXT).kw(&["?", "ON", "OFF"]),
            Stage::Names(on) => Prompt::new(if on { "Enter PDF layer names to turn on" } else { "Enter PDF layer names to turn off" }, Accept::TEXT),
        }
    }
    fn input(&mut self, s: &mut Session, i: Input) -> Result<Step> {
        let text = match &i {
            Input::Keyword(k) | Input::Text(k) => Some(k.trim().to_string()),
            _ => None,
        };
        match self.stage {
            Stage::Select => match self.sel.feed(s, &i)? {
                SelOutcome::More => Ok(Step::Continue),
                SelOutcome::Empty => Ok(Step::Done),
                SelOutcome::Done(hs) => self.selected(s, hs),
            },
            Stage::Option => match (&i, text.as_deref().map(str::to_ascii_lowercase).as_deref()) {
                (Input::Enter, _) => Ok(Step::Done),
                (_, Some("?")) => {
                    self.list(s);
                    Ok(Step::Continue)
                }
                (_, Some("on")) => {
                    self.stage = Stage::Names(true);
                    Ok(Step::Continue)
                }
                (_, Some("off" | "of")) => {
                    self.stage = Stage::Names(false);
                    Ok(Step::Continue)
                }
                _ => {
                    s.echo("Invalid option keyword.");
                    Ok(Step::Continue)
                }
            },
            Stage::Names(on) => {
                let Some(t) = text.filter(|t| !t.is_empty()) else {
                    self.stage = Stage::Option;
                    return Ok(Step::Continue);
                };
                let pats: Vec<String> = t.split(',').map(|n| n.trim().to_string()).filter(|n| !n.is_empty()).collect();
                if let Some(h) = self.target {
                    let r = if on { set(s, h, &pats, &[]) } else { set(s, h, &[], &pats) };
                    if let Err(e) = r {
                        s.echo(e.to_string());
                    }
                }
                self.stage = Stage::Option;
                Ok(Step::Continue)
            }
        }
    }
}
