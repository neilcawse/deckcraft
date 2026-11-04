//! Web inside a page that keeps the files (`Services::host_save`, `?host=parent`): each file is
//! handed over with the presentation it was made from and whether it asked for a new file, a
//! Save Changes prompt's Save doesn't lose a presentation the page hasn't stored yet, a repaired
//! copy isn't offered as the file it came from, and typing not yet in a presentation counts as
//! unsaved work.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::cell::RefCell;
use std::rc::Rc;

use deckcraft_engine::Session;
use egui_kittest::Harness;
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
    let mut h = Harness::builder().with_size(egui::vec2(1400.0, 900.0)).build_ui_state(
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
