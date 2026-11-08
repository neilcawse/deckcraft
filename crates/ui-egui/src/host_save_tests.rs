//! Web inside a page that keeps the files (`Services::host_save`, `?host=parent`): each file is
//! handed over with the presentation it was made from and whether it asked for a new file, a
//! Save Changes prompt's Save doesn't lose a presentation the page hasn't stored yet, a repaired
//! copy isn't offered as the file it came from, and typing not yet in a presentation counts as
//! unsaved work, whether or not the pane it was typed in is drawn.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::cell::RefCell;
use std::rc::Rc;

use deckcraft_engine::Session;
use egui::accesskit::Role;
use egui_kittest::kittest::{NodeT, Queryable};
use egui_kittest::{Harness, Node};
use serde_json::json;

use crate::{HostSave, OpenedFile, Services, SlideApp};

/// One file handed to the page: name, Save As, presentation, and whether it saved the presentation.
type Handed = Rc<RefCell<Vec<(String, bool, u64, bool)>>>;

/// The app in a page that keeps its files; `confirms`: the page says when it stored a save.
fn host_app(confirms: bool) -> (SlideApp, Handed) {
    let handed: Handed = Rc::default();
    let log = Rc::clone(&handed);
    let services = Services {
        // The page sets a download too; with `host_save` it is never used for saves.
        download: Some(Box::new(|name: &str, _: &[u8]| panic!("{name} went to a download, not the page"))),
        host_save: Some(Box::new(move |s: &HostSave<'_>| {
            log.borrow_mut().push((s.name.to_string(), s.save_as, s.document, s.saved.is_some()));
            Ok(())
        })),
        host_confirms_saves: confirms,
        ..Default::default()
    };
    (SlideApp::new(Session::new(), services), handed)
}

fn harness(app: SlideApp) -> Harness<'static, SlideApp> {
    harness_sized(app, egui::vec2(1400.0, 900.0))
}

fn harness_sized(app: SlideApp, size: egui::Vec2) -> Harness<'static, SlideApp> {
    let mut h = Harness::builder().with_size(size).build_ui_state(
        |ui, app: &mut SlideApp| {
            app.logic(ui.ctx());
            app.ui(ui);
        },
        app,
    );
    // The app keeps asking for frames, so step instead of run().
    h.run_steps(3);
    h
}

/// Two presentations, "First" and "Second" (active, with an unsaved change). Returns their uids.
fn two_presentations(app: &mut SlideApp) -> (u64, u64) {
    app.run("file.new", json!({})).unwrap();
    let first = app.session.active().unwrap().uid;
    app.run("file.new", json!({})).unwrap();
    let second = app.session.active().unwrap().uid;
    // New presentations start saved; this one gets an unsaved change.
    app.run("slide.new", json!({})).unwrap();
    assert!(app.session.active().unwrap().is_dirty());
    (first, second)
}

fn status(h: &Harness<'_, SlideApp>) -> String {
    h.state().status.as_ref().map(|s| s.0.clone()).unwrap_or_default()
}

/// Ctrl+W on the second presentation, then Save: the file handed over is the second's, though
/// the close leaves the first one active (it was saved over the first one's file).
#[test]
fn close_then_save_hands_over_the_closing_presentation() {
    let (mut app, handed) = host_app(false);
    let (first, second) = two_presentations(&mut app);
    let mut h = harness(app);
    h.state_mut().close_presentation();
    h.run_steps(2);
    assert_eq!(h.state().dialog.as_ref().map(|d| d.id.as_str()), Some("close"));
    h.key_press(egui::Key::Enter);
    h.run_steps(3);
    let docs: Vec<u64> = h.state().session.documents().iter().map(|d| d.uid).collect();
    assert_eq!(docs, [first], "the saved presentation is closed");
    let handed = handed.borrow();
    assert_eq!(handed.len(), 1, "{handed:?}");
    let (_, save_as, document, saved) = &handed[0];
    assert_eq!(*document, second, "the save belongs to the presentation that was closed");
    assert!(!*save_as && *saved, "a Save of the presentation");
}

/// A page that confirms saves: Save in the Save Changes prompt keeps the presentation open and
/// unsaved until the page has stored it; closing again then closes it without asking.
#[test]
fn close_then_save_waits_for_the_page_to_store_it() {
    let (mut app, handed) = host_app(true);
    let (first, second) = two_presentations(&mut app);
    let mut h = harness(app);
    h.state_mut().close_presentation();
    h.run_steps(2);
    h.key_press(egui::Key::Enter);
    h.run_steps(3);
    assert_eq!(h.state().session.documents().len(), 2, "not closed before the page stored it");
    let active = h.state().session.active().unwrap();
    assert_eq!(active.uid, second);
    assert!(active.is_dirty(), "unsaved until the page says it stored the file");
    assert!(status(&h).contains("Waiting for the save"), "{:?}", status(&h));
    assert_eq!(handed.borrow().iter().map(|h| h.2).collect::<Vec<_>>(), [second]);
    // The page stored it (the bridge marks the presentation saved as it was then).
    let d = h.state_mut().session.document_mut(second).unwrap();
    d.saved_doc = d.doc.clone();
    h.state_mut().close_presentation();
    h.run_steps(3);
    assert!(h.state().dialog.is_none());
    let docs: Vec<u64> = h.state().session.documents().iter().map(|d| d.uid).collect();
    assert_eq!(docs, [first]);
}

/// Save is not Save As; Save As, Export and Print ask for a new file. Only Save and Save As
/// save the presentation.
#[test]
fn save_as_export_and_print_ask_for_a_new_file() {
    let (mut app, handed) = host_app(true);
    app.run("file.new", json!({})).unwrap();
    let uid = app.session.active().unwrap().uid;
    assert!(app.save());
    app.run("file.save", json!({})).unwrap();
    app.save_as_dialog("pptx");
    app.run("file.saveAs", json!({})).unwrap();
    app.run("app.saveAsDialog", json!({})).unwrap();
    assert!(app.export_as("pptx", json!({})));
    assert!(app.print_pdf(json!({})));
    let got: Vec<(String, bool, bool)> = handed.borrow().iter().map(|(n, a, d, s)| (n.clone(), *a && *d == uid, *s)).collect();
    assert_eq!(
        got,
        [
            ("Presentation1.pptx".to_string(), false, true),
            ("Presentation1.pptx".to_string(), false, true),
            ("Presentation1.pptx".to_string(), true, true),
            ("Presentation1.pptx".to_string(), true, true),
            ("Presentation1.deckcraft".to_string(), true, true),
            ("Presentation1.pptx".to_string(), true, false),
            ("Presentation1.pdf".to_string(), true, false),
        ],
    );
}

/// A page that doesn't confirm saves has saved the presentation once it has the file; one that
/// does leaves it unsaved for the page to mark.
#[test]
fn a_page_that_confirms_saves_marks_them_itself() {
    for confirms in [false, true] {
        let (mut app, _) = host_app(confirms);
        app.run("file.new", json!({})).unwrap();
        app.run("slide.new", json!({})).unwrap();
        assert!(app.save());
        assert_eq!(app.session.active().unwrap().is_dirty(), confirms, "confirms: {confirms}");
        // An export is a copy: it never saves the presentation.
        let (mut app, _) = host_app(confirms);
        app.run("file.new", json!({})).unwrap();
        app.run("slide.new", json!({})).unwrap();
        assert!(app.export_as("pptx", json!({})));
        assert!(app.session.active().unwrap().is_dirty());
    }
}

/// A page that didn't get the file: the Save Changes prompt's Save keeps the presentation.
#[test]
fn close_then_save_keeps_the_presentation_when_the_page_did_not_get_it() {
    let (mut app, _) = host_app(false);
    app.services.host_save = Some(Box::new(|_: &HostSave<'_>| Err("the page is gone".into())));
    let (_, second) = two_presentations(&mut app);
    let mut h = harness(app);
    h.state_mut().close_presentation();
    h.run_steps(2);
    h.key_press(egui::Key::Enter);
    h.run_steps(3);
    assert_eq!(h.state().session.active().map(|d| d.uid), Some(second));
    assert!(status(&h).contains("the page is gone"), "{:?}", status(&h));
}

/// A damaged file opens as a repaired copy, which must not be saved over the file; an intact
/// one may be. An outline goes into the open presentation and opens none.
#[test]
fn a_repaired_copy_is_not_offered_as_the_file_it_came_from() {
    let mut p = deckcraft_model::Presentation::default();
    p.add_media("a.png", "image/png", b"PICTURE-BYTES-0123456789".to_vec());
    let good = deckcraft_engine::cmd::file::save_bytes(&p, "deckcraft").unwrap();
    // Pictures are stored as they are, so flipping a byte breaks the entry's CRC.
    let mut bad = good.clone();
    let at = bad.windows(13).position(|w| w == b"PICTURE-BYTES").unwrap();
    bad[at] ^= 0xff;
    let (mut app, _) = host_app(true);
    app.run("file.new", json!({})).unwrap();
    let opened = app.open_or_insert_bytes("Broken.deckcraft", &bad).expect("the repaired copy opens");
    assert_eq!(app.session.active().map(|d| d.uid), Some(opened.document));
    assert!(app.session.active().unwrap().title.ends_with("(Repaired)"));
    assert!(!opened.in_place, "a repaired copy goes to a new file");
    let opened = app.open_or_insert_bytes("Fine.deckcraft", &good).expect("opens");
    assert_eq!(opened, OpenedFile { document: app.session.active().unwrap().uid, in_place: true });
    let count = app.session.documents().len();
    assert_eq!(app.open_or_insert_bytes("notes.md", b"# One\n# Two\n"), None);
    assert_eq!(app.session.documents().len(), count);
    assert_eq!(app.open_or_insert_bytes("junk.pptx", b"not a presentation"), None);
}

/// A comment being written is unsaved work although every presentation is saved.
#[test]
fn a_draft_counts_as_unsaved_work() {
    let (mut app, _) = host_app(true);
    app.run("file.new", json!({})).unwrap();
    let mut h = harness(app);
    assert!(!h.state().has_unsaved_work());
    h.state_mut().run("app.dialog", json!({"id": "comment"})).unwrap();
    h.run_steps(2);
    assert!(!h.state().has_unsaved_work(), "an empty comment is nothing to lose");
    h.state_mut().dialog.as_mut().unwrap().fields.insert("text".into(), "Check the figures".into());
    h.run_steps(2);
    assert!(h.state().has_unsaved_work(), "the typed comment would be lost");
    assert!(!h.state().session.active().unwrap().is_dirty());
    h.state_mut().dialog = None;
    h.run_steps(2);
    assert!(!h.state().has_unsaved_work());
}

/// The active presentation as saved, so only typing that is not in it yet is unsaved work.
fn mark_saved(app: &mut SlideApp) {
    let d = app.session.active_mut().unwrap();
    d.saved_doc = d.doc.clone();
}

/// The text field hinted `hint`. A field keeps its hint while it holds text, though only an empty
/// one exposes it as its placeholder.
fn field<'a>(h: &'a Harness<'_, SlideApp>, role: Role, hint: &str) -> Node<'a> {
    h.query_all_by_role(role)
        .find(|n| n.accesskit_node().data().placeholder() == Some(hint))
        .unwrap_or_else(|| panic!("no field hinted {hint:?} is drawn"))
}

/// A comment being written is still unsaved work while a slide show covers its dialog: nothing
/// draws the dialog then, but it comes back with the text.
#[test]
fn a_comment_being_written_counts_as_unsaved_work_while_a_slide_show_covers_its_dialog() {
    let (mut app, _) = host_app(true);
    app.run("file.new", json!({})).unwrap();
    let mut h = harness(app);
    h.state_mut().run("app.dialog", json!({"id": "comment"})).unwrap();
    h.run_steps(2);
    h.state_mut().dialog.as_mut().unwrap().fields.insert("text".into(), "Check the figures".into());
    h.state_mut().start_show(0, false);
    h.run_steps(3);
    assert!(h.state().show.is_some() && h.state().dialog.is_some(), "the show covers the open dialog");
    assert!(h.state().has_unsaved_work(), "the typed comment is still there");
    h.state_mut().show = None;
    h.run_steps(2);
    assert!(h.state().has_unsaved_work(), "and it is when the dialog is back");
    h.state_mut().dialog = None;
    h.run_steps(2);
    assert!(!h.state().has_unsaved_work());
}

/// A reply typed under a comment and not posted is unsaved work whether or not the Comments pane
/// is drawn: the pane shows the reply again when it comes back, so hiding the pane leaves it
/// counting, and posting the reply ends the draft.
#[test]
fn a_reply_draft_counts_as_unsaved_work_with_the_comments_pane_hidden() {
    let (mut app, _) = host_app(true);
    app.run("file.new", json!({})).unwrap();
    app.run("comment.add", json!({"text": "Check the figures"})).unwrap();
    mark_saved(&mut app);
    app.run("view.pane", json!({"pane": "comments"})).unwrap();
    let uid = app.session.active().unwrap().uid;
    let mut h = harness(app);
    assert!(!h.state().has_unsaved_work());

    field(&h, Role::TextInput, "Reply…").click();
    h.run_steps(1);
    h.event(egui::Event::Text("Will do".into()));
    h.run_steps(2);
    assert!(h.state().has_unsaved_work(), "the typed reply would be lost");
    assert!(!h.state().session.active().unwrap().is_dirty(), "it is not in the presentation");

    // With the pane hidden nothing draws the reply, but it is still there to come back.
    h.state_mut().run("view.pane", json!({})).unwrap();
    h.run_steps(3);
    assert!(h.query_all_by_role(Role::TextInput).all(|n| n.accesskit_node().data().placeholder() != Some("Reply…")), "the pane is not drawn");
    assert!(h.state().has_unsaved_work(), "hiding the pane doesn't drop the reply");
    h.state_mut().run("view.pane", json!({"pane": "comments"})).unwrap();
    h.run_steps(3);
    assert_eq!(field(&h, Role::TextInput, "Reply…").value().as_deref(), Some("Will do"), "the pane shows the reply again");
    assert!(h.state().has_unsaved_work());

    // Posting it puts the reply in the presentation and ends the draft.
    field(&h, Role::TextInput, "Reply…").click();
    h.run_steps(1);
    h.key_press(egui::Key::Enter);
    h.run_steps(3);
    let comment = &h.state().session.active().unwrap().doc.slides[0].comments[0];
    assert_eq!(comment.replies.iter().map(|r| r.text.as_str()).collect::<Vec<_>>(), ["Will do"]);
    assert_eq!(h.state().drafts.reply(uid, 0, 0), "", "the posted reply is not a draft any more");
    mark_saved(h.state_mut());
    assert!(!h.state().has_unsaved_work(), "nothing is left to lose");
}

/// Speaker notes typed and not committed yet are unsaved work although the notes pane is hidden;
/// the pane puts them in the presentation when it comes back.
#[test]
fn speaker_notes_being_typed_count_as_unsaved_work_with_the_notes_pane_hidden() {
    let (mut app, _) = host_app(true);
    app.run("file.new", json!({})).unwrap();
    app.run("view.notes", json!({"on": true})).unwrap();
    let mut h = harness(app);
    assert!(!h.state().has_unsaved_work());
    field(&h, Role::MultilineTextInput, "Click to add notes").click();
    h.run_steps(1);
    h.event(egui::Event::Text("Open with the figures".into()));
    h.run_steps(2);
    assert!(h.state().has_unsaved_work(), "the typed notes would be lost");
    assert!(!h.state().session.active().unwrap().is_dirty(), "they are not in the presentation");

    h.state_mut().run("view.notes", json!({"on": false})).unwrap();
    h.run_steps(3);
    assert!(h.state().has_unsaved_work(), "hiding the pane doesn't drop the notes");
    h.state_mut().run("view.notes", json!({"on": true})).unwrap();
    h.run_steps(3);
    let st = h.state().session.active().unwrap();
    assert_eq!(st.doc.slides[0].notes_text(), "Open with the figures");
    assert!(st.is_dirty(), "the pane committed them");
    mark_saved(h.state_mut());
    assert!(!h.state().has_unsaved_work(), "committed notes are no draft");
}

/// Alt text typed for a shape and not applied is unsaved work although the Format Shape pane is
/// hidden; the pane shows it again when it comes back, and applying it ends the draft.
#[test]
fn alt_text_being_typed_counts_as_unsaved_work_with_the_format_pane_hidden() {
    let (mut app, _) = host_app(true);
    app.run("file.new", json!({})).unwrap();
    let added = app.session.execute("shape.insert", &json!({"preset": "rect", "rect": [80.0, 80.0, 200.0, 120.0]})).unwrap();
    let shape = added.get("id").and_then(serde_json::Value::as_u64).unwrap();
    app.session.execute("edit.select", &json!({"ids": [shape]})).unwrap();
    mark_saved(&mut app);
    app.run("view.pane", json!({"pane": "format", "tab": "size"})).unwrap();
    let mut h = harness_sized(app, egui::vec2(1400.0, 1800.0));
    assert!(!h.state().has_unsaved_work());

    field(&h, Role::MultilineTextInput, "Alt text").click();
    h.run_steps(1);
    h.event(egui::Event::Text("A bar chart".into()));
    h.run_steps(2);
    assert!(h.state().has_unsaved_work(), "the typed alt text would be lost");
    assert!(!h.state().session.active().unwrap().is_dirty(), "it is not on the shape");

    h.state_mut().run("view.pane", json!({})).unwrap();
    h.run_steps(3);
    assert!(h.state().has_unsaved_work(), "hiding the pane doesn't drop the alt text");
    h.state_mut().run("view.pane", json!({"pane": "format", "tab": "size"})).unwrap();
    h.run_steps(3);
    assert_eq!(field(&h, Role::MultilineTextInput, "Alt text").value().as_deref(), Some("A bar chart"), "the pane shows it again");

    // Leaving the field applies it.
    field(&h, Role::MultilineTextInput, "Alt text").click();
    h.run_steps(1);
    h.query_all_by_label("Alt Text").find(|n| n.accesskit_node().role() == Role::Label).expect("the Alt Text heading").click();
    h.run_steps(3);
    let descr = h.state().session.active().unwrap().doc.slides[0].shapes.last().unwrap().descr.clone();
    assert_eq!(descr, "A bar chart");
    mark_saved(h.state_mut());
    assert!(!h.state().has_unsaved_work(), "applied alt text is no draft");
}
