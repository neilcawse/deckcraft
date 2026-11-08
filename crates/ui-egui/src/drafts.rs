//! Text typed into a pane and not in the presentation yet, which the app keeps itself instead of in
//! egui's temporary data. The pane that shows it can go away (hidden, another view or tab, a slide
//! show) while the text stays and comes back with the pane, so it counts as unsaved work
//! ([`SlideApp::has_unsaved_work`](crate::SlideApp::has_unsaved_work)) whether or not anything draws
//! it. A draft belongs to one presentation, by its uid: another presentation never shows it, and it
//! goes when the presentation closes or what it was typed for is gone.

use std::collections::HashMap;

use deckcraft_engine::Session;
use deckcraft_model::{ShapeId, Slide, TextBody};

use crate::notes;

/// Slide `slide` of the open presentation `doc`.
pub(crate) fn slide_of(session: &Session, doc: u64, slide: usize) -> Option<&Slide> {
    session.documents().iter().find(|d| d.uid == doc)?.doc.slides.get(slide).map(|s| &**s)
}

/// The alt text `shape` has now, on slide `slide` of the open presentation `doc`.
fn alt_of(session: &Session, doc: u64, slide: usize, shape: ShapeId) -> Option<&str> {
    slide_of(session, doc, slide)?.shape(shape).map(|s| s.descr.as_str())
}

/// Whether comment `index` is still on slide `slide` of the open presentation `doc`.
fn comment_exists(session: &Session, doc: u64, slide: usize, index: usize) -> bool {
    slide_of(session, doc, slide).is_some_and(|s| index < s.comments.len())
}

/// Whether slide `slide` of the open presentation `doc` still has the notes `base`.
fn notes_are(session: &Session, doc: u64, slide: usize, base: &TextBody) -> bool {
    slide_of(session, doc, slide).is_some_and(|s| s.notes == *base)
}

/// What the panes have been typed into and what has not been entered yet.
#[derive(Default)]
pub(crate) struct Drafts {
    /// Replies typed under a comment and not posted: (presentation, slide, comment) → text.
    replies: HashMap<(u64, usize, usize), String>,
    /// Alt text typed for a shape and not applied: (presentation, slide, shape) → (the alt text the
    /// shape had, what was typed).
    alt_texts: HashMap<(u64, usize, ShapeId), (String, String)>,
    /// Notes Page text kept while it is typed in or edited: (presentation, slide) → (the notes it
    /// was made from, the draft).
    notes_pages: HashMap<(u64, usize), (TextBody, notes::Draft)>,
}

impl Drafts {
    /// The reply typed under comment `index` of `slide`; empty when there is none.
    pub(crate) fn reply(&self, doc: u64, slide: usize, index: usize) -> String {
        self.replies.get(&(doc, slide, index)).cloned().unwrap_or_default()
    }

    /// Keeps `text` as that reply. An empty one is no draft.
    pub(crate) fn set_reply(&mut self, doc: u64, slide: usize, index: usize, text: String) {
        let key = (doc, slide, index);
        if text.is_empty() {
            self.replies.remove(&key);
        } else {
            self.replies.insert(key, text);
        }
    }

    /// Comment `index` of `slide` was deleted: its reply goes with it, and the replies under the
    /// comments after it move up with them.
    pub(crate) fn comment_deleted(&mut self, doc: u64, slide: usize, index: usize) {
        self.replies.remove(&(doc, slide, index));
        let mut later: Vec<usize> = self.replies.keys().filter(|&&(d, s, i)| d == doc && s == slide && i > index).map(|&(_, _, i)| i).collect();
        later.sort_unstable();
        for i in later {
            if let Some(text) = self.replies.remove(&(doc, slide, i)) {
                self.replies.insert((doc, slide, i - 1), text);
            }
        }
    }

    /// The alt text typed for `shape` and not applied, as (the alt text it started from, what was typed).
    pub(crate) fn alt_text(&self, doc: u64, slide: usize, shape: ShapeId) -> Option<(String, String)> {
        self.alt_texts.get(&(doc, slide, shape)).cloned()
    }

    /// Keeps `typed` for `shape` while it differs from `current`, the alt text the shape has.
    pub(crate) fn set_alt_text(&mut self, doc: u64, slide: usize, shape: ShapeId, current: &str, typed: String) {
        let key = (doc, slide, shape);
        if typed == current {
            self.alt_texts.remove(&key);
        } else {
            self.alt_texts.insert(key, (current.to_string(), typed));
        }
    }

    /// The Notes Page draft of `slide` made from `notes`, the notes it has now. A draft made from
    /// other notes (an undo changed them) is none.
    pub(crate) fn notes_page(&self, doc: u64, slide: usize, notes: &TextBody) -> Option<notes::Draft> {
        self.notes_pages.get(&(doc, slide)).filter(|(base, _)| base == notes).map(|(_, draft)| draft.clone())
    }

    /// Keeps `draft`, made from the notes `base`.
    pub(crate) fn keep_notes_page(&mut self, doc: u64, slide: usize, base: TextBody, draft: notes::Draft) {
        self.notes_pages.insert((doc, slide), (base, draft));
    }

    /// `slide`'s Notes Page draft is not needed any more.
    pub(crate) fn drop_notes_page(&mut self, doc: u64, slide: usize) {
        self.notes_pages.remove(&(doc, slide));
    }

    /// Drops the drafts of a presentation that has closed, and those whose comment, shape or notes
    /// are gone or have changed since.
    pub(crate) fn forget_stale(&mut self, session: &Session) {
        self.replies.retain(|&(doc, slide, index), _| comment_exists(session, doc, slide, index));
        self.alt_texts.retain(|&(doc, slide, shape), (base, _)| alt_of(session, doc, slide, shape) == Some(base.as_str()));
        self.notes_pages.retain(|&(doc, slide), (base, _)| notes_are(session, doc, slide, base));
    }

    /// Whether anything typed and not entered waits in an open presentation: a reply that is not
    /// blank, alt text that differs from the shape's, notes that were edited.
    pub(crate) fn pending(&self, session: &Session) -> bool {
        self.replies.iter().any(|(&(doc, slide, index), text)| !text.trim().is_empty() && comment_exists(session, doc, slide, index))
            || self
                .alt_texts
                .iter()
                .any(|(&(doc, slide, shape), (base, typed))| alt_of(session, doc, slide, shape).is_some_and(|now| now == base && now != typed))
            || self.notes_pages.iter().any(|(&(doc, slide), (base, draft))| draft.edited && notes_are(session, doc, slide, base))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use deckcraft_engine::Session;
    use deckcraft_model::TextBody;
    use serde_json::json;

    use super::Drafts;
    use crate::notes;

    /// A presentation with `n` comments on its first slide, and its uid.
    fn commented(n: usize) -> (Session, u64) {
        let mut s = Session::with_new();
        for i in 0..n {
            s.execute("comment.add", &json!({"text": format!("Comment {i}")})).unwrap();
        }
        let uid = s.active().unwrap().uid;
        (s, uid)
    }

    #[test]
    fn a_reply_counts_unless_it_is_blank_or_its_comment_is_gone() {
        let (mut s, uid) = commented(1);
        let mut d = Drafts::default();
        d.set_reply(uid, 0, 0, "   ".into());
        assert_eq!(d.reply(uid, 0, 0), "   ", "the field keeps what was typed");
        assert!(!d.pending(&s), "spaces are not a reply");
        d.set_reply(uid, 0, 0, "Will do".into());
        assert!(d.pending(&s));
        d.set_reply(uid, 0, 0, String::new());
        assert!(!d.pending(&s) && d.replies.is_empty(), "an emptied field is no draft");
        d.set_reply(uid, 0, 0, "Will do".into());
        s.execute("comment.delete", &json!({"index": 0})).unwrap();
        assert!(!d.pending(&s), "the comment it answers is gone");
        d.forget_stale(&s);
        assert!(d.replies.is_empty());
    }

    #[test]
    fn a_deleted_comments_reply_goes_and_the_ones_after_it_move_up() {
        let mut d = Drafts::default();
        for (i, text) in ["zero", "one", "two", "three"].into_iter().enumerate() {
            d.set_reply(7, 0, i, text.into());
        }
        d.set_reply(7, 1, 2, "other slide".into());
        d.set_reply(8, 0, 2, "other presentation".into());
        d.comment_deleted(7, 0, 1);
        let kept: Vec<String> = (0..4).map(|i| d.reply(7, 0, i)).collect();
        assert_eq!(kept, ["zero", "two", "three", ""]);
        assert_eq!(d.reply(7, 1, 2), "other slide");
        assert_eq!(d.reply(8, 0, 2), "other presentation");
    }

    #[test]
    fn closing_a_presentation_drops_its_drafts() {
        let (mut s, uid) = commented(1);
        let mut d = Drafts::default();
        d.set_reply(uid, 0, 0, "Will do".into());
        assert!(d.pending(&s));
        s.close_document(0);
        assert!(!d.pending(&s), "nothing waits in a presentation that is gone");
        d.forget_stale(&s);
        assert!(d.replies.is_empty());
    }

    #[test]
    fn alt_text_counts_while_it_differs_from_the_shapes_and_starts_from_it() {
        let mut s = Session::with_new();
        s.execute("insert.textBox", &json!({"text": "Hello"})).unwrap();
        let st = s.active().unwrap();
        let (uid, shape) = (st.uid, st.doc.slides[0].shapes.last().unwrap().id);
        let mut d = Drafts::default();
        d.set_alt_text(uid, 0, shape, "", "A greeting".into());
        assert_eq!(d.alt_text(uid, 0, shape), Some((String::new(), "A greeting".into())));
        assert!(d.pending(&s));
        d.set_alt_text(uid, 0, shape, "", String::new());
        assert!(!d.pending(&s) && d.alt_texts.is_empty(), "the shape's own alt text typed back is no draft");
        // Another edit gave the shape alt text under the draft: the pane shows that, not the draft.
        d.set_alt_text(uid, 0, shape, "", "A greeting".into());
        s.execute("shape.altText", &json!({"text": "Logo", "ids": [shape.0]})).unwrap();
        assert!(!d.pending(&s));
        d.forget_stale(&s);
        assert!(d.alt_texts.is_empty());
    }

    /// The first slide's notes.
    fn notes_of(s: &Session) -> TextBody {
        s.active().unwrap().doc.slides[0].notes.clone()
    }

    #[test]
    fn a_notes_page_draft_counts_while_it_is_edited_from_the_notes_the_slide_has() {
        let mut s = Session::with_new();
        let uid = s.active().unwrap().uid;
        let base = notes_of(&s);
        let mut draft = notes::Draft::new(&base);
        let mut d = Drafts::default();
        d.keep_notes_page(uid, 0, base.clone(), draft.clone());
        assert!(!d.pending(&s), "a draft nobody edited is nothing to lose");
        assert_eq!(d.notes_page(uid, 0, &base), Some(draft.clone()));
        draft.edited = true;
        d.keep_notes_page(uid, 0, base.clone(), draft);
        assert!(d.pending(&s));
        // Another edit changed the slide's notes under the draft: it was made from notes that are gone.
        let mut other = base.clone();
        deckcraft_model::edit::replace_text(&mut other, "Typed elsewhere");
        s.execute("slide.notes", &json!({"body": other, "index": 0})).unwrap();
        assert_ne!(notes_of(&s), base);
        assert!(!d.pending(&s));
        assert!(d.notes_page(uid, 0, &notes_of(&s)).is_none());
        d.forget_stale(&s);
        assert!(d.notes_pages.is_empty());
        d.keep_notes_page(uid, 0, notes_of(&s), notes::Draft::new(&notes_of(&s)));
        d.drop_notes_page(uid, 0);
        assert!(d.notes_pages.is_empty());
    }
}
