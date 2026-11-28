use ropey::Rope;

use crate::transaction::{EditError, Transaction};

/// Monotonic edit counter. Increases on every applied edit, undo and redo;
/// background work tagged with an older revision is stale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Revision(pub u64);

impl Revision {
    /// The first revision of a new document. Every document gets its own
    /// range of 2^32 revisions, so a document that replaces another under
    /// the same tab (revert, reopen with another encoding) never repeats
    /// its predecessor's revisions and caches keyed by revision cannot be
    /// mistaken for fresh. Edits still count up by one.
    pub fn fresh() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Revision(NEXT.fetch_add(1, Ordering::Relaxed) << 32)
    }
}

/// Line-ending style observed in a text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineEnding {
    /// No line breaks.
    None,
    Lf,
    CrLf,
    Cr,
    /// More than one style is present; the text is kept as-is.
    Mixed,
}

impl LineEnding {
    pub fn detect(text: &str) -> Self {
        Self::detect_chunks(std::iter::once(text))
    }

    /// Detect over text given in pieces (e.g. rope chunks) without
    /// joining them. Stops early once the text is known to be mixed.
    pub fn detect_chunks<'a>(chunks: impl IntoIterator<Item = &'a str>) -> Self {
        let (mut lf, mut crlf, mut cr) = (false, false, false);
        // A '\r' ending the previous chunk, not yet known to start "\r\n".
        let mut pending_cr = false;
        for chunk in chunks {
            let bytes = chunk.as_bytes();
            let mut i = 0;
            if pending_cr && !bytes.is_empty() {
                if bytes[0] == b'\n' {
                    crlf = true;
                    i = 1;
                } else {
                    cr = true;
                }
                pending_cr = false;
            }
            // Most chunks hold no '\r', so only '\n' can be in them: two
            // memchr scans instead of a byte loop (the status bar asks
            // after every keystroke, #206).
            if !bytes[i..].contains(&b'\r') {
                lf = lf || bytes[i..].contains(&b'\n');
                i = bytes.len();
            }
            while i < bytes.len() {
                match bytes[i] {
                    b'\r' if i + 1 == bytes.len() => pending_cr = true,
                    b'\r' if bytes[i + 1] == b'\n' => {
                        crlf = true;
                        i += 1;
                    }
                    b'\r' => cr = true,
                    b'\n' => lf = true,
                    _ => {}
                }
                i += 1;
            }
            if u8::from(lf) + u8::from(crlf) + u8::from(cr) > 1 {
                return Self::Mixed;
            }
        }
        cr |= pending_cr;
        match (lf, crlf, cr) {
            (false, false, false) => Self::None,
            (true, false, false) => Self::Lf,
            (false, true, false) => Self::CrLf,
            (false, false, true) => Self::Cr,
            _ => Self::Mixed,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Document {
    rope: Rope,
    revision: Revision,
    /// History entries are tagged with unique ids. The id of the newest
    /// undo entry identifies the current content state in history.
    /// History entries: an id and the transactions to apply, in order, to
    /// step back (or forward). Most entries hold one transaction; grouped
    /// edits (see [`Document::merge_undo_since`]) hold several.
    undo: Vec<(u64, Vec<Transaction>)>,
    redo: Vec<(u64, Vec<Transaction>)>,
    next_id: u64,
    /// History position (top undo id, `None` = empty history) of the last
    /// clean state: the loaded file, the last save, or a fresh empty buffer.
    saved: Option<u64>,
}

impl Default for Document {
    fn default() -> Self {
        Self::new()
    }
}

impl Document {
    /// An empty document. It is clean until edited.
    pub fn new() -> Self {
        Self {
            rope: Rope::new(),
            revision: Revision::fresh(),
            undo: Vec::new(),
            redo: Vec::new(),
            next_id: 0,
            saved: None,
        }
    }

    /// A document whose content matches a file just loaded from disk.
    pub fn from_saved_text(text: &str) -> Self {
        let mut doc = Self::new();
        doc.rope = Rope::from_str(text);
        doc
    }

    pub fn text(&self) -> String {
        self.rope.to_string()
    }

    pub fn rope(&self) -> &Rope {
        &self.rope
    }

    pub fn len_chars(&self) -> usize {
        self.rope.len_chars()
    }

    pub fn revision(&self) -> Revision {
        self.revision
    }

    /// True when content differs from the last save, judged by history
    /// position: undoing back to the saved state makes the document clean.
    pub fn is_dirty(&self) -> bool {
        self.saved != self.position()
    }

    fn position(&self) -> Option<u64> {
        self.undo.last().map(|(id, _)| *id)
    }

    /// Record that the current content was written to disk successfully.
    /// Call only after the save has completed.
    pub fn mark_saved(&mut self) {
        self.saved = self.position();
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Apply `tx` as one undoable step. Empty transactions are ignored.
    pub fn apply(&mut self, tx: &Transaction) -> Result<(), EditError> {
        if tx.is_empty() {
            return Ok(());
        }
        let inverse = tx.apply(&mut self.rope)?;
        let id = self.next_id;
        self.next_id += 1;
        self.undo.push((id, vec![inverse]));
        self.redo.clear();
        self.bump();
        Ok(())
    }

    /// Number of undo entries, to pass to [`Document::merge_undo_since`].
    pub fn undo_mark(&self) -> usize {
        self.undo.len()
    }

    /// Turn every undo entry added since `mark` into one, so a series of
    /// edits (a macro run) undoes in one step. Positions inside the group
    /// stop being reachable; a save point among them leaves the document
    /// dirty, as it should.
    pub fn merge_undo_since(&mut self, mark: usize) {
        if self.undo.len() <= mark + 1 {
            return;
        }
        let tail = self.undo.split_off(mark);
        let id = tail.last().map_or(0, |(id, _)| *id);
        let group: Vec<Transaction> = tail.into_iter().rev().flat_map(|(_, txs)| txs).collect();
        self.undo.push((id, group));
    }

    pub fn undo(&mut self) -> bool {
        self.step(false).is_some()
    }

    pub fn redo(&mut self) -> bool {
        self.step(true).is_some()
    }

    /// Undo and return where carets belong afterwards, or `None` if there
    /// was nothing to undo.
    pub fn undo_carets(&mut self) -> Option<Vec<usize>> {
        self.step(false)
    }

    pub fn redo_carets(&mut self) -> Option<Vec<usize>> {
        self.step(true)
    }

    fn step(&mut self, redo: bool) -> Option<Vec<usize>> {
        let (from, to) = if redo {
            (&mut self.redo, &mut self.undo)
        } else {
            (&mut self.undo, &mut self.redo)
        };
        let (id, txs) = from.pop()?;
        let carets = txs
            .last()
            .map(Transaction::caret_positions)
            .unwrap_or_default();
        // History entries were produced from valid edits of this exact rope
        // state, so applying them cannot fail; on error, roll back what was
        // applied and keep the entry.
        let mut inverses = Vec::with_capacity(txs.len());
        for tx in &txs {
            match tx.apply(&mut self.rope) {
                Ok(inv) => inverses.push(inv),
                Err(_) => {
                    for inv in inverses.iter().rev() {
                        let _ = inv.apply(&mut self.rope);
                    }
                    from.push((id, txs));
                    return None;
                }
            }
        }
        inverses.reverse();
        to.push((id, inverses));
        self.bump();
        Some(carets)
    }

    fn bump(&mut self) {
        self.revision = Revision(self.revision.0 + 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Change;

    #[test]
    fn line_ending_detection_is_chunk_independent() {
        for text in [
            "",
            "a",
            "a\nb\n",
            "a\r\nb\r\n",
            "a\rb\r",
            "a\nb\r\n",
            "a\r",
            "\r\n",
            "x\r\ny\rz",
            "\r\r\n",
        ] {
            let whole = LineEnding::detect(text);
            // Every split point, including inside "\r\n".
            for k in 0..=text.len() {
                let (a, b) = text.split_at(k);
                assert_eq!(LineEnding::detect_chunks([a, b]), whole, "{text:?} at {k}");
            }
        }
        // Three chunks, each style alone or after the others.
        for (a, b, c, want) in [
            ("x\ny\n", "z\n", "w", LineEnding::Lf),
            ("x\ny\n", "z", "w\r", LineEnding::Mixed),
            ("x\r", "\ny", "z\r\n", LineEnding::CrLf),
            ("x\r", "\ny", "z\n", LineEnding::Mixed),
            ("x\r", "y", "z", LineEnding::Cr),
            ("xy", "z", "", LineEnding::None),
        ] {
            assert_eq!(
                LineEnding::detect_chunks([a, b, c]),
                want,
                "{a:?} {b:?} {c:?}"
            );
        }
        assert_eq!(LineEnding::detect("a\r\nb\r\n"), LineEnding::CrLf);
        assert_eq!(LineEnding::detect("a\r"), LineEnding::Cr);
        assert_eq!(LineEnding::detect("a\nb\r\n"), LineEnding::Mixed);
    }

    #[test]
    fn merged_edits_undo_and_redo_as_one_step() {
        let mut doc = Document::from_saved_text("abc");
        doc.apply(&Transaction::single(Change::insert(0, "0")))
            .unwrap();
        let mark = doc.undo_mark();
        doc.apply(&Transaction::single(Change::insert(4, "d")))
            .unwrap();
        doc.apply(&Transaction::single(Change::delete(1..2)))
            .unwrap();
        doc.apply(&Transaction::single(Change::insert(0, ">")))
            .unwrap();
        doc.merge_undo_since(mark);
        assert_eq!(doc.text(), ">0bcd");
        assert!(doc.undo());
        assert_eq!(doc.text(), "0abc", "the whole group at once");
        assert!(doc.redo());
        assert_eq!(doc.text(), ">0bcd");
        assert!(doc.undo() && doc.undo());
        assert_eq!(doc.text(), "abc");
        assert!(!doc.is_dirty());
        doc.merge_undo_since(doc.undo_mark()); // nothing to merge
        assert!(!doc.undo());
    }

    #[test]
    fn insert_undo_redo() {
        let mut doc = Document::from_saved_text("hello");
        doc.apply(&Transaction::single(Change::insert(5, " world")))
            .unwrap();
        assert_eq!(doc.text(), "hello world");
        assert!(doc.is_dirty());
        assert!(doc.undo());
        assert_eq!(doc.text(), "hello");
        assert!(doc.redo());
        assert_eq!(doc.text(), "hello world");
        assert!(!doc.redo());
    }

    #[test]
    fn multi_change_transaction_undoes_as_one_step() {
        let mut doc = Document::from_saved_text("abc");
        let mut tx = Transaction::new();
        tx.push(Change::insert(0, "1"))
            .push(Change::replace(2..3, "B"))
            .push(Change::delete(3..4));
        doc.apply(&tx).unwrap();
        assert_eq!(doc.text(), "1aB");
        assert!(doc.undo());
        assert_eq!(doc.text(), "abc");
        assert!(!doc.can_undo());
    }

    #[test]
    fn a_new_document_never_repeats_another_documents_revisions() {
        let mut a = Document::from_saved_text("one");
        let b = Document::from_saved_text("one");
        assert_ne!(a.revision(), b.revision());
        let start = a.revision();
        let mut tx = Transaction::new();
        tx.push(Change::insert(0, "x"));
        a.apply(&tx).unwrap();
        assert_eq!(a.revision(), Revision(start.0 + 1), "edits count by one");
        assert_ne!(Document::new().revision(), start);
    }

    #[test]
    fn invalid_transaction_leaves_document_untouched() {
        let mut doc = Document::from_saved_text("abc");
        let start = doc.revision();
        let mut tx = Transaction::new();
        tx.push(Change::insert(0, "x")).push(Change::delete(2..99));
        let err = doc.apply(&tx).unwrap_err();
        assert!(matches!(err, EditError::OutOfBounds { len: 4, .. }));
        assert_eq!(doc.text(), "abc");
        assert_eq!(doc.revision(), start);
        assert!(!doc.can_undo());
    }

    #[test]
    #[allow(clippy::reversed_empty_ranges)]
    fn inverted_range_is_rejected() {
        let mut doc = Document::from_saved_text("abc");
        let err = doc
            .apply(&Transaction::single(Change::delete(2..1)))
            .unwrap_err();
        assert_eq!(err, EditError::InvertedRange { start: 2, end: 1 });
    }

    #[test]
    fn new_edit_clears_redo() {
        let mut doc = Document::new();
        doc.apply(&Transaction::single(Change::insert(0, "a")))
            .unwrap();
        doc.undo();
        doc.apply(&Transaction::single(Change::insert(0, "b")))
            .unwrap();
        assert!(!doc.can_redo());
    }

    #[test]
    fn revision_increases_on_every_mutation() {
        let mut doc = Document::new();
        let r0 = doc.revision();
        doc.apply(&Transaction::single(Change::insert(0, "a")))
            .unwrap();
        let r1 = doc.revision();
        doc.undo();
        let r2 = doc.revision();
        assert!(r0 < r1 && r1 < r2);
    }

    #[test]
    fn empty_transaction_is_noop() {
        let mut doc = Document::from_saved_text("x");
        doc.apply(&Transaction::new()).unwrap();
        doc.apply(&Transaction::single(Change::insert(1, "")))
            .unwrap();
        assert!(!doc.is_dirty());
        assert!(!doc.can_undo());
    }

    #[test]
    fn undo_to_saved_point_is_clean_and_redo_is_dirty() {
        let mut doc = Document::from_saved_text("a");
        doc.apply(&Transaction::single(Change::insert(1, "b")))
            .unwrap();
        doc.mark_saved();
        doc.apply(&Transaction::single(Change::insert(2, "c")))
            .unwrap();
        assert!(doc.is_dirty());
        doc.undo();
        assert!(!doc.is_dirty());
        doc.undo();
        assert!(doc.is_dirty());
        doc.redo();
        assert!(!doc.is_dirty());
    }

    #[test]
    fn unreachable_save_point_stays_dirty() {
        let mut doc = Document::from_saved_text("a");
        doc.apply(&Transaction::single(Change::insert(1, "b")))
            .unwrap();
        doc.mark_saved();
        doc.undo();
        // Branching history discards the saved state from redo.
        doc.apply(&Transaction::single(Change::insert(1, "b")))
            .unwrap();
        assert_eq!(doc.text(), "ab");
        assert!(doc.is_dirty(), "content equal but save point unreachable");
    }

    #[test]
    fn new_document_is_clean_until_edited() {
        let mut doc = Document::new();
        assert!(!doc.is_dirty());
        doc.apply(&Transaction::single(Change::insert(0, "a")))
            .unwrap();
        assert!(doc.is_dirty());
        doc.mark_saved();
        assert!(!doc.is_dirty());
    }

    #[test]
    fn edits_preserve_crlf_and_non_ascii() {
        let mut doc = Document::from_saved_text("a\r\n🙂é\r\n");
        // Char indices: a=0 \r=1 \n=2 🙂=3 é=4
        doc.apply(&Transaction::single(Change::insert(4, "x")))
            .unwrap();
        assert_eq!(doc.text(), "a\r\n🙂xé\r\n");
        doc.undo();
        assert_eq!(doc.text(), "a\r\n🙂é\r\n");
    }

    #[test]
    fn undo_reports_caret_positions() {
        let mut doc = Document::from_saved_text("abcd");
        let mut tx = Transaction::new();
        // Descending order, as multi-cursor commands emit.
        tx.push(Change::insert(3, "XX"))
            .push(Change::insert(1, "Y"));
        doc.apply(&tx).unwrap();
        assert_eq!(doc.text(), "aYbcXXd");
        assert_eq!(doc.undo_carets(), Some(vec![1, 3]));
        assert_eq!(doc.redo_carets(), Some(vec![2, 6]));
        assert_eq!(doc.redo_carets(), None);
    }

    #[test]
    fn line_ending_detection() {
        assert_eq!(LineEnding::detect(""), LineEnding::None);
        assert_eq!(LineEnding::detect("a\nb\n"), LineEnding::Lf);
        assert_eq!(LineEnding::detect("a\r\nb\r\n"), LineEnding::CrLf);
        assert_eq!(LineEnding::detect("a\rb"), LineEnding::Cr);
        assert_eq!(LineEnding::detect("a\r\nb\n"), LineEnding::Mixed);
    }
}
