use super::{SlideApp, Tokens, Ui, Value, json, run};
use crate::drafts::Tree;

/// SmartArt Text Pane: one line per node, indented by level. Enter adds a line after, Tab and
/// Shift+Tab demote and promote, Backspace in an empty line removes it.
pub(super) fn smartart(app: &mut SlideApp, ui: &mut Ui) {
    let Some((gid, d)) = app.session.active().and_then(deckcraft_engine::cmd::smartart::selected) else {
        ui.label("Select a SmartArt graphic to edit its text.");
        return;
    };
    let texts: Vec<String> = d.nodes.iter().map(|n| n.text.clone()).collect();
    // The lines typed and not entered yet are the app's, not this pane's: they are still there when
    // the pane is drawn again (a view, slide or selection that hid it came and went), and they count
    // as unsaved work meanwhile. They belong to the graphic (on its slide, master or layout), and
    // only while its text is still what they started from; otherwise the pane shows its text.
    let at = app.session.active().and_then(|st| Tree::of(st).map(|tree| (st.uid, tree)));
    let mut drafts = at.map_or_else(|| texts.clone(), |(doc, tree)| app.drafts.smartart_lines(doc, tree, gid, &texts));
    if drafts.len() != texts.len() {
        drafts.clone_from(&texts);
    }
    let mut action: Option<(&str, Value)> = None;
    let mut commit: Option<usize> = None;
    let mut left_behind: Vec<usize> = Vec::new();
    let mut focus_row: Option<usize> = None;
    ui.label(egui::RichText::new("Type your text here").color(Tokens::get(ui.ctx()).text_dim));
    ui.add_space(4.0);
    for (i, node) in d.nodes.iter().enumerate() {
        let Some(draft) = drafts.get_mut(i) else { break };
        let id = egui::Id::unique(("smartart_line", gid.0, i));
        ui.horizontal(|ui| {
            ui.add_space(16.0 * node.level as f32);
            ui.label("•");
            let te = egui::TextEdit::singleline(draft).id(id).hint_text("SmartArt text").desired_width(f32::INFINITY);
            let r = ui.add(te);
            // (The widget's own `has_focus` is also false while the window is unfocused: this asks egui's memory.)
            let kept = ui.ctx().memory(|m| m.has_focus(id)) || r.lost_focus();
            if !kept && texts.get(i).is_some_and(|t| t != &*draft) {
                // Typed before this pane was hidden, and nothing has the line now.
                left_behind.push(i);
            }
            if app.ui.smartart_focus == Some(i) {
                r.request_focus();
                app.ui.smartart_focus = None;
            }
            if r.gained_focus() || r.has_focus() {
                app.ui.smartart_row = i;
            }
            if r.has_focus() {
                let back = ui.input(|inp| inp.key_pressed(egui::Key::Backspace));
                if back && draft.is_empty() && texts.get(i).is_some_and(String::is_empty) && d.nodes.len() > 1 {
                    action = Some(("smartArt.removeNode", json!({"index": i})));
                    focus_row = Some(i.saturating_sub(1));
                }
            }
            if r.lost_focus() {
                let (tab, shift) = ui.input(|inp| (inp.key_pressed(egui::Key::Tab), inp.modifiers.shift));
                if tab {
                    action = Some((if shift { "smartArt.promote" } else { "smartArt.demote" }, json!({"index": i})));
                    focus_row = Some(i);
                } else if ui.input(|inp| inp.key_pressed(egui::Key::Enter)) {
                    action = Some(("smartArt.addNode", json!({"index": i, "position": "after"})));
                    focus_row = Some(i + 1);
                }
                commit = Some(i);
            }
        });
    }
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        if ui.button("+ Add").clicked() {
            action = Some(("smartArt.addNode", json!({"index": app.ui.smartart_row.min(d.nodes.len().saturating_sub(1)), "position": "after"})));
        }
        if ui.button("Remove").clicked() && !d.nodes.is_empty() {
            action = Some(("smartArt.removeNode", json!({"index": app.ui.smartart_row.min(d.nodes.len() - 1)})));
        }
    });
    // Commit a changed line before any structural edit, so the edit sees the new text.
    if let Some(i) = commit
        && drafts.get(i) != texts.get(i)
    {
        run(app, "smartArt.setNodes", json!({"id": gid.0, "index": i, "text": drafts.get(i).cloned().unwrap_or_default()}));
    }
    if action.is_some() {
        for (i, t) in drafts.iter().enumerate() {
            if Some(i) != commit && texts.get(i) != Some(t) {
                run(app, "smartArt.setNodes", json!({"id": gid.0, "index": i, "text": t}));
            }
        }
    } else {
        // A line typed before the pane was hidden goes in as the focus leaving it would have put it.
        for &i in &left_behind {
            if let Some(t) = drafts.get(i) {
                run(app, "smartArt.setNodes", json!({"id": gid.0, "index": i, "text": t}));
            }
        }
    }
    // Lines typed and not entered yet stay with the app, to be counted whether or not this pane is
    // drawn again; what was just entered made the presentation unsaved instead.
    if let Some((doc, tree)) = at {
        if commit.is_none() && action.is_none() && left_behind.is_empty() {
            app.drafts.set_smartart(doc, tree, gid, &texts, drafts);
        } else {
            app.drafts.drop_smartart(doc, tree, gid);
        }
    }
    if let Some((cmd, mut p)) = action {
        if let Some(o) = p.as_object_mut() {
            o.insert("id".into(), json!(gid.0));
        }
        if let Ok(r) = app.run(cmd, p) {
            let row = r.get("index").and_then(Value::as_u64).map(|v| v as usize).or(focus_row).unwrap_or(0);
            app.ui.smartart_row = row;
            app.ui.smartart_focus = Some(row);
        }
    }
}
