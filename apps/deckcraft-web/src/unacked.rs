//! Saves handed to the host page (`?host=parent&ack=1`) that it hasn't stored yet: each is the
//! presentation it came from and its content then, so the page's `deckcraft:saved` marks that
//! presentation saved as it was saved, whichever presentation is active by then.

/// At most this many saves wait for the page; the oldest is forgotten first. A forgotten save
/// that the page stores after all leaves its presentation unsaved, which is safe.
pub const MAX_UNACKED: usize = 16;

/// A save the page hasn't stored yet.
pub struct Unacked<T> {
    pub save_id: u64,
    /// The presentation it came from ([`deckcraft_engine::DocState::uid`]).
    pub doc: u64,
    /// Its content as saved.
    pub content: T,
}

/// The saves waiting for the page, oldest first.
pub struct Pending<T>(Vec<Unacked<T>>);

impl<T> Default for Pending<T> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<T> Pending<T> {
    /// A save handed to the page.
    pub fn push(&mut self, save: Unacked<T>) {
        if self.0.len() >= MAX_UNACKED {
            self.0.remove(0);
        }
        self.0.push(save);
    }

    /// The page will never store `save_id` (`deckcraft:discard`: the picker was dismissed, the
    /// write failed, a newer save replaced it): forget it, and its presentation stays unsaved.
    pub fn discard(&mut self, save_id: u64) {
        self.0.retain(|u| u.save_id != save_id);
    }

    /// The page stored `save_id`: that save, with the older saves of its presentation forgotten
    /// (they won't be stored after it). `None` when it isn't waiting (unknown, discarded,
    /// already stored).
    pub fn stored(&mut self, save_id: u64) -> Option<Unacked<T>> {
        let at = self.0.iter().position(|u| u.save_id == save_id)?;
        let save = self.0.remove(at);
        self.0.retain(|u| u.doc != save.doc || u.save_id > save_id);
        Some(save)
    }

    /// Forget the saves of presentations that aren't open any more.
    pub fn retain_open(&mut self, open: &[u64]) {
        self.0.retain(|u| open.contains(&u.doc));
    }

    #[cfg(test)]
    fn ids(&self) -> Vec<u64> {
        self.0.iter().map(|u| u.save_id).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn save(save_id: u64, doc: u64) -> Unacked<&'static str> {
        Unacked { save_id, doc, content: "content" }
    }

    #[test]
    fn a_discarded_save_is_forgotten_and_never_marks_its_presentation_saved() {
        let mut p = Pending::default();
        p.push(save(1, 7));
        p.push(save(2, 7));
        p.discard(1);
        assert_eq!(p.ids(), [2]);
        assert!(p.stored(1).is_none(), "a discarded save stored late counts for nothing");
        assert_eq!(p.stored(2).map(|u| u.doc), Some(7));
        assert!(p.stored(2).is_none(), "stored once");
    }

    #[test]
    fn a_stored_save_forgets_the_older_saves_of_its_presentation_only() {
        let mut p = Pending::default();
        p.push(save(1, 7));
        p.push(save(2, 8));
        p.push(save(3, 7));
        p.push(save(4, 7));
        assert_eq!(p.stored(3).map(|u| u.save_id), Some(3));
        assert_eq!(p.ids(), [2, 4]);
    }

    #[test]
    fn saves_of_closed_presentations_are_forgotten() {
        let mut p = Pending::default();
        p.push(save(1, 7));
        p.push(save(2, 8));
        p.retain_open(&[8]);
        assert_eq!(p.ids(), [2]);
    }

    #[test]
    fn saves_the_page_never_answers_stay_bounded() {
        let mut p = Pending::default();
        for id in 1..=(MAX_UNACKED as u64 * 3) {
            p.push(save(id, 7));
        }
        let ids = p.ids();
        assert_eq!(ids.len(), MAX_UNACKED);
        assert_eq!(ids.last(), Some(&(MAX_UNACKED as u64 * 3)), "the newest saves are kept");
    }
}
