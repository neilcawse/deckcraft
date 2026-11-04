//! The `postMessage` bridge to a same-origin parent page (`?host=parent`), for pages that embed
//! DeckCraft in an `<iframe>`. Messages are plain objects with a `type`:
//!
//! - `deckcraft:ready` (to the parent): `{ version }`, the app is listening; send a file now.
//! - `deckcraft:open` (from the parent): `{ name, bytes }`, bytes an `ArrayBuffer` or
//!   `Uint8Array`. It opens like a presentation picked with File › Open or dropped on the window.
//! - `deckcraft:opened` (to the parent): `{ name, docId }` once a sent file has been opened:
//!   `docId` is the presentation it became, or `null` when it must not be written over the sent
//!   file: it didn't open, went into the open presentation (an outline), or opened as a repaired
//!   copy of a damaged file.
//! - `deckcraft:save` (to the parent): `{ name, bytes, docId, saveId, saveAs }` (`bytes` a
//!   transferred `ArrayBuffer`) for every file DeckCraft would otherwise download: Save and Save
//!   As (.pptx / .deckcraft), Export and Print. `docId` is the presentation the bytes were made
//!   from, the same number `deckcraft:opened` gave for a sent file, so the parent writes it back
//!   only to that file. `saveAs` is true when the command asked for a new file (Save As, Export,
//!   Print), false for Save. A file the control channel writes by path has `docId: null` and
//!   `saveAs: true`: it is never a presentation's file. `saveId` numbers the saves. The parent
//!   stores it and reports failures itself.
//! - `deckcraft:saved` (from the parent, with `?ack=1`): `{ saveId, name }`, the save is stored
//!   as `name`. Only then does the presentation count as saved, as it was when it was saved; a
//!   Save Changes prompt's Save leaves the presentation open until then.
//!   Without `?ack=1` a presentation counts as saved once it is handed over.
//! - `deckcraft:discard` (from the parent, with `?ack=1`): `{ saveId }`, the parent will never
//!   store that save (the picker was dismissed, the write failed, a newer save replaced it); the
//!   presentation stays unsaved.
//! - `deckcraft:dirty` (to the parent): `{ dirty }` whenever unsaved work appears or goes away:
//!   a presentation with unsaved changes, or typing not yet in one (notes, a comment…).
//! - `deckcraft:request-save` (from the parent): save the active presentation as File › Save
//!   would, so a parent's own Save button or shortcut gets a `deckcraft:save` back.
//! - `deckcraft:failed` (to the parent): `{ error }` when the app couldn't start.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::Arc;

use deckcraft_engine::model::Presentation;
use deckcraft_ui_egui::{HostSave, Services, SlideApp};
use wasm_bindgen::JsCast as _;
use wasm_bindgen::JsValue;
use wasm_bindgen::closure::Closure;

use crate::unacked::{Pending, Unacked};

const MAX_AUTHOR_CHARS: usize = 200;

#[derive(Clone)]
pub struct Bridge {
    window: web_sys::Window,
    parent: web_sys::Window,
    origin: String,
    /// `?ack=1`: a presentation counts as saved when the parent says it stored it
    /// (`deckcraft:saved`), not when it is handed over.
    ack: bool,
    /// Files the parent sent that the app hasn't been given yet.
    sent: Rc<RefCell<VecDeque<(String, Vec<u8>)>>>,
    /// Saves the parent stored since the last frame: `(saveId, name)`.
    stored: Rc<RefCell<Vec<(u64, String)>>>,
    unacked: Rc<RefCell<Pending<Arc<Presentation>>>>,
    next_save_id: Rc<Cell<u64>>,
    /// The parent asked for a save (`deckcraft:request-save`) since the last frame.
    save_requested: Rc<Cell<bool>>,
    /// The unsaved-work state last reported to the parent.
    reported: Rc<Cell<Option<bool>>>,
}

impl Bridge {
    /// `None` when the page isn't framed (there is no parent to talk to).
    pub fn new(ack: bool) -> Option<Self> {
        let window = web_sys::window()?;
        let parent = window.parent().ok().flatten()?;
        if js_sys::Object::is(&parent, &window) {
            return None;
        }
        let origin = window.location().origin().ok()?;
        Some(Self {
            window,
            parent,
            origin,
            ack,
            sent: Rc::default(),
            stored: Rc::default(),
            unacked: Rc::default(),
            next_save_id: Rc::new(Cell::new(1)),
            save_requested: Rc::default(),
            reported: Rc::default(),
        })
    }

    /// Hand saved, exported and written files to the parent instead of downloading them, and
    /// hear files from it.
    pub fn connect(&self, services: &mut Services, ctx: egui::Context) {
        self.listen(ctx);
        // Save, Save As, Export and Print go to the parent at once, with the presentation their
        // bytes were made from, so a failed hand-off fails the save.
        let bridge = self.clone();
        services.host_save = Some(Box::new(move |save: &HostSave<'_>| bridge.hand_over(save)));
        services.host_confirms_saves = self.ack;
        // Nothing saves through `download` while `host_save` is set; it marks the browser build.
        let bridge = self.clone();
        services.download = Some(Box::new(move |name: &str, bytes: &[u8]| {
            if let Err(e) = bridge.post_save(name, bytes, None, true) {
                log::error!("couldn't hand {name} to the page: {e}");
            }
        }));
        // Control-channel writes by path (`app.screenshot {path}`): never a presentation's file.
        let bridge = self.clone();
        services.write = Some(Box::new(move |name: &str, bytes: &[u8]| bridge.post_save(name, bytes, None, true).map(|_| ())));
    }

    pub fn set_author(app: &mut SlideApp, author: &str) {
        let author: String = author.trim().chars().take(MAX_AUTHOR_CHARS).collect();
        if !author.is_empty() {
            app.session.prefs.author = author;
        }
    }

    pub fn post_ready(&self) {
        self.send(&[("type", "deckcraft:ready".into()), ("version", env!("CARGO_PKG_VERSION").into())]);
    }

    pub fn post_failed(&self, error: &str) {
        self.send(&[("type", "deckcraft:failed".into()), ("error", error.into())]);
    }

    /// After each frame: a save the parent asked for runs, stored saves mark their presentation
    /// saved, the next sent file opens (and the parent hears which presentation it became), and
    /// the parent hears when unsaved work appears or goes away.
    pub fn sync(&self, app: &mut SlideApp, ctx: &egui::Context) {
        if self.save_requested.take() && app.session.active().is_some() {
            app.save();
        }
        self.apply_stored(app);
        let next = self.sent.borrow_mut().pop_front();
        if let Some((name, bytes)) = next {
            self.open_sent(app, &name, &bytes);
            ctx.request_repaint();
        }
        let dirty = app.has_unsaved_work();
        if self.reported.get() != Some(dirty) && self.send(&[("type", "deckcraft:dirty".into()), ("dirty", dirty.into())]) {
            self.reported.set(Some(dirty));
        }
    }

    /// Open a sent file like a dropped one and tell the parent which presentation it became. The
    /// untouched blank presentation the page started with gives way to it.
    fn open_sent(&self, app: &mut SlideApp, name: &str, bytes: &[u8]) {
        let replace_initial =
            app.session.documents().len() == 1 && app.session.documents().first().is_some_and(|d| d.path.is_none() && !d.is_dirty());
        let opened = app.open_or_insert_bytes(name, bytes);
        if replace_initial && opened.is_some() && app.session.documents().len() > 1 {
            app.session.close_document(0);
        }
        let doc = opened.filter(|o| o.in_place).map(|o| o.document);
        self.send(&[("type", "deckcraft:opened".into()), ("name", name.into()), ("docId", id_value(doc))]);
    }

    /// Post a Save, Save As, Export or Print. With `?ack=1` a save of the presentation waits for
    /// the parent to store it.
    fn hand_over(&self, save: &HostSave<'_>) -> Result<(), String> {
        let save_id = self.post_save(save.name, save.bytes, Some(save.document), save.save_as)?;
        if self.ack
            && let Some(content) = &save.saved
        {
            self.unacked.borrow_mut().push(Unacked { save_id, doc: save.document, content: Arc::clone(content) });
        }
        Ok(())
    }

    /// A stored save marks its presentation saved as it was then (later edits stay unsaved) and
    /// names an untitled presentation after the file. Older saves of that presentation won't be
    /// stored after a newer one, and saves of closed presentations are forgotten.
    fn apply_stored(&self, app: &mut SlideApp) {
        let stored = std::mem::take(&mut *self.stored.borrow_mut());
        let mut unacked = self.unacked.borrow_mut();
        for (save_id, name) in stored {
            let Some(save) = unacked.stored(save_id) else { continue };
            let Some(doc) = app.session.document_mut(save.doc) else { continue };
            mark_saved(doc, save.content, &name);
        }
        let open: Vec<u64> = app.session.documents().iter().map(|d| d.uid).collect();
        unacked.retain_open(&open);
    }

    /// Post `deckcraft:save`; returns its `saveId`.
    fn post_save(&self, path: &str, bytes: &[u8], doc: Option<u64>, save_as: bool) -> Result<u64, String> {
        let name = std::path::Path::new(path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| path.to_string());
        let save_id = self.next_save_id.get();
        self.next_save_id.set(save_id.saturating_add(1));
        let buffer = js_sys::Uint8Array::from(bytes).buffer();
        let message = message(&[
            ("type", "deckcraft:save".into()),
            ("name", name.as_str().into()),
            ("bytes", buffer.clone().into()),
            ("docId", id_value(doc)),
            ("saveId", id_value(Some(save_id))),
            ("saveAs", save_as.into()),
        ])
        .ok_or_else(|| "couldn't build the message".to_string())?;
        self.parent
            .post_message_with_transfer(&message, &self.origin, &js_sys::Array::of1(&buffer))
            .map_err(|e| e.as_string().unwrap_or_else(|| format!("{e:?}")))?;
        Ok(save_id)
    }

    fn listen(&self, ctx: egui::Context) {
        let parent = self.parent.clone();
        let origin = self.origin.clone();
        let sent = Rc::clone(&self.sent);
        let stored = Rc::clone(&self.stored);
        let unacked = Rc::clone(&self.unacked);
        let save_requested = Rc::clone(&self.save_requested);
        let on_message = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |event: web_sys::MessageEvent| {
            if event.origin() != origin || !event.source().is_some_and(|s| js_sys::Object::is(&s, &parent)) {
                return;
            }
            let data = event.data();
            match string(&data, "type").as_deref() {
                Some("deckcraft:open") => {
                    let name = string(&data, "name").filter(|n| !n.trim().is_empty()).unwrap_or_else(|| "Presentation.pptx".to_string());
                    let Some(bytes) = bytes(&data) else { return };
                    sent.borrow_mut().push_back((name, bytes));
                }
                Some("deckcraft:saved") => {
                    let Some(save_id) = number(&data, "saveId") else { return };
                    let Some(name) = string(&data, "name").filter(|n| !n.trim().is_empty()) else { return };
                    stored.borrow_mut().push((save_id, name));
                }
                Some("deckcraft:discard") => {
                    if let Some(save_id) = number(&data, "saveId") {
                        unacked.borrow_mut().discard(save_id);
                    }
                    return;
                }
                Some("deckcraft:request-save") => save_requested.set(true),
                _ => return,
            }
            ctx.request_repaint();
        });
        if self.window.add_event_listener_with_callback("message", on_message.as_ref().unchecked_ref()).is_ok() {
            on_message.forget();
        }
    }

    fn send(&self, fields: &[(&str, JsValue)]) -> bool {
        message(fields).is_some_and(|m| self.parent.post_message(&m, &self.origin).is_ok())
    }
}

/// The presentation counts as saved with `content`, and an untitled one takes the file's name.
fn mark_saved(doc: &mut deckcraft_engine::DocState, content: Arc<Presentation>, name: &str) {
    doc.saved_doc = content;
    if doc.path.is_none() {
        doc.title = std::path::Path::new(name).file_stem().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| name.to_string());
    }
}

/// A presentation id or save number as a JS number (ids stay far below 2^53), `null` for none.
fn id_value(id: Option<u64>) -> JsValue {
    id.map_or(JsValue::NULL, |id| JsValue::from_f64(id as f64))
}

fn message(fields: &[(&str, JsValue)]) -> Option<js_sys::Object> {
    let m = js_sys::Object::new();
    for (key, value) in fields {
        js_sys::Reflect::set(&m, &JsValue::from_str(key), value).ok()?;
    }
    Some(m)
}

fn string(data: &JsValue, key: &str) -> Option<String> {
    js_sys::Reflect::get(data, &JsValue::from_str(key)).ok()?.as_string()
}

/// A whole, non-negative number field.
fn number(data: &JsValue, key: &str) -> Option<u64> {
    let n = js_sys::Reflect::get(data, &JsValue::from_str(key)).ok()?.as_f64()?;
    (n >= 0.0 && n.fract() == 0.0 && n < 9_007_199_254_740_992.0).then_some(n as u64)
}

fn bytes(data: &JsValue) -> Option<Vec<u8>> {
    let bytes = js_sys::Reflect::get(data, &JsValue::from_str("bytes")).ok()?;
    if let Some(array) = bytes.dyn_ref::<js_sys::Uint8Array>() {
        Some(array.to_vec())
    } else if bytes.is_instance_of::<js_sys::ArrayBuffer>() {
        Some(js_sys::Uint8Array::new(&bytes).to_vec())
    } else {
        None
    }
}
