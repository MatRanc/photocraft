//! Slice tool and Slice Select tool (Crop group, C): drawing new user slices, selecting and
//! moving slices, the slice overlay (outlines and numbered badges in the Preferences › Guides,
//! Grid & Slices colour) and both tools' options bars. Every change is an engine command
//! (`slice.new`, `slice.set`, `slice.promote`, `slice.divide`, `slice.delete`); this module only
//! turns pointer gestures into them.

use egui::{Color32, Rect, Stroke, vec2};
use photocraft_doc::slices::{self, ResolvedSlice};
use photocraft_doc::{Document, SliceOrigin};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::PhotocraftApp;
use crate::canvas::{ToolEvent, ViewXform};
use crate::state::{DialogKind, Tool};

/// Slice tool state (serde: readable through the control channel).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SliceUi {
    /// The selected slice's display number (Slice Select tool).
    pub selected: Option<usize>,
    /// Slice Select options bar › Hide Auto Slices.
    pub hide_auto: bool,
    #[serde(skip)]
    pub drag: Option<SliceDrag>,
}

/// A gesture in progress.
#[derive(Clone, Debug, PartialEq)]
pub enum SliceDrag {
    New {
        start: [f64; 2],
        cur: [f64; 2],
    },
    Move {
        id: u32,
        start: [f64; 2],
        cur: [f64; 2],
        rect: photocraft_geom::Rect,
    },
    /// A handle of the selected slice: `handle` is −1, 0 or 1 per axis (the left, untouched or
    /// right edge; the top, untouched or bottom edge).
    Resize {
        id: u32,
        handle: [i8; 2],
        start: [f64; 2],
        cur: [f64; 2],
        rect: photocraft_geom::Rect,
    },
}

/// How far from a handle (screen points) a press still grabs it.
const HANDLE_REACH: f64 = 5.0;

/// The handle of `r` at document point `p`, within `reach` document pixels: the corners first,
/// then the edge middles, as the overlay draws them.
fn handle_at(r: photocraft_geom::Rect, p: [f64; 2], reach: f64) -> Option<[i8; 2]> {
    let (x0, y0, x1, y1) = (f64::from(r.x0), f64::from(r.y0), f64::from(r.x1), f64::from(r.y1));
    let at = |h: [i8; 2]| {
        let x = match h[0] {
            -1 => x0,
            1 => x1,
            _ => (x0 + x1) / 2.0,
        };
        let y = match h[1] {
            -1 => y0,
            1 => y1,
            _ => (y0 + y1) / 2.0,
        };
        (p[0] - x).abs() <= reach && (p[1] - y).abs() <= reach
    };
    [[-1, -1], [1, -1], [-1, 1], [1, 1], [0, -1], [0, 1], [-1, 0], [1, 0]].into_iter().find(|h| at(*h))
}

/// `r` with the edges `handle` grabs moved by (`dx`, `dy`), turned the right way round when an
/// edge is dragged past the opposite one, and at least 1 × 1.
fn resized(r: photocraft_geom::Rect, handle: [i8; 2], dx: i32, dy: i32) -> photocraft_geom::Rect {
    let (mut x0, mut y0, mut x1, mut y1) = (r.x0, r.y0, r.x1, r.y1);
    match handle[0] {
        -1 => x0 = x0.saturating_add(dx),
        1 => x1 = x1.saturating_add(dx),
        _ => {}
    }
    match handle[1] {
        -1 => y0 = y0.saturating_add(dy),
        1 => y1 = y1.saturating_add(dy),
        _ => {}
    }
    let (x0, x1) = (x0.min(x1), x0.max(x1).max(x0.min(x1).saturating_add(1)));
    let (y0, y1) = (y0.min(y1), y0.max(y1).max(y0.min(y1).saturating_add(1)));
    photocraft_geom::Rect::new(x0, y0, x1, y1)
}

fn is_slice_tool(t: Tool) -> bool {
    matches!(t, Tool::Slice | Tool::SliceSelect)
}

/// The slice under a document point: stored slices win (the latest on top), then auto slices.
pub fn slice_at(doc: &Document, x: f64, y: f64) -> Option<ResolvedSlice> {
    let all = slices::resolve(doc);
    let (xi, yi) = (x.floor() as i32, y.floor() as i32);
    let order = |r: &ResolvedSlice| r.id.and_then(|id| doc.slices.list.iter().position(|s| s.id == id));
    all.iter().filter(|r| r.rect.contains(xi, yi)).max_by_key(|r| order(r).map_or(0, |i| i + 1)).cloned()
}

fn rect_from(a: [f64; 2], b: [f64; 2]) -> photocraft_geom::Rect {
    let (x0, x1) = (a[0].min(b[0]).floor() as i32, a[0].max(b[0]).ceil() as i32);
    let (y0, y1) = (a[1].min(b[1]).floor() as i32, a[1].max(b[1]).ceil() as i32);
    photocraft_geom::Rect::new(x0, y0, x1, y1)
}

/// Where a new slice drawn from `start` to `cur` ends under the Slice tool's Style (Normal, Fixed
/// Ratio, Fixed Size), as the marquee's (#8360).
fn new_end(app: &PhotocraftApp, start: [f64; 2], cur: [f64; 2]) -> [f64; 2] {
    let o = &app.ui.tool_options;
    crate::chrome_ui::marquee_end(&o.slice_style, f64::from(o.slice_width), f64::from(o.slice_height), false, start, cur)
}

/// Pointer input for the two slice tools. Returns true when the event was theirs.
pub fn pointer(app: &mut PhotocraftApp, ev: ToolEvent, _mods: egui::Modifiers) -> bool {
    let tool = app.ui.tool;
    if !is_slice_tool(tool) {
        return false;
    }
    let Some(doc) = app.session.active().map(|d| d.doc.clone()) else { return true };
    let locked = app.session.file_menu.slices_locked;
    match ev {
        // View › Lock Slices: neither tool draws, selects or moves a slice, and the press says
        // why, as Photoshop's error does (#7865).
        ToolEvent::Down { .. } if locked => {
            app.ui.slices.selected = None;
            app.ui.status = tl!("Slices are locked (View › Lock Slices)").to_string();
            app.ui.status_error = true;
        }
        ToolEvent::Down { x, y, .. } => {
            if tool == Tool::Slice {
                app.ui.slices.drag = Some(SliceDrag::New { start: [x, y], cur: [x, y] });
            } else {
                // A handle of the selected user slice resizes it (#7863).
                let reach = HANDLE_REACH / f64::from(app.point_zoom().max(0.01));
                let grabbed = app.ui.slices.selected.filter(|_| !locked).and_then(|n| {
                    let r = slices::resolve(&doc).into_iter().find(|r| r.number == n && r.origin == SliceOrigin::User)?;
                    Some((r.id?, r.rect, handle_at(r.rect, [x, y], reach)?))
                });
                if let Some((id, rect, handle)) = grabbed {
                    app.ui.slices.drag = Some(SliceDrag::Resize { id, handle, start: [x, y], cur: [x, y], rect });
                    return true;
                }
                let hit = slice_at(&doc, x, y);
                app.ui.slices.selected = hit.as_ref().map(|r| r.number);
                // The whole slice moves, also when the press hits one piece of a covered slice.
                if let Some(r) = hit
                    && let Some(sl) = r.id.and_then(|id| doc.slices.get(id)).filter(|_| r.origin == SliceOrigin::User)
                {
                    app.ui.slices.drag = Some(SliceDrag::Move { id: sl.id, start: [x, y], cur: [x, y], rect: sl.rect });
                }
            }
        }
        ToolEvent::Move { x, y, .. } => match &mut app.ui.slices.drag {
            Some(SliceDrag::New { cur, .. }) | Some(SliceDrag::Move { cur, .. }) | Some(SliceDrag::Resize { cur, .. }) => *cur = [x, y],
            None => {}
        },
        ToolEvent::Up { x, y } => match app.ui.slices.drag.take() {
            Some(SliceDrag::New { start, .. }) => {
                let r = rect_from(start, new_end(app, start, [x, y]));
                if r.width() >= 1 && r.height() >= 1 && (r.width() > 2 || r.height() > 2) {
                    if let Ok(v) = app.run("slice.new", json!({"rect": [r.x0, r.y0, r.width(), r.height()]})) {
                        app.ui.slices.selected = v.get("number").and_then(Value::as_u64).map(|n| n as usize);
                    }
                } else {
                    app.ui.slices.selected = slice_at(&doc, x, y).map(|r| r.number);
                }
            }
            Some(SliceDrag::Move { id, start, rect, .. }) => {
                let (dx, dy) = ((x - start[0]).round() as i32, (y - start[1]).round() as i32);
                if dx != 0 || dy != 0 {
                    let r = rect.translate(dx, dy);
                    if app.run("slice.set", json!({"slice": id, "rect": [r.x0, r.y0, r.width(), r.height()]})).is_ok() {
                        let d = app.session.active().map(|d| d.doc.clone());
                        app.ui.slices.selected = d.and_then(|d| slices::resolve(&d).into_iter().find(|s| s.id == Some(id)).map(|s| s.number));
                    }
                }
            }
            Some(SliceDrag::Resize { id, handle, start, rect, .. }) => {
                let r = resized(rect, handle, (x - start[0]).round() as i32, (y - start[1]).round() as i32);
                if r != rect && app.run("slice.set", json!({"slice": id, "rect": [r.x0, r.y0, r.width(), r.height()]})).is_ok() {
                    let d = app.session.active().map(|d| d.doc.clone());
                    app.ui.slices.selected = d.and_then(|d| slices::resolve(&d).into_iter().find(|s| s.id == Some(id)).map(|s| s.number));
                }
            }
            None => {}
        },
    }
    true
}

fn hex(s: &str, fallback: Color32) -> Color32 {
    let s = s.trim_start_matches('#');
    let b = |i: usize| s.get(i..i + 2).and_then(|h| u8::from_str_radix(h, 16).ok());
    match (s.len(), b(0), b(2), b(4)) {
        (6, Some(r), Some(g), Some(bl)) => Color32::from_rgb(r, g, bl),
        _ => fallback,
    }
}

/// Slice outlines and numbered badges (View › Show › Slices; always while a slice tool is on).
pub fn draw_overlay(app: &PhotocraftApp, painter: &egui::Painter, xf: &ViewXform) {
    let tool_on = is_slice_tool(app.ui.tool);
    let Some(doc) = app.session.active().map(|d| d.doc.clone()) else { return };
    if !(tool_on || (app.ui.view.shows(app.ui.view.show.slices) && !doc.slices.is_empty())) {
        return;
    }
    let prefs = &app.session.prefs().guides_grid_and_slices;
    let color = hex(&prefs.slice_color, Color32::from_rgb(0x38, 0xb5, 0xff));
    let numbers = prefs.show_slice_numbers;
    let selected_color = Color32::from_rgb(0xff, 0xb0, 0x00);
    let to_screen = |r: photocraft_geom::Rect| Rect::from_two_pos(xf.to_screen(r.x0 as f32, r.y0 as f32), xf.to_screen(r.x1 as f32, r.y1 as f32));
    let all = slices::resolve(&doc);
    // Auto slices first (dotted, underneath), then user and layer slices.
    for pass in [true, false] {
        for r in all.iter().filter(|r| (r.origin == SliceOrigin::Auto) == pass) {
            if pass && (!tool_on || app.ui.slices.hide_auto) {
                continue;
            }
            let sr = to_screen(r.rect);
            let selected = app.ui.slices.selected == Some(r.number);
            if pass {
                let pts = [sr.left_top(), sr.right_top(), sr.right_bottom(), sr.left_bottom(), sr.left_top()];
                painter.add(egui::Shape::dashed_line(&pts, Stroke::new(1.0, color.gamma_multiply(0.7)), 3.0, 3.0));
            } else {
                painter.rect_stroke(sr, 0.0, Stroke::new(1.0, color), egui::StrokeKind::Inside);
            }
            if selected && tool_on {
                painter.rect_stroke(sr, 0.0, Stroke::new(2.0, selected_color), egui::StrokeKind::Inside);
                if r.origin == SliceOrigin::User {
                    for c in [
                        sr.left_top(),
                        sr.right_top(),
                        sr.left_bottom(),
                        sr.right_bottom(),
                        sr.center_top(),
                        sr.center_bottom(),
                        sr.left_center(),
                        sr.right_center(),
                    ] {
                        painter.rect_filled(Rect::from_center_size(c, vec2(5.0, 5.0)), 0.0, selected_color);
                    }
                }
            }
            if numbers && sr.width() > 18.0 && sr.height() > 12.0 {
                let label = match r.origin {
                    SliceOrigin::Layer => format!("{:02} ▪", r.number),
                    _ => format!("{:02}", r.number),
                };
                let font = egui::FontId::proportional(10.0);
                let galley = painter.layout_no_wrap(label, font, Color32::WHITE);
                let badge = Rect::from_min_size(sr.left_top() + vec2(2.0, 2.0), galley.size() + vec2(6.0, 2.0));
                let bg = if pass {
                    Color32::from_gray(110)
                } else if selected && tool_on {
                    selected_color
                } else {
                    color
                };
                painter.rect_filled(badge, 2.0, bg);
                painter.galley(badge.min + vec2(3.0, 1.0), galley, Color32::WHITE);
            }
        }
    }
    match &app.ui.slices.drag {
        Some(SliceDrag::New { start, cur }) => {
            let sr = to_screen(rect_from(*start, new_end(app, *start, *cur)));
            painter.rect_stroke(sr, 0.0, Stroke::new(1.0, Color32::WHITE), egui::StrokeKind::Middle);
            let pts = [sr.left_top(), sr.right_top(), sr.right_bottom(), sr.left_bottom(), sr.left_top()];
            painter.add(egui::Shape::dashed_line(&pts, Stroke::new(1.0, color), 4.0, 4.0));
        }
        Some(SliceDrag::Move { start, cur, rect, .. }) => {
            let r = rect.translate((cur[0] - start[0]).round() as i32, (cur[1] - start[1]).round() as i32);
            painter.rect_stroke(to_screen(r), 0.0, Stroke::new(2.0, selected_color), egui::StrokeKind::Inside);
        }
        Some(SliceDrag::Resize { handle, start, cur, rect, .. }) => {
            let r = resized(*rect, *handle, (cur[0] - start[0]).round() as i32, (cur[1] - start[1]).round() as i32);
            painter.rect_stroke(to_screen(r), 0.0, Stroke::new(2.0, selected_color), egui::StrokeKind::Inside);
        }
        None => {}
    }
}

/// The selected slice's id, promoting an auto slice first when `promote`.
fn selected_id(app: &mut PhotocraftApp, promote: bool) -> Option<u32> {
    let n = app.ui.slices.selected?;
    let doc = app.session.active()?.doc.clone();
    let r = slices::resolve(&doc).into_iter().find(|r| r.number == n)?;
    match r.id {
        Some(id) => Some(id),
        None if promote => app.run("slice.promote", json!({"number": n})).ok()?.get("slice")?.as_u64().map(|v| v as u32),
        None => None,
    }
}

/// Slice Options dialog for the selected slice (auto slices are promoted on OK).
pub fn open_options(app: &mut PhotocraftApp) -> Result<Value, String> {
    let n = app.ui.slices.selected.ok_or("select a slice with the Slice Select tool")?;
    let st = app.session.active().ok_or("no document")?;
    let r = slices::resolve(&st.doc).into_iter().find(|r| r.number == n).ok_or("no such slice")?;
    let v = photocraft_engine::slice_cmds::resolved_json(&st.doc, &r);
    let mut f = serde_json::Map::new();
    f.insert("__command".into(), json!("slice.set"));
    f.insert("__label".into(), json!("Slice Options"));
    f.insert("__form".into(), json!(true));
    f.insert("__choices".into(), json!({"kind": ["image", "noImage"]}));
    f.insert("number".into(), json!(n));
    f.insert("kind".into(), v["kind"].clone());
    for k in ["name", "url", "target", "alt", "message", "cellText"] {
        f.insert(k.into(), v[k].clone());
    }
    f.insert("background".into(), json!(v["background"].as_str().unwrap_or("none")));
    Ok(json!({"dialog": app.ui.open_dialog(DialogKind::Command, f)}))
}

/// Divide Slice dialog.
fn open_divide(app: &mut PhotocraftApp) -> Result<Value, String> {
    let n = app.ui.slices.selected.ok_or("select a slice first")?;
    let mut f = serde_json::Map::new();
    f.insert("__command".into(), json!("slice.divide"));
    f.insert("__label".into(), json!("Divide Slice"));
    f.insert("__form".into(), json!(true));
    f.insert("__choices".into(), json!({}));
    f.insert("number".into(), json!(n));
    f.insert("horizontal".into(), json!(2));
    f.insert("vertical".into(), json!(1));
    Ok(json!({"dialog": app.ui.open_dialog(DialogKind::Command, f)}))
}

/// Options bars of the Slice and Slice Select tools.
pub fn options_bar(app: &mut PhotocraftApp, ui: &mut egui::Ui, tool: Tool) -> bool {
    if !is_slice_tool(tool) {
        return false;
    }
    let has_doc = app.session.active().is_some();
    let locked = app.session.file_menu.slices_locked;
    let t = crate::theme::Tokens::get(ui.ctx());
    if tool == Tool::Slice {
        // Style, Width and Height, as the marquee's (#8360).
        let o = &mut app.ui.tool_options;
        crate::panels::opt_label(ui, tl!("Style"));
        let styles = [("normal".to_string(), tl!("Normal")), ("fixedRatio".to_string(), tl!("Fixed Ratio")), ("fixedSize".to_string(), tl!("Fixed Size"))];
        if crate::widgets::dropdown(ui, "slice-style", &mut o.slice_style, &styles, 96.0) {
            (o.slice_width, o.slice_height) = if o.slice_style == "fixedSize" { (64.0, 64.0) } else { (1.0, 1.0) };
        }
        let fixed_size = o.slice_style == "fixedSize";
        let (range, unit) = (crate::tool_memory::style_range(&o.slice_style), if fixed_size { "px" } else { "" });
        ui.add_enabled_ui(o.slice_style != "normal", |ui| {
            crate::panels::opt_label(ui, tl!("Width"));
            crate::widgets::value_field(ui, &mut o.slice_width, range.clone(), unit, 62.0);
            if crate::icons::button(ui, "arrow-left-right", 22.0, false, tl!("Swaps height and width")).clicked() {
                std::mem::swap(&mut o.slice_width, &mut o.slice_height);
            }
            crate::panels::opt_label(ui, tl!("Height"));
            crate::widgets::value_field(ui, &mut o.slice_height, range, unit, 62.0);
        });
        crate::widgets::vline(ui, 22.0);
        if ui.add_enabled_ui(has_doc && !locked, |ui| crate::widgets::secondary_button(ui, tl!("Slices From Guides"), 0.0)).inner.clicked() {
            let _ = app.run("slice.fromGuides", json!({}));
        }
    } else {
        let sel = app.ui.slices.selected.is_some();
        let stored =
            sel && app.session.active().is_some_and(|d| slices::resolve(&d.doc).iter().any(|r| Some(r.number) == app.ui.slices.selected && r.id.is_some()));
        if ui.add_enabled_ui(sel && !locked, |ui| crate::widgets::secondary_button(ui, tl!("Promote"), 0.0)).inner.clicked() {
            let _ = selected_id(app, true).map(|id| app.run("slice.promote", json!({"slice": id})));
        }
        if ui.add_enabled_ui(sel && !locked, |ui| crate::widgets::secondary_button(ui, tl!("Divide…"), 0.0)).inner.clicked() {
            let _ = open_divide(app);
        }
        if ui.add_enabled_ui(stored && !locked, |ui| crate::widgets::secondary_button(ui, tl!("Delete"), 0.0)).inner.clicked()
            && let Some(id) = selected_id(app, false)
            && app.run("slice.delete", json!({"slice": id})).is_ok()
        {
            app.ui.slices.selected = None;
        }
        crate::widgets::vline(ui, 22.0);
        crate::widgets::checkbox(ui, &mut app.ui.slices.hide_auto, tl!("Hide Auto Slices"));
        crate::widgets::vline(ui, 22.0);
        if ui.add_enabled_ui(sel && !locked, |ui| crate::widgets::secondary_button(ui, tl!("Slice Options…"), 0.0)).inner.clicked() {
            let _ = open_options(app);
        }
    }
    if locked {
        ui.label(egui::RichText::new(tl!("Slices are locked (View › Lock Slices)")).color(t.text_dim).size(11.0));
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> PhotocraftApp {
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), Default::default());
        app.run("file.new", json!({"width": 100, "height": 80})).unwrap();
        app
    }

    #[test]
    fn slice_tool_draws_and_select_tool_moves() {
        let mut app = app();
        app.ui.tool = Tool::Slice;
        let m = egui::Modifiers::NONE;
        assert!(pointer(&mut app, ToolEvent::Down { x: 10.0, y: 10.0, pressure: 1.0 }, m));
        pointer(&mut app, ToolEvent::Move { x: 40.0, y: 30.0, pressure: 1.0 }, m);
        pointer(&mut app, ToolEvent::Up { x: 40.0, y: 30.0 }, m);
        let doc = app.session.active().unwrap().doc.clone();
        assert_eq!(doc.slices.list.len(), 1);
        assert_eq!(doc.slices.list[0].rect, photocraft_geom::Rect::new(10, 10, 40, 30));
        let n = app.ui.slices.selected.unwrap();
        // Slice Select: drag it by (5, 5).
        app.ui.tool = Tool::SliceSelect;
        pointer(&mut app, ToolEvent::Down { x: 20.0, y: 20.0, pressure: 1.0 }, m);
        assert_eq!(app.ui.slices.selected, Some(n));
        pointer(&mut app, ToolEvent::Up { x: 25.0, y: 25.0 }, m);
        let doc = app.session.active().unwrap().doc.clone();
        assert_eq!(doc.slices.list[0].rect, photocraft_geom::Rect::new(15, 15, 45, 35));
        // Clicking an auto slice selects it; Slice Options promotes it.
        pointer(&mut app, ToolEvent::Down { x: 90.0, y: 75.0, pressure: 1.0 }, m);
        pointer(&mut app, ToolEvent::Up { x: 90.0, y: 75.0 }, m);
        let r = open_options(&mut app).unwrap();
        let id = r["dialog"].as_u64().unwrap();
        app.ui.dialog_mut(id).unwrap().fields.insert("name".into(), json!("footer"));
        crate::dialogs::confirm(&mut app, id).unwrap();
        let doc = app.session.active().unwrap().doc.clone();
        assert!(doc.slices.list.iter().any(|s| s.name == "footer"));
    }

    /// #7865: with View › Lock Slices on, Slice Select doesn't select (or move) a slice, and the
    /// Slice tool doesn't start one; both say why, as Photoshop's error does.
    #[test]
    fn locked_slices_cannot_be_selected() {
        let mut app = app();
        let m = egui::Modifiers::NONE;
        app.ui.tool = Tool::Slice;
        pointer(&mut app, ToolEvent::Down { x: 10.0, y: 10.0, pressure: 1.0 }, m);
        pointer(&mut app, ToolEvent::Up { x: 40.0, y: 30.0 }, m);
        let rect = app.session.active().unwrap().doc.slices.list[0].rect;
        app.ui.slices.selected = None;
        app.run("view.lockSlices", json!({"on": true})).unwrap();
        app.ui.tool = Tool::SliceSelect;
        pointer(&mut app, ToolEvent::Down { x: 20.0, y: 20.0, pressure: 1.0 }, m);
        pointer(&mut app, ToolEvent::Move { x: 30.0, y: 30.0, pressure: 1.0 }, m);
        pointer(&mut app, ToolEvent::Up { x: 30.0, y: 30.0 }, m);
        assert_eq!(app.ui.slices.selected, None, "not selected");
        assert_eq!(app.session.active().unwrap().doc.slices.list[0].rect, rect, "not moved");
        assert!(app.ui.status_error && app.ui.status.contains("locked"), "{}", app.ui.status);
        // The Slice tool says so too.
        app.ui.status_error = false;
        app.ui.tool = Tool::Slice;
        pointer(&mut app, ToolEvent::Down { x: 50.0, y: 50.0, pressure: 1.0 }, m);
        pointer(&mut app, ToolEvent::Up { x: 80.0, y: 70.0 }, m);
        assert_eq!(app.session.active().unwrap().doc.slices.list.len(), 1);
        assert!(app.ui.status_error && app.ui.status.contains("locked"), "{}", app.ui.status);
        // Unlocked again, Slice Select selects it.
        app.run("view.lockSlices", json!({"on": false})).unwrap();
        app.ui.tool = Tool::SliceSelect;
        pointer(&mut app, ToolEvent::Down { x: 20.0, y: 20.0, pressure: 1.0 }, m);
        pointer(&mut app, ToolEvent::Up { x: 20.0, y: 20.0 }, m);
        assert!(app.ui.slices.selected.is_some());
    }

    /// #8360: the Slice tool's Style, as the marquee's. Fixed Ratio keeps the drag's larger extent
    /// and gives the other the ratio; Fixed Size draws a slice of that size where the drag starts,
    /// towards the drag, also on a plain click.
    #[test]
    fn slice_style_fixes_the_ratio_or_the_size() {
        let mut app = app();
        app.ui.tool = Tool::Slice;
        let m = egui::Modifiers::NONE;
        let draw = |app: &mut PhotocraftApp, a: [f64; 2], b: [f64; 2]| {
            pointer(app, ToolEvent::Down { x: a[0], y: a[1], pressure: 1.0 }, m);
            pointer(app, ToolEvent::Move { x: b[0], y: b[1], pressure: 1.0 }, m);
            pointer(app, ToolEvent::Up { x: b[0], y: b[1] }, m);
            app.session.active().unwrap().doc.slices.list.last().unwrap().rect
        };
        let o = &mut app.ui.tool_options;
        (o.slice_style, o.slice_width, o.slice_height) = ("fixedRatio".into(), 2.0, 1.0);
        assert_eq!(draw(&mut app, [10.0, 10.0], [30.0, 40.0]), photocraft_geom::Rect::new(10, 10, 70, 40));
        let o = &mut app.ui.tool_options;
        (o.slice_style, o.slice_width, o.slice_height) = ("fixedSize".into(), 24.0, 16.0);
        assert_eq!(draw(&mut app, [5.0, 5.0], [5.0, 5.0]), photocraft_geom::Rect::new(5, 5, 29, 21), "a click");
        assert_eq!(draw(&mut app, [90.0, 70.0], [80.0, 60.0]), photocraft_geom::Rect::new(66, 54, 90, 70), "up and to the left");
        // The preview while dragging shows the same rectangle the slice gets.
        pointer(&mut app, ToolEvent::Down { x: 40.0, y: 40.0, pressure: 1.0 }, m);
        pointer(&mut app, ToolEvent::Move { x: 41.0, y: 41.0, pressure: 1.0 }, m);
        let Some(SliceDrag::New { start, cur }) = app.ui.slices.drag.clone() else { panic!("drawing a slice") };
        assert_eq!(rect_from(start, new_end(&app, start, cur)), photocraft_geom::Rect::new(40, 40, 64, 56));
    }

    /// #8360: the options bar's Style picks Fixed Size, which starts at 64 × 64 px, and the slice
    /// drawn next has that size.
    #[test]
    fn options_bar_picks_the_slice_style() {
        use egui::accesskit::Role;
        use egui_kittest::Harness;
        use egui_kittest::kittest::{NodeT, Queryable};

        let mut app = app();
        app.sync_views();
        app.ui.tool = Tool::Slice;
        let mut h = Harness::builder().with_size(vec2(1400.0, 200.0)).build_ui_state(
            |ui, app: &mut PhotocraftApp| {
                if ui.ctx().fonts(|f| f.families().contains(&egui::FontFamily::Name("medium".into()))) {
                    crate::panels::options_bar(app, ui);
                }
            },
            app,
        );
        PhotocraftApp::setup_context(&h.ctx, crate::theme::ThemeKind::Pro);
        h.run_steps(4);
        let fields = |h: &Harness<'static, PhotocraftApp>| {
            h.query_all_by_role(Role::SpinButton)
                .filter(|n| !n.accesskit_node().is_disabled())
                .filter_map(|n| n.accesskit_node().numeric_value())
                .collect::<Vec<_>>()
        };
        assert!(fields(&h).is_empty(), "Normal has no Width or Height to set");
        h.get_all_by_role(Role::ComboBox).find(|n| n.accesskit_node().value().as_deref() == Some("Normal")).unwrap().click();
        h.run_steps(3);
        h.get_by_label("Fixed Size").click();
        h.run_steps(4);
        let o = &h.state().ui.tool_options;
        assert_eq!((o.slice_style.as_str(), o.slice_width, o.slice_height), ("fixedSize", 64.0, 64.0));
        assert_eq!(fields(&h), [64.0, 64.0]);
        let (app, m) = (h.state_mut(), egui::Modifiers::NONE);
        pointer(app, ToolEvent::Down { x: 10.0, y: 10.0, pressure: 1.0 }, m);
        pointer(app, ToolEvent::Up { x: 10.0, y: 10.0 }, m);
        assert_eq!(app.session.active().unwrap().doc.slices.list[0].rect, photocraft_geom::Rect::new(10, 10, 74, 74));
    }

    #[test]
    fn dragging_a_piece_moves_the_whole_covered_slice() {
        // #7770: the slice under a later one is drawn as pieces; dragging one moves all of it.
        let mut app = app();
        app.run("slice.new", json!({"rect": [0, 0, 60, 60]})).unwrap();
        app.run("slice.new", json!({"rect": [40, 40, 20, 20]})).unwrap();
        app.ui.tool = Tool::SliceSelect;
        let m = egui::Modifiers::NONE;
        pointer(&mut app, ToolEvent::Down { x: 10.0, y: 50.0, pressure: 1.0 }, m);
        pointer(&mut app, ToolEvent::Up { x: 15.0, y: 50.0 }, m);
        let doc = app.session.active().unwrap().doc.clone();
        assert_eq!(doc.slices.list[0].rect, photocraft_geom::Rect::new(5, 0, 65, 60));
    }

    #[test]
    fn handles_resize_the_edges_they_hang_off() {
        let r = photocraft_geom::Rect::new(10, 10, 40, 30);
        assert_eq!(handle_at(r, [10.5, 9.0], 2.0), Some([-1, -1]));
        assert_eq!(handle_at(r, [25.0, 30.0], 2.0), Some([0, 1]));
        assert_eq!(handle_at(r, [40.0, 20.0], 2.0), Some([1, 0]));
        assert_eq!(handle_at(r, [25.0, 20.0], 2.0), None, "the middle is no handle");
        assert_eq!(resized(r, [1, 0], 5, 9), photocraft_geom::Rect::new(10, 10, 45, 30), "the right edge only");
        assert_eq!(resized(r, [-1, 1], -4, 6), photocraft_geom::Rect::new(6, 10, 40, 36));
        assert_eq!(resized(r, [-1, 0], 50, 0), photocraft_geom::Rect::new(40, 10, 60, 30), "dragged past the other edge");
        assert_eq!(resized(r, [1, 0], -30, 0), photocraft_geom::Rect::new(10, 10, 11, 30), "never empty");
    }

    /// #7863: with Slice Select, dragging a handle of the selected slice resizes it; the middle
    /// still moves it, and locked slices keep their size.
    #[test]
    fn slice_select_handles_resize_the_slice() {
        let mut app = app();
        let m = egui::Modifiers::NONE;
        app.ui.tool = Tool::Slice;
        pointer(&mut app, ToolEvent::Down { x: 10.0, y: 10.0, pressure: 1.0 }, m);
        pointer(&mut app, ToolEvent::Up { x: 40.0, y: 30.0 }, m);
        app.ui.tool = Tool::SliceSelect;
        let rect = |app: &PhotocraftApp| app.session.active().unwrap().doc.slices.list[0].rect;
        // The bottom-right corner.
        pointer(&mut app, ToolEvent::Down { x: 40.0, y: 30.0, pressure: 1.0 }, m);
        assert!(matches!(app.ui.slices.drag, Some(SliceDrag::Resize { handle: [1, 1], .. })));
        pointer(&mut app, ToolEvent::Move { x: 50.0, y: 45.0, pressure: 1.0 }, m);
        pointer(&mut app, ToolEvent::Up { x: 50.0, y: 45.0 }, m);
        assert_eq!(rect(&app), photocraft_geom::Rect::new(10, 10, 50, 45));
        assert!(app.ui.slices.selected.is_some(), "still selected");
        // The left edge's middle.
        pointer(&mut app, ToolEvent::Down { x: 10.0, y: 27.0, pressure: 1.0 }, m);
        pointer(&mut app, ToolEvent::Up { x: 4.0, y: 60.0 }, m);
        assert_eq!(rect(&app), photocraft_geom::Rect::new(4, 10, 50, 45), "only the left edge moves");
        // Inside, away from the handles, it still moves.
        pointer(&mut app, ToolEvent::Down { x: 30.0, y: 20.0, pressure: 1.0 }, m);
        pointer(&mut app, ToolEvent::Up { x: 32.0, y: 21.0 }, m);
        assert_eq!(rect(&app), photocraft_geom::Rect::new(6, 11, 52, 46));
        // Locked: the handle doesn't resize.
        app.run("view.lockSlices", json!({"on": true})).unwrap();
        pointer(&mut app, ToolEvent::Down { x: 52.0, y: 46.0, pressure: 1.0 }, m);
        pointer(&mut app, ToolEvent::Up { x: 70.0, y: 70.0 }, m);
        assert_eq!(rect(&app), photocraft_geom::Rect::new(6, 11, 52, 46));
    }

    #[test]
    fn locked_slices_ignore_gestures() {
        let mut app = app();
        app.run("view.lockSlices", json!({"on": true})).unwrap();
        app.ui.tool = Tool::Slice;
        let m = egui::Modifiers::NONE;
        pointer(&mut app, ToolEvent::Down { x: 10.0, y: 10.0, pressure: 1.0 }, m);
        pointer(&mut app, ToolEvent::Up { x: 40.0, y: 30.0 }, m);
        assert!(app.session.active().unwrap().doc.slices.is_empty());
    }
}
