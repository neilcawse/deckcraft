//! The `postMessage` bridge to a same-origin parent page (`?host=parent`), for pages that embed
//! DeckCraft in an `<iframe>`. Messages are plain objects with a `type`:
//!
//! - `deckcraft:ready` (to the parent): `{ version }`, the app is listening; send a file now.
//! - `deckcraft:open` (from the parent): `{ name, bytes }`, bytes an `ArrayBuffer` or
//!   `Uint8Array`. It opens like a presentation picked with File › Open or dropped on the window.
//! - `deckcraft:opened` (to the parent): `{ name, docId }` once a sent file has opened: `docId`
//!   is the presentation it became, or `null` when it didn't open.
//! - `deckcraft:save` (to the parent): `{ name, bytes, docId, saveId }` (`bytes` a transferred
//!   `ArrayBuffer`) for every file DeckCraft would otherwise download: Save and Save As
//!   (.pptx / .deckcraft) and Export. `docId` is the presentation it came from (`null` when none
//!   is open), the same number `deckcraft:opened` gave for a sent file, so the parent writes it
//!   back only to that file. `saveId` numbers the saves. The parent stores it and reports
//!   failures itself.
//! - `deckcraft:saved` (from the parent, with `?ack=1`): `{ saveId, name }`, the save is stored
//!   as `name`. Only then does the presentation count as saved, as it was when it was saved.
//!   Without `?ack=1` a presentation counts as saved once it is handed over.
//! - `deckcraft:dirty` (to the parent): `{ dirty }` whenever unsaved changes appear or go away.
//! - `deckcraft:request-save` (from the parent): save the active presentation as File › Save
//!   would, so a parent's own Save button or shortcut gets a `deckcraft:save` back.
//! - `deckcraft:failed` (to the parent): `{ error }` when the app couldn't start.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::Arc;

use deckcraft_engine::model::Presentation;
use deckcraft_ui_egui::{Services, SlideApp};
use wasm_bindgen::JsCast as _;
use wasm_bindgen::JsValue;
use wasm_bindgen::closure::Closure;

/// Saving one of these hands the whole presentation to the parent, which stores it: the
/// presentation counts as saved under that name, as it would after Save on the desktop.
const PRESENTATION_EXTS: &[&str] = &["pptx", "potx", "ppsx", "deckcraft"];
const MAX_AUTHOR_CHARS: usize = 200;

/// Files by name: `(name, bytes)`.
type Files = Rc<RefCell<Vec<(String, Vec<u8>)>>>;

/// A presentation handed to the parent and not yet acknowledged: the presentation it came from
/// and its content then.
struct Unacked {
    save_id: u64,
    doc: u64,
    content: Arc<Presentation>,
}

#[derive(Clone)]
pub struct Bridge {
    window: web_sys::Window,
    parent: web_sys::Window,
    origin: String,
    /// `?ack=1`: a presentation counts as saved when the parent says it stored it
    /// (`deckcraft:saved`), not when it is handed over.
    ack: bool,
    /// Files DeckCraft saved since the last frame, posted after it with the presentation they
    /// came from.
    saves: Files,
    /// Files the parent sent that the app hasn't been given yet.
    sent: Rc<RefCell<VecDeque<(String, Vec<u8>)>>>,
    /// The sent file the app was given last frame, and the presentations open before it.
    opening: Rc<RefCell<Option<(String, Vec<u64>)>>>,
    /// Saves the parent stored since the last frame: `(saveId, name)`.
    stored: Rc<RefCell<Vec<(u64, String)>>>,
    unacked: Rc<RefCell<Vec<Unacked>>>,
    next_save_id: Rc<Cell<u64>>,
    /// The parent asked for a save (`deckcraft:request-save`) since the last frame.
    save_requested: Rc<Cell<bool>>,
    /// The unsaved-changes state last reported to the parent.
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
            saves: Rc::default(),
            sent: Rc::default(),
            opening: Rc::default(),
            stored: Rc::default(),
            unacked: Rc::default(),
            next_save_id: Rc::new(Cell::new(1)),
            save_requested: Rc::default(),
            reported: Rc::default(),
        })
    }

    /// Send saved and exported files to the parent instead of downloading them, and hear files
    /// from it.
    pub fn connect(&self, services: &mut Services, ctx: egui::Context) {
        self.listen(ctx.clone());
        let saves = Rc::clone(&self.saves);
        let repaint = ctx.clone();
        services.download = Some(Box::new(move |name: &str, bytes: &[u8]| {
            saves.borrow_mut().push((name.to_string(), bytes.to_vec()));
            repaint.request_repaint();
        }));
        let saves = Rc::clone(&self.saves);
        services.write = Some(Box::new(move |name: &str, bytes: &[u8]| {
            saves.borrow_mut().push((name.to_string(), bytes.to_vec()));
            ctx.request_repaint();
            Ok(())
        }));
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

    /// After each frame: a save the parent asked for runs, saves go to the parent with the
    /// presentation they came from, stored saves mark their presentation saved, the parent hears
    /// which presentation the file it sent became, the next sent file goes to the app, and the
    /// parent hears when unsaved changes appear or go away.
    pub fn sync(&self, app: &mut SlideApp, ctx: &egui::Context) {
        if self.save_requested.take() && app.session.active().is_some() {
            app.save();
        }
        self.post_saves(app);
        self.apply_stored(app);
        if let Some((name, before)) = self.opening.take() {
            let docs = app.session.documents();
            let doc = crate::doc_ids::opened_doc(&before, docs.iter().map(|d| (d.uid, d.title.as_str())), &name);
            self.send(&[("type", "deckcraft:opened".into()), ("name", name.as_str().into()), ("docId", id_value(doc))]);
        }
        let next = self.sent.borrow_mut().pop_front();
        if let Some((name, bytes)) = next {
            self.open_sent(app, name, &bytes);
            ctx.request_repaint();
        }
        let dirty = app.session.documents().iter().any(|d| d.is_dirty());
        if self.reported.get() != Some(dirty) && self.send(&[("type", "deckcraft:dirty".into()), ("dirty", dirty.into())]) {
            self.reported.set(Some(dirty));
        }
    }

    /// Open a sent file like a dropped one. The untouched blank presentation the page started
    /// with gives way to it.
    fn open_sent(&self, app: &mut SlideApp, name: String, bytes: &[u8]) {
        let before: Vec<u64> = app.session.documents().iter().map(|d| d.uid).collect();
        let replace_initial =
            app.session.documents().len() == 1 && app.session.documents().first().is_some_and(|d| d.path.is_none() && !d.is_dirty());
        app.open_or_insert_bytes(&name, bytes);
        if replace_initial && app.session.documents().len() > 1 {
            app.session.close_document(0);
        }
        *self.opening.borrow_mut() = Some((name, before));
    }

    fn post_saves(&self, app: &mut SlideApp) {
        let saves = std::mem::take(&mut *self.saves.borrow_mut());
        for (path, bytes) in saves {
            let name = std::path::Path::new(&path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or(path);
            let save_id = self.next_save_id.get();
            self.next_save_id.set(save_id + 1);
            let doc = app.session.active().map(|d| d.uid);
            if let Err(e) = self.post_save(&name, &bytes, doc, save_id) {
                log::error!("couldn't hand {name} to the page: {e}");
                continue;
            }
            if !is_presentation(&name) {
                continue;
            }
            let Some(active) = app.session.active_mut() else { continue };
            let content = Arc::clone(&active.doc);
            if self.ack {
                self.unacked.borrow_mut().push(Unacked { save_id, doc: active.uid, content });
            } else {
                mark_saved(active, content, &name);
            }
        }
    }

    /// A stored save marks its presentation saved as it was then (later edits stay unsaved) and
    /// names an untitled presentation after the file. Older saves of that presentation won't be
    /// stored after a newer one, and saves of closed presentations are forgotten.
    fn apply_stored(&self, app: &mut SlideApp) {
        let stored = std::mem::take(&mut *self.stored.borrow_mut());
        let mut unacked = self.unacked.borrow_mut();
        for (save_id, name) in stored {
            let Some(at) = unacked.iter().position(|u| u.save_id == save_id) else { continue };
            let save = unacked.remove(at);
            unacked.retain(|u| u.doc != save.doc || u.save_id > save_id);
            let Some(doc) = app.session.document_mut(save.doc) else { continue };
            mark_saved(doc, save.content, &name);
        }
        let open: Vec<u64> = app.session.documents().iter().map(|d| d.uid).collect();
        unacked.retain(|u| open.contains(&u.doc));
    }

    fn post_save(&self, name: &str, bytes: &[u8], doc: Option<u64>, save_id: u64) -> Result<(), String> {
        let buffer = js_sys::Uint8Array::from(bytes).buffer();
        let message = message(&[
            ("type", "deckcraft:save".into()),
            ("name", name.into()),
            ("bytes", buffer.clone().into()),
            ("docId", id_value(doc)),
            ("saveId", id_value(Some(save_id))),
        ])
        .ok_or_else(|| "couldn't build the message".to_string())?;
        self.parent
            .post_message_with_transfer(&message, &self.origin, &js_sys::Array::of1(&buffer))
            .map_err(|e| e.as_string().unwrap_or_else(|| format!("{e:?}")))
    }

    fn listen(&self, ctx: egui::Context) {
        let parent = self.parent.clone();
        let origin = self.origin.clone();
        let sent = Rc::clone(&self.sent);
        let stored = Rc::clone(&self.stored);
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

fn is_presentation(name: &str) -> bool {
    name.rsplit_once('.').is_some_and(|(_, ext)| PRESENTATION_EXTS.iter().any(|e| ext.eq_ignore_ascii_case(e)))
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
