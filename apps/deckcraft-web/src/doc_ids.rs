//! Which document a file sent by the host page (`?host=parent`) became, so the host writes a
//! document back only to the file it came from. The bridge hands the app one sent file per frame
//! and looks at the documents after the next frame: a presentation made with File › New, or
//! opened with the app's own File › Open, is never taken for the sent file, and the host asks
//! where to save it instead of overwriting the file it sent.

/// The document `name` became: the first one not open in `before` whose title starts with the
/// file's stem (the title is the stem, `"Report"` for `Report.pptx`, or `"{stem} (Repaired)"`
/// for a damaged file). `None` when the file didn't open (unreadable, password-protected, or an
/// outline whose slides went into the open presentation): a document that appeared then counts
/// as the user's own and is never written over the sent file.
pub fn opened_doc<'a>(before: &[u64], after: impl IntoIterator<Item = (u64, &'a str)>, name: &str) -> Option<u64> {
    let stem = std::path::Path::new(name).file_stem().and_then(std::ffi::OsStr::to_str).unwrap_or(name);
    after.into_iter().find(|(id, title)| !before.contains(id) && title.starts_with(stem)).map(|(id, _)| id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_new_document_named_after_the_file_is_the_one_it_became() {
        assert_eq!(opened_doc(&[1], [(1, "Presentation 1"), (2, "Report")], "Report.pptx"), Some(2));
        assert_eq!(opened_doc(&[1], [(1, "Presentation 1"), (2, "deck (Repaired)")], "deck.pptx"), Some(2));
    }

    #[test]
    fn a_blank_start_closed_for_the_sent_file_does_not_matter() {
        // The untouched blank presentation the page started with is closed once the file opens.
        assert_eq!(opened_doc(&[1], [(2, "Report")], "Report.pptx"), Some(2));
    }

    #[test]
    fn a_document_the_user_made_is_not_taken_for_a_file_that_failed_to_open() {
        // The sent file was refused; the user's File › New in the same frame made `Presentation 2`.
        assert_eq!(opened_doc(&[1], [(1, "Presentation 1"), (2, "Presentation 2")], "Report.pptx"), None);
    }

    #[test]
    fn an_already_open_document_with_the_same_name_is_not_the_new_one() {
        // `Report` was open before; the second copy sent is document 3.
        assert_eq!(opened_doc(&[1, 2], [(1, "Presentation 1"), (2, "Report"), (3, "Report")], "Report.pptx"), Some(3));
        assert_eq!(opened_doc(&[1, 2], [(1, "Presentation 1"), (2, "Report")], "Report.pptx"), None);
    }
}
