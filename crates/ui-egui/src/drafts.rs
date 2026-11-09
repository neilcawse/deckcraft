//! Text typed into a pane and not in the presentation yet, which the app keeps itself instead of in
//! egui's temporary data. The pane that shows it can go away (hidden, another view or tab, a slide
//! show) while the text stays and comes back with the pane, so it counts as unsaved work
//! ([`SlideApp::has_unsaved_work`](crate::SlideApp::has_unsaved_work)) whether or not anything draws
//! it. A draft belongs to what it was typed for, never to a place in a list: a slide is told by its
//! id, a comment by the number it carries, a shape by its id, a master or layout by its id. So
//! moving, deleting or undoing slides and comments neither moves a draft to another one nor loses
//! it, and a draft goes when its presentation closes or what it was typed for is gone or changed.
//! The pane that draws a draft enters it into the presentation the next time the field is drawn
//! without the focus, as leaving the field would have, into the slide it was typed for whichever
//! slide is current. A Save enters what a pane that isn't drawn still holds. A reply is the one
//! draft that only posting enters: it stays, and keeps its presentation from closing unasked.

use std::collections::HashMap;

use deckcraft_engine::{DocState, Selection, Session, Target};
use deckcraft_model::{Comment, LayoutId, MasterId, Presentation, Shape, ShapeId, Slide, SlideId, TextBody};
use serde_json::{Value, json};

use crate::{SlideApp, notes};

/// The open presentation `doc`.
fn document(session: &Session, doc: u64) -> Option<&DocState> {
    session.documents().iter().find(|d| d.uid == doc)
}

/// Slide `slide` of the open presentation `doc`.
pub(crate) fn slide_of(session: &Session, doc: u64, slide: SlideId) -> Option<&Slide> {
    document(session, doc)?.doc.slide(slide)
}

/// Where slide `slide` of the open presentation `doc` is in the deck now.
pub(crate) fn slide_index(session: &Session, doc: u64, slide: SlideId) -> Option<usize> {
    document(session, doc)?.doc.slide_index(slide)
}

/// Whether `shape` is a title placeholder.
fn is_title(shape: &Shape) -> bool {
    shape.ph_type().is_some_and(|k| k.is_title())
}

/// The title the Outline shows for a slide with `shapes`: the first title placeholder and its text
/// on one line.
pub(crate) fn outline_title(shapes: &[Shape]) -> Option<(ShapeId, String)> {
    let title = shapes.iter().find(|s| is_title(s))?;
    Some((title.id, title.text.as_ref().map(|t| t.text().replace('\u{b}', " ")).unwrap_or_default()))
}

/// The text the Outline shows for a text shape that is not the title: a line per paragraph, indented
/// by a tab per level.
pub(crate) fn outline_body(text: &TextBody) -> String {
    text.paragraphs.iter().map(|p| format!("{}{}", "\t".repeat(p.level as usize), p.text())).collect::<Vec<_>>().join("\n")
}

/// The shapes a draft is about: those of one slide, master, layout or page master. A slide is named
/// by its id, so moving it in the deck doesn't make it another.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Tree {
    Slide(SlideId),
    Master(MasterId),
    Layout(MasterId, LayoutId),
    Handout,
    NotesMaster,
}

impl Tree {
    /// The shapes the presentation's selection is editing.
    pub(crate) fn of(st: &DocState) -> Option<Tree> {
        match st.selection.target {
            Target::Slides => st.current_slide().map(|s| Tree::Slide(s.id)),
            Target::Master { master } => st.doc.masters.get(master).map(|m| Tree::Master(m.id)),
            Target::Layout { master, layout } => {
                let m = st.doc.masters.get(master)?;
                Some(Tree::Layout(m.id, m.layouts.get(layout)?.id))
            }
            Target::Handout => Some(Tree::Handout),
            Target::NotesMaster => Some(Tree::NotesMaster),
        }
    }

    /// The shapes in this tree now; none when it is gone.
    fn shapes(self, doc: &Presentation) -> Option<&[Shape]> {
        match self {
            Tree::Slide(id) => doc.slide(id).map(|s| s.shapes.as_slice()),
            Tree::Master(id) => doc.masters.iter().find(|m| m.id == id).map(|m| m.shapes.as_slice()),
            Tree::Layout(master, layout) => {
                doc.masters.iter().find(|m| m.id == master)?.layouts.iter().find(|l| l.id == layout).map(|l| l.shapes.as_slice())
            }
            Tree::Handout => doc.handout_master.as_ref().map(|m| m.shapes.as_slice()),
            Tree::NotesMaster => doc.notes_master.as_ref().map(|m| m.shapes.as_slice()),
        }
    }

    /// Points `sel` at this tree of `doc`, to edit its shapes: the slide or the master, layout or page
    /// master, with no shape selected. False when the tree is gone.
    fn point(self, doc: &Presentation, sel: &mut Selection) -> bool {
        let target = match self {
            Tree::Slide(id) => {
                let Some(index) = doc.slide_index(id) else { return false };
                sel.slide = index;
                Target::Slides
            }
            Tree::Master(id) => match doc.masters.iter().position(|m| m.id == id) {
                Some(master) => Target::Master { master },
                None => return false,
            },
            Tree::Layout(master, layout) => {
                let Some((mi, m)) = doc.masters.iter().enumerate().find(|(_, m)| m.id == master) else { return false };
                let Some(li) = m.layouts.iter().position(|l| l.id == layout) else { return false };
                Target::Layout { master: mi, layout: li }
            }
            Tree::Handout if doc.handout_master.is_some() => Target::Handout,
            Tree::NotesMaster if doc.notes_master.is_some() => Target::NotesMaster,
            Tree::Handout | Tree::NotesMaster => return false,
        };
        sel.target = target;
        sel.shapes.clear();
        sel.text = None;
        sel.cells = None;
        true
    }
}

/// The comment a reply is typed under: the comment itself, told by its [`CommentId`] and its slide,
/// never by where it is in the list or by what it says. The id stays with the comment when its slide
/// moves, when the comments around it are deleted and when the deck is undone, and comments that say
/// the same thing (a note imported twice) each have their own.
///
/// [`CommentId`]: deckcraft_model::CommentId
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ReplyOwner {
    doc: u64,
    slide: SlideId,
    comment: u64,
}

impl ReplyOwner {
    /// The owner of `comment`, which is on slide `slide` of the open presentation `doc`.
    pub(crate) fn of(doc: u64, slide: SlideId, comment: &Comment) -> ReplyOwner {
        ReplyOwner { doc, slide, comment: comment.id.get() }
    }

    /// Whether the comment is still on its slide.
    fn exists(&self, session: &Session) -> bool {
        slide_of(session, self.doc, self.slide).is_some_and(|s| s.comments.iter().any(|c| c.id.get() == self.comment))
    }
}

/// A text field the app keeps a draft for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Field {
    /// A shape's alt text, in the Format Shape pane.
    AltText,
    /// A slide's title in the Outline.
    OutlineTitle,
    /// A text shape other than the title, in the Outline.
    OutlineBody,
}

/// The field of a shape that text is typed into.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct TextOwner {
    doc: u64,
    tree: Tree,
    shape: ShapeId,
    field: Field,
}

impl TextOwner {
    /// The alt text of `shape` among the shapes of `tree`.
    pub(crate) fn alt_text(doc: u64, tree: Tree, shape: ShapeId) -> TextOwner {
        TextOwner { doc, tree, shape, field: Field::AltText }
    }

    /// The Outline's title of slide `slide`, the text of its title placeholder `shape`.
    pub(crate) fn outline_title(doc: u64, slide: SlideId, shape: ShapeId) -> TextOwner {
        TextOwner { doc, tree: Tree::Slide(slide), shape, field: Field::OutlineTitle }
    }

    /// The Outline's text of `shape` on slide `slide`.
    pub(crate) fn outline_body(doc: u64, slide: SlideId, shape: ShapeId) -> TextOwner {
        TextOwner { doc, tree: Tree::Slide(slide), shape, field: Field::OutlineBody }
    }

    /// What the field holds in the presentation now; none when what it is for is gone.
    fn current(&self, session: &Session) -> Option<String> {
        let shapes = self.tree.shapes(&document(session, self.doc)?.doc)?;
        match self.field {
            Field::AltText => deckcraft_model::find_shape(shapes, self.shape).map(|s| s.descr.clone()),
            Field::OutlineTitle => outline_title(shapes).filter(|(id, _)| *id == self.shape).map(|(_, text)| text),
            Field::OutlineBody => shapes.iter().find(|s| s.id == self.shape && !is_title(s)).and_then(|s| s.text.as_ref()).map(outline_body),
        }
    }

    /// The command that puts `typed` into the field, with its parameters. They name the shape, so they
    /// hold wherever the selection is as long as the editing is pointed at the field's shapes
    /// ([`SlideApp::run_in`]).
    fn enter_command(&self, typed: &str) -> (&'static str, Value) {
        match self.field {
            Field::AltText => ("shape.altText", json!({"text": typed, "ids": [self.shape.0]})),
            Field::OutlineTitle | Field::OutlineBody => ("text.set", json!({"id": self.shape.0, "text": typed})),
        }
    }
}

/// The text of each node of SmartArt graphic `group` among the shapes of `tree` in the open
/// presentation `doc`, as the Text Pane shows it.
pub(crate) fn smartart_texts(session: &Session, doc: u64, tree: Tree, group: ShapeId) -> Option<Vec<String>> {
    deckcraft_engine::cmd::smartart::node_texts(tree.shapes(&document(session, doc)?.doc)?, group)
}

/// Whether slide `slide` of the open presentation `doc` still has the notes `base`.
fn notes_are(session: &Session, doc: u64, slide: SlideId, base: &TextBody) -> bool {
    slide_of(session, doc, slide).is_some_and(|s| s.notes == *base)
}

/// What the panes have been typed into and what has not been entered yet.
#[derive(Default)]
pub(crate) struct Drafts {
    /// Replies typed under a comment and not posted.
    replies: HashMap<ReplyOwner, String>,
    /// Alt text and Outline text typed and not applied: (the text the field started from, what was
    /// typed).
    texts: HashMap<TextOwner, (String, String)>,
    /// Text Pane lines typed and not entered, by (presentation, shapes, graphic): (the text of every
    /// node the lines started from, the lines).
    smartart: HashMap<(u64, Tree, ShapeId), (Vec<String>, Vec<String>)>,
    /// Speaker notes typed in the Notes pane or on the Notes Page, by (presentation, slide): (the
    /// notes the slide had, the draft).
    notes: HashMap<(u64, SlideId), (TextBody, notes::Draft)>,
}

impl Drafts {
    /// The reply typed under `owner`; empty when there is none.
    pub(crate) fn reply(&self, owner: &ReplyOwner) -> String {
        self.replies.get(owner).cloned().unwrap_or_default()
    }

    /// Keeps `text` as that reply. An empty one is no draft.
    pub(crate) fn set_reply(&mut self, owner: ReplyOwner, text: String) {
        if text.is_empty() {
            self.replies.remove(&owner);
        } else {
            self.replies.insert(owner, text);
        }
    }

    /// The text typed into the field and not entered, as (the text it started from, what was typed).
    pub(crate) fn text(&self, owner: &TextOwner) -> Option<(String, String)> {
        self.texts.get(owner).cloned()
    }

    /// Keeps `typed` for the field while it differs from `current`, the text the field's shape has.
    pub(crate) fn set_text(&mut self, owner: TextOwner, current: &str, typed: String) {
        if typed == current {
            self.texts.remove(&owner);
        } else {
            self.texts.insert(owner, (current.to_string(), typed));
        }
    }

    /// The field's draft is not needed any more.
    pub(crate) fn drop_text(&mut self, owner: &TextOwner) {
        self.texts.remove(owner);
    }

    /// The lines the Text Pane shows for `group`, whose nodes have the texts `current`: the ones
    /// typed and not entered while the graphic's text is still what they started from, else `current`.
    pub(crate) fn smartart_lines(&self, doc: u64, tree: Tree, group: ShapeId, current: &[String]) -> Vec<String> {
        match self.smartart.get(&(doc, tree, group)) {
            Some((base, typed)) if base == current => typed.clone(),
            _ => current.to_vec(),
        }
    }

    /// Keeps the Text Pane's `lines` for `group` while they differ from `current`, its nodes' texts.
    pub(crate) fn set_smartart(&mut self, doc: u64, tree: Tree, group: ShapeId, current: &[String], lines: Vec<String>) {
        if lines == current {
            self.smartart.remove(&(doc, tree, group));
        } else {
            self.smartart.insert((doc, tree, group), (current.to_vec(), lines));
        }
    }

    /// The Text Pane's lines for `group` are not needed any more.
    pub(crate) fn drop_smartart(&mut self, doc: u64, tree: Tree, group: ShapeId) {
        self.smartart.remove(&(doc, tree, group));
    }

    /// The notes draft of `slide` made from `current`, the notes it has now. A draft made from other
    /// notes (an undo or another edit changed them) is none.
    pub(crate) fn notes(&self, doc: u64, slide: SlideId, current: &TextBody) -> Option<notes::Draft> {
        self.notes.get(&(doc, slide)).filter(|(base, _)| base == current).map(|(_, draft)| draft.clone())
    }

    /// Keeps `draft`, made from the notes `base` of `slide`.
    pub(crate) fn keep_notes(&mut self, doc: u64, slide: SlideId, base: TextBody, draft: notes::Draft) {
        self.notes.insert((doc, slide), (base, draft));
    }

    /// `slide`'s notes draft is not needed any more.
    pub(crate) fn drop_notes(&mut self, doc: u64, slide: SlideId) {
        self.notes.remove(&(doc, slide));
    }

    /// The text in the notes field of `slide`'s draft (what a person sees there while typing).
    #[cfg(test)]
    pub(crate) fn notes_text(&self, doc: u64, slide: SlideId) -> Option<String> {
        self.notes.get(&(doc, slide)).map(|(_, draft)| draft.text.clone())
    }

    /// The notes edited and not entered in presentation `doc`, except those of the slide `typing`
    /// whose field is being typed in, as (slide, notes). A draft made from notes the slide no longer
    /// has is not among them ([`Self::forget_stale`] drops it).
    pub(crate) fn notes_to_enter(&self, session: &Session, doc: u64, typing: Option<SlideId>) -> Vec<(SlideId, TextBody)> {
        self.notes
            .iter()
            .filter(|&(&(d, s), (base, draft))| d == doc && draft.edited && Some(s) != typing && notes_are(session, d, s, base))
            .map(|(&(_, s), (_, draft))| (s, draft.body.clone()))
            .collect()
    }

    /// The text typed into fields of presentation `doc` that is not in their shapes, as (field, text):
    /// the drafts whose shape still has the text they started from.
    pub(crate) fn texts_to_enter(&self, session: &Session, doc: u64) -> Vec<(TextOwner, String)> {
        self.texts
            .iter()
            .filter(|&(o, (base, typed))| o.doc == doc && typed != base && o.current(session).is_some_and(|now| now == *base))
            .map(|(o, (_, typed))| (o.clone(), typed.clone()))
            .collect()
    }

    /// The Text Pane lines of presentation `doc` that differ from their graphic's nodes, as (shapes,
    /// graphic, (node, line) of each node that differs).
    pub(crate) fn smartart_to_enter(&self, session: &Session, doc: u64) -> Vec<(Tree, ShapeId, Vec<(usize, String)>)> {
        self.smartart
            .iter()
            .filter(|&(&(d, tree, group), (base, lines))| {
                d == doc && lines != base && smartart_texts(session, d, tree, group).is_some_and(|now| now == *base)
            })
            .map(|(&(_, tree, group), (base, lines))| {
                let changed = lines.iter().enumerate().filter(|&(i, line)| base.get(i) != Some(line)).map(|(i, line)| (i, line.clone())).collect();
                (tree, group, changed)
            })
            .collect()
    }

    /// Drops the drafts of a presentation that has closed, and those whose comment, shape, graphic or
    /// notes are gone or have changed since.
    pub(crate) fn forget_stale(&mut self, session: &Session) {
        self.replies.retain(|owner, _| owner.exists(session));
        self.texts.retain(|owner, (base, _)| owner.current(session).is_some_and(|now| now == *base));
        self.smartart.retain(|&(doc, tree, group), (base, _)| smartart_texts(session, doc, tree, group).is_some_and(|now| now == *base));
        self.notes.retain(|&(doc, slide), (base, _)| notes_are(session, doc, slide, base));
    }

    /// Whether anything typed and not entered waits in an open presentation: a reply that is not
    /// blank, text that differs from the shape's, Text Pane lines or notes that were edited.
    pub(crate) fn pending(&self, session: &Session) -> bool {
        self.unposted(session, None) || self.unentered(session, None)
    }

    /// [`Self::pending`] for the open presentation `doc` alone.
    pub(crate) fn pending_in(&self, session: &Session, doc: u64) -> bool {
        self.unposted(session, Some(doc)) || self.unentered(session, Some(doc))
    }

    /// Whether a reply that is not blank waits under a comment that is still there, in the open
    /// presentation `only` (any when none). Only posting enters a reply: a Save leaves it, so it
    /// stays protected until it is posted or thrown away.
    pub(crate) fn unposted(&self, session: &Session, only: Option<u64>) -> bool {
        self.replies.iter().any(|(owner, text)| only.is_none_or(|doc| doc == owner.doc) && !text.trim().is_empty() && owner.exists(session))
    }

    /// Whether text waits to go into its shape, Text Pane lines into their graphic or notes into
    /// their slide, in the open presentation `only` (any when none): what a Save enters
    /// ([`SlideApp::enter_drafts`]) before it writes the file.
    pub(crate) fn unentered(&self, session: &Session, only: Option<u64>) -> bool {
        let wanted = |doc: u64| only.is_none_or(|only| only == doc);
        self.texts.iter().any(|(owner, (base, typed))| wanted(owner.doc) && typed != base && owner.current(session).is_some_and(|now| now == *base))
            || self.smartart.iter().any(|(&(doc, tree, group), (base, lines))| {
                wanted(doc) && lines != base && smartart_texts(session, doc, tree, group).is_some_and(|now| now == *base)
            })
            || self.notes.iter().any(|(&(doc, slide), (base, draft))| wanted(doc) && draft.edited && notes_are(session, doc, slide, base))
    }
}

impl SlideApp {
    /// Runs command `id` with the editing pointed at `tree` of the active presentation, wherever it is
    /// now: a command that names its shape still looks for it on the slide, master or layout the
    /// selection is on. The selection goes back afterwards, so a draft entered from elsewhere (the
    /// Outline with another slide current, a Save) neither moves the person nor needs the shape to
    /// be selected.
    pub(crate) fn run_in(&mut self, tree: Tree, id: &str, params: Value) -> Result<Value, String> {
        let Some(st) = self.session.active() else { return Err("no presentation is open".into()) };
        if Tree::of(st) == Some(tree) {
            return self.run(id, params);
        }
        let before = st.selection.clone();
        let mut pointed = before.clone();
        if !tree.point(&st.doc, &mut pointed) {
            return Err("what the text was typed for is gone".into());
        }
        self.session.select(|_, sel| *sel = pointed).map_err(|e| e.to_string())?;
        let result = self.run(id, params);
        let _ = self.session.select(|_, sel| *sel = before);
        result
    }

    /// Puts `typed`, the text of `owner`'s field, into the shape it was typed for, wherever the editing
    /// is now. The draft goes once the presentation has taken the text; when the presentation refuses it
    /// (the status line says why) the draft stays, and the field goes on counting as unsaved work.
    /// Whether the text went in.
    pub(crate) fn enter_text(&mut self, owner: &TextOwner, typed: &str) -> bool {
        if self.session.active().map(|d| d.uid) != Some(owner.doc) {
            return false;
        }
        let (id, params) = owner.enter_command(typed);
        let entered = self.run_in(owner.tree, id, params).is_ok();
        if entered {
            self.drafts.drop_text(owner);
        }
        entered
    }

    /// Puts the Text Pane lines typed for SmartArt graphic `group` of `tree`, as (node, line), into
    /// it, wherever the editing is now. The draft goes once every line has gone in; when the
    /// presentation refuses one (the status line says why) the draft stays, and the lines go on
    /// counting as unsaved work. Whether every line went in.
    pub(crate) fn enter_lines(&mut self, doc: u64, tree: Tree, group: ShapeId, lines: &[(usize, String)]) -> bool {
        if self.session.active().map(|d| d.uid) != Some(doc) {
            return false;
        }
        let mut entered = true;
        for (index, text) in lines {
            entered &= self.run_in(tree, "smartArt.setNodes", json!({"id": group.0, "index": index, "text": text})).is_ok();
        }
        if entered {
            self.drafts.drop_smartart(doc, tree, group);
        }
        entered
    }

    /// Enters into the active presentation everything typed for it and not entered yet that can be
    /// entered, whether or not the panes that showed it are drawn now: what a Save would otherwise
    /// leave out. A reply stays, as only posting enters one.
    pub(crate) fn enter_drafts(&mut self) {
        let Some(doc) = self.session.active().map(|d| d.uid) else { return };
        for (owner, typed) in self.drafts.texts_to_enter(&self.session, doc) {
            self.enter_text(&owner, &typed);
        }
        for (tree, group, lines) in self.drafts.smartart_to_enter(&self.session, doc) {
            self.enter_lines(doc, tree, group, &lines);
        }
        notes::enter_edits(self, doc, None);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use deckcraft_engine::Session;
    use deckcraft_model::{ShapeId, SlideId, TextBody};
    use serde_json::json;

    use super::{Drafts, ReplyOwner, TextOwner, Tree};
    use crate::notes;

    /// The first presentation's uid and the open slides' ids.
    fn ids(s: &Session) -> (u64, Vec<SlideId>) {
        let st = s.active().unwrap();
        (st.uid, st.doc.slides.iter().map(|sl| sl.id).collect())
    }

    /// The owner of comment `index` of slide `slide` (by place in the deck, now).
    fn owner(s: &Session, slide: usize, index: usize) -> ReplyOwner {
        let st = s.active().unwrap();
        let sl = &st.doc.slides[slide];
        ReplyOwner::of(st.uid, sl.id, &sl.comments[index])
    }

    /// A presentation with `n` comments on its first slide.
    fn commented(n: usize) -> Session {
        let mut s = Session::with_new();
        for i in 0..n {
            s.execute("comment.add", &json!({"text": format!("Comment {i}")})).unwrap();
        }
        s
    }

    #[test]
    fn a_reply_counts_unless_it_is_blank_or_its_comment_is_gone() {
        let mut s = commented(1);
        let who = owner(&s, 0, 0);
        let mut d = Drafts::default();
        d.set_reply(who.clone(), "   ".into());
        assert_eq!(d.reply(&who), "   ", "the field keeps what was typed");
        assert!(!d.pending(&s), "spaces are not a reply");
        d.set_reply(who.clone(), "Will do".into());
        assert!(d.pending(&s));
        d.set_reply(who.clone(), String::new());
        assert!(!d.pending(&s) && d.replies.is_empty(), "an emptied field is no draft");
        d.set_reply(who, "Will do".into());
        s.execute("comment.delete", &json!({"index": 0})).unwrap();
        assert!(!d.pending(&s), "the comment it answers is gone");
        d.forget_stale(&s);
        assert!(d.replies.is_empty());
    }

    /// A reply stays with its comment when the slide is moved, and when the comments before it go.
    #[test]
    fn a_reply_stays_with_its_comment_when_slides_move_and_comments_go() {
        let mut s = commented(3);
        s.execute("slide.new", &json!({})).unwrap();
        s.execute("slide.go", &json!({"index": 1})).unwrap();
        s.execute("comment.add", &json!({"text": "On the second slide"})).unwrap();
        let (mut d, third, other) = (Drafts::default(), owner(&s, 0, 2), owner(&s, 1, 0));
        d.set_reply(third.clone(), "For the third".into());

        // The slide moves behind the other one: the reply is still its comment's, not the one now at
        // the same place in the deck.
        s.execute("slide.move", &json!({"from": 0, "to": 1})).unwrap();
        d.forget_stale(&s);
        assert_eq!(d.reply(&third), "For the third");
        assert_eq!(d.reply(&other), "", "the comment now first has no reply typed");
        assert_eq!(d.reply(&owner(&s, 1, 2)), "For the third", "the comment is where the slide went");
        assert!(d.pending(&s));

        // Deleting the comments before it moves it up the list, not the reply to another comment.
        s.execute("slide.go", &json!({"index": 1})).unwrap();
        s.execute("comment.delete", &json!({"index": 0})).unwrap();
        d.forget_stale(&s);
        assert_eq!(d.reply(&owner(&s, 1, 1)), "For the third", "now the second comment");
        assert_eq!(d.reply(&owner(&s, 1, 0)), "");
        assert!(d.pending(&s));

        // Deleting the slide takes its replies; the other slide's would stay.
        d.set_reply(other.clone(), "For the other".into());
        s.execute("slide.delete", &json!({"index": 1})).unwrap();
        assert!(d.pending(&s), "the other slide's reply waits");
        d.forget_stale(&s);
        assert_eq!(d.replies.len(), 1);
        assert_eq!(d.reply(&other), "For the other");
    }

    /// Comments that say the same are told apart by the number each carries, not by their order or
    /// their words: a reply stays with its own comment when the one before it goes, and when that
    /// deletion is undone.
    #[test]
    fn identical_comments_have_their_own_replies() {
        let mut s = Session::with_new();
        for _ in 0..2 {
            s.execute("comment.add", &json!({"text": "Same", "x": 1.0, "y": 1.0})).unwrap();
        }
        // Dates are to the second; make sure the two are identical.
        let mut doc = (*s.active().unwrap().doc).clone();
        let date = doc.slides[0].comments[0].date.clone();
        std::sync::Arc::make_mut(&mut doc.slides[0]).comments[1].date = date;
        s.active_mut().unwrap().doc = std::sync::Arc::new(doc);
        let comments = &s.active().unwrap().doc.slides[0].comments;
        assert_eq!(comments[0], comments[1], "nothing they say tells them apart");
        let (first, second) = (owner(&s, 0, 0), owner(&s, 0, 1));
        assert_ne!(first, second);
        let mut d = Drafts::default();
        d.set_reply(second.clone(), "For the second".into());
        assert_eq!(d.reply(&first), "");
        assert_eq!(d.reply(&second), "For the second");
        assert!(d.pending(&s));

        // The first goes: the second is the only comment left, and keeps its reply.
        s.execute("comment.delete", &json!({"index": 0})).unwrap();
        d.forget_stale(&s);
        assert_eq!(d.reply(&owner(&s, 0, 0)), "For the second", "the same comment, now first in the list");
        assert_eq!(d.reply(&first), "");
        assert!(d.pending(&s));

        // Undoing brings the first back as the comment it was, with no reply; the second keeps its own.
        s.execute("edit.undo", &json!({})).unwrap();
        d.forget_stale(&s);
        assert_eq!((owner(&s, 0, 0), owner(&s, 0, 1)), (first, second.clone()), "an undo gives the comments back as they were");
        assert_eq!(d.reply(&second), "For the second");
        assert!(d.pending(&s));
    }

    /// A reply waits to be posted and no Save enters it; typed text waits to be entered and a Save
    /// does. Both are counted for their own presentation only.
    #[test]
    fn a_reply_is_unposted_and_typed_text_is_unentered_each_in_its_own_presentation() {
        let mut s = commented(1);
        let (uid, _) = ids(&s);
        let mut d = Drafts::default();
        d.set_reply(owner(&s, 0, 0), "Will do".into());
        assert!(d.unposted(&s, None) && !d.unentered(&s, None), "a Save can't enter a reply");
        assert!(d.unposted(&s, Some(uid)) && !d.unposted(&s, Some(uid + 1)), "it waits in its own presentation only");
        assert!(d.pending_in(&s, uid) && !d.pending_in(&s, uid + 1));

        s.execute("insert.textBox", &json!({"text": "Hello"})).unwrap();
        let (field, _) = alt_field(&s);
        d.set_text(field, "", "A greeting".into());
        assert!(d.unentered(&s, None) && d.unentered(&s, Some(uid)) && !d.unentered(&s, Some(uid + 1)));
        assert!(d.pending_in(&s, uid));
    }

    #[test]
    fn closing_a_presentation_drops_its_drafts() {
        let mut s = commented(1);
        let who = owner(&s, 0, 0);
        let mut d = Drafts::default();
        d.set_reply(who, "Will do".into());
        assert!(d.pending(&s));
        s.close_document(0);
        assert!(!d.pending(&s), "nothing waits in a presentation that is gone");
        d.forget_stale(&s);
        assert!(d.replies.is_empty());
    }

    /// The alt text of the last shape on the first slide, and the field it is typed into.
    fn alt_field(s: &Session) -> (TextOwner, ShapeId) {
        let st = s.active().unwrap();
        let shape = st.doc.slides[0].shapes.last().unwrap().id;
        (TextOwner::alt_text(st.uid, Tree::of(st).unwrap(), shape), shape)
    }

    #[test]
    fn alt_text_counts_while_it_differs_from_the_shapes_and_starts_from_it() {
        let mut s = Session::with_new();
        s.execute("insert.textBox", &json!({"text": "Hello"})).unwrap();
        let (field, shape) = alt_field(&s);
        let mut d = Drafts::default();
        d.set_text(field.clone(), "", "A greeting".into());
        assert_eq!(d.text(&field), Some((String::new(), "A greeting".into())));
        assert!(d.pending(&s));
        d.set_text(field.clone(), "", String::new());
        assert!(!d.pending(&s) && d.texts.is_empty(), "the shape's own alt text typed back is no draft");
        // Another edit gave the shape alt text under the draft: the pane shows that, not the draft.
        d.set_text(field.clone(), "", "A greeting".into());
        s.execute("shape.altText", &json!({"text": "Logo", "ids": [shape.0]})).unwrap();
        assert!(!d.pending(&s));
        d.forget_stale(&s);
        assert!(d.texts.is_empty());
    }

    /// Alt text typed for a shape of the slide master or of a layout is kept like any other (it is
    /// not on a slide).
    #[test]
    fn alt_text_of_a_master_or_layout_shape_is_kept() {
        for (target, master) in [(json!({"master": 0}), true), (json!({"master": 0, "layout": 1}), false)] {
            let mut s = Session::with_new();
            s.execute("view.slideMaster", &target).unwrap();
            let st = s.active().unwrap();
            let tree = Tree::of(st).unwrap();
            assert_eq!(matches!(tree, Tree::Master(_)), master, "{tree:?}");
            let shape = st.shapes().first().unwrap().id;
            let field = TextOwner::alt_text(st.uid, tree, shape);
            let mut d = Drafts::default();
            d.set_text(field.clone(), "", "The master's title".into());
            d.forget_stale(&s);
            assert_eq!(d.text(&field).map(|t| t.1).as_deref(), Some("The master's title"), "a pass doesn't forget it");
            assert!(d.pending(&s));
        }
    }

    /// The slide's title and its other text, as the Outline edits them.
    #[test]
    fn outline_text_counts_while_it_differs_from_the_shapes_and_starts_from_it() {
        let mut s = Session::with_new();
        let st = s.active().unwrap();
        let (uid, slide) = (st.uid, st.doc.slides[0].id);
        let title = super::outline_title(&st.doc.slides[0].shapes).unwrap();
        let mut d = Drafts::default();
        let field = TextOwner::outline_title(uid, slide, title.0);
        d.set_text(field.clone(), &title.1, "A new title".into());
        assert!(d.pending(&s));
        // The slide moves: the draft is still the title's.
        s.execute("slide.new", &json!({})).unwrap();
        s.execute("slide.move", &json!({"from": 0, "to": 1})).unwrap();
        d.forget_stale(&s);
        assert!(d.pending(&s), "it is the same slide's title");
        // Someone else gave the title text: the draft was made from text that is gone.
        s.execute("slide.go", &json!({"index": 1})).unwrap();
        s.execute("text.set", &json!({"id": title.0.0, "text": "Typed elsewhere"})).unwrap();
        assert!(!d.pending(&s));
        d.forget_stale(&s);
        assert!(d.texts.is_empty());
    }

    /// A presentation with a SmartArt graphic of two nodes selected: its uid, tree, group and texts.
    fn graphic() -> (Session, u64, Tree, ShapeId, Vec<String>) {
        let mut s = Session::with_new();
        s.execute("insert.smartArt", &json!({"kind": "list", "items": ["First", "Second"]})).unwrap();
        let st = s.active().unwrap();
        let (group, diagram) = deckcraft_engine::cmd::smartart::selected(st).unwrap();
        let texts = diagram.nodes.iter().map(|n| n.text.clone()).collect();
        let (uid, tree) = (st.uid, Tree::of(st).unwrap());
        (s, uid, tree, group, texts)
    }

    #[test]
    fn text_pane_lines_count_while_they_differ_from_the_graphics_and_start_from_it() {
        let (mut s, uid, tree, group, texts) = graphic();
        assert_eq!(texts, ["First", "Second"]);
        let mut d = Drafts::default();
        assert_eq!(d.smartart_lines(uid, tree, group, &texts), texts, "nothing typed: the graphic's text");
        let typed = vec!["First, edited".to_string(), "Second".to_string()];
        d.set_smartart(uid, tree, group, &texts, typed.clone());
        assert_eq!(d.smartart_lines(uid, tree, group, &texts), typed);
        assert!(d.pending(&s));
        d.set_smartart(uid, tree, group, &texts, texts.clone());
        assert!(!d.pending(&s) && d.smartart.is_empty(), "typed back to the graphic's text: no draft");
        // The graphic's text changed under the draft: it is gone, and the lines are the new text.
        d.set_smartart(uid, tree, group, &texts, typed);
        s.execute("smartArt.setNodes", &json!({"id": group.0, "index": 1, "text": "Changed elsewhere"})).unwrap();
        assert!(!d.pending(&s));
        let now = vec!["First".to_string(), "Changed elsewhere".to_string()];
        assert_eq!(d.smartart_lines(uid, tree, group, &now), now);
        d.forget_stale(&s);
        assert!(d.smartart.is_empty());
    }

    #[test]
    fn a_text_pane_draft_goes_with_its_graphic() {
        let (mut s, uid, tree, group, texts) = graphic();
        let mut d = Drafts::default();
        d.set_smartart(uid, tree, group, &texts, vec!["Typed".into(), "Second".into()]);
        assert!(d.pending(&s));
        s.execute("edit.select", &json!({"ids": [group.0]})).unwrap();
        s.execute("edit.delete", &json!({})).unwrap();
        assert!(!d.pending(&s), "the graphic is gone");
        d.forget_stale(&s);
        assert!(d.smartart.is_empty());
    }

    /// The first slide's notes.
    fn notes_of(s: &Session, slide: usize) -> TextBody {
        s.active().unwrap().doc.slides[slide].notes.clone()
    }

    #[test]
    fn a_notes_draft_counts_while_it_is_edited_from_the_notes_the_slide_has() {
        let mut s = Session::with_new();
        let (uid, slides) = ids(&s);
        let base = notes_of(&s, 0);
        let mut draft = notes::Draft::new(&base);
        let mut d = Drafts::default();
        d.keep_notes(uid, slides[0], base.clone(), draft.clone());
        assert!(!d.pending(&s), "a draft nobody edited is nothing to lose");
        assert_eq!(d.notes(uid, slides[0], &base), Some(draft.clone()));
        draft.edited = true;
        d.keep_notes(uid, slides[0], base.clone(), draft);
        assert!(d.pending(&s));
        // Another edit changed the slide's notes under the draft: it was made from notes that are gone.
        let mut other = base.clone();
        deckcraft_model::edit::replace_text(&mut other, "Typed elsewhere");
        s.execute("slide.notes", &json!({"body": other, "index": 0})).unwrap();
        assert_ne!(notes_of(&s, 0), base);
        assert!(!d.pending(&s));
        assert!(d.notes(uid, slides[0], &notes_of(&s, 0)).is_none());
        d.forget_stale(&s);
        assert!(d.notes.is_empty());
        d.keep_notes(uid, slides[0], notes_of(&s, 0), notes::Draft::new(&notes_of(&s, 0)));
        d.drop_notes(uid, slides[0]);
        assert!(d.notes.is_empty());
    }

    /// Edited notes are entered for the slide they were typed for, wherever it is by then.
    #[test]
    fn edited_notes_are_entered_for_their_slide_not_the_place_it_had() {
        let mut s = Session::with_new();
        s.execute("slide.new", &json!({})).unwrap();
        let (uid, slides) = ids(&s);
        let base = notes_of(&s, 0);
        let mut typed = base.clone();
        deckcraft_model::edit::replace_text(&mut typed, "For the first slide");
        let mut draft = notes::Draft::new(&base);
        draft.body = typed.clone();
        draft.edited = true;
        let mut d = Drafts::default();
        d.keep_notes(uid, slides[0], base.clone(), draft);
        s.execute("slide.move", &json!({"from": 0, "to": 1})).unwrap();
        assert!(d.pending(&s));
        assert!(d.notes_to_enter(&s, uid, Some(slides[0])).is_empty(), "its field is being typed in");
        assert_eq!(d.notes_to_enter(&s, uid, None), vec![(slides[0], typed)]);
        assert_eq!(super::slide_index(&s, uid, slides[0]), Some(1), "the slide the notes belong to moved");
        assert!(d.pending(&s), "they wait until the slide has them");
        d.drop_notes(uid, slides[0]);
        assert!(d.notes.is_empty() && !d.pending(&s));
    }
}
