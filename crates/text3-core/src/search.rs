//! Literal and regular-expression search over a document, and replace-all
//! as a single undoable transaction.
//!
//! Matching uses the `regex` crate, which guarantees linear-time matching
//! (no catastrophic backtracking), so hostile patterns cannot hang the UI.
//! Empty matches are skipped: they cannot be selected or meaningfully
//! replaced one by one.

use std::ops::Range;

use regex::{Regex, RegexBuilder};
use ropey::Rope;

use crate::transaction::{Change, Transaction};

/// Stop collecting after this many matches; the UI reports "N+".
pub const MATCH_LIMIT: usize = 100_000;
/// Upper bound on compiled pattern size (bytes), against pattern bombs.
const PATTERN_SIZE_LIMIT: usize = 1 << 20;
/// Searching a document goes through pieces of about this many bytes,
/// each ending after a line break, instead of one copy of the whole text
/// (#210). Only for patterns that cannot match a line break.
const SEGMENT: usize = if cfg!(test) { 64 } else { 1 << 20 };

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct Query {
    pub pattern: String,
    pub regex: bool,
    pub case_sensitive: bool,
    pub whole_word: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SearchError {
    #[error("invalid pattern: {0}")]
    InvalidPattern(String),
}

#[derive(Debug, Clone)]
pub struct Matcher {
    re: Regex,
    regex_mode: bool,
    /// The pattern can never match `\n`, so no match crosses a line
    /// break and the text can be searched piece by piece.
    within_lines: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Matches {
    /// Char ranges, ascending, non-overlapping, non-empty.
    pub ranges: Vec<Range<usize>>,
    /// True when [`MATCH_LIMIT`] was reached.
    pub truncated: bool,
}

impl Query {
    pub fn compile(&self) -> Result<Option<Matcher>, SearchError> {
        if self.pattern.is_empty() {
            return Ok(None);
        }
        let core = if self.regex {
            self.pattern.clone()
        } else {
            regex::escape(&self.pattern)
        };
        let pattern = if self.whole_word {
            format!(r"\b(?:{core})\b")
        } else {
            core
        };
        let re = RegexBuilder::new(&pattern)
            .case_insensitive(!self.case_sensitive)
            .multi_line(true)
            // `$`, `^` and `.` treat `\r\n` as one line break, so a pattern
            // like `\s+$` or `.*` never consumes the CR of a CRLF file.
            .crlf(true)
            .size_limit(PATTERN_SIZE_LIMIT)
            .build()
            .map_err(|e| SearchError::InvalidPattern(e.to_string()))?;
        let within_lines = regex_syntax::ParserBuilder::new()
            .case_insensitive(!self.case_sensitive)
            .multi_line(true)
            .crlf(true)
            .build()
            .parse(&pattern)
            .is_ok_and(|hir| !can_match_newline(&hir));
        Ok(Some(Matcher {
            re,
            regex_mode: self.regex,
            within_lines,
        }))
    }
}

impl Matcher {
    /// Non-empty matches in `text` as byte ranges (for searching files
    /// without building a rope).
    pub fn find_in_str<'t>(&'t self, text: &'t str) -> impl Iterator<Item = Range<usize>> + 't {
        self.re
            .find_iter(text)
            .filter(|m| m.start() != m.end())
            .map(|m| m.range())
    }

    /// Call `f` with the document's text and the char index it starts
    /// at: in pieces of about [`SEGMENT`] bytes, each ending after a line
    /// break, when no match can cross one; otherwise in one piece.
    /// Returns false when `f` cancelled.
    fn segments(&self, rope: &Rope, mut f: impl FnMut(&str, usize) -> Seg) -> bool {
        if !self.within_lines || rope.len_bytes() <= SEGMENT {
            return f(&rope.to_string(), 0) != Seg::Cancel;
        }
        let mut buf = String::with_capacity(SEGMENT + 64 * 1024);
        let mut base = 0usize;
        let mut chunks = rope.chunks().peekable();
        while let Some(chunk) = chunks.next() {
            buf.push_str(chunk);
            let last = chunks.peek().is_none();
            if buf.len() < SEGMENT && !last {
                continue;
            }
            // Search up to the last line break; carry the rest over.
            let cut = if last {
                buf.len()
            } else {
                match buf.rfind('\n') {
                    Some(i) => i + 1,
                    None => continue,
                }
            };
            let piece = &buf[..cut];
            match f(piece, base) {
                Seg::Next => {}
                Seg::Stop => return true,
                Seg::Cancel => return false,
            }
            base += piece.chars().count();
            buf.drain(..cut);
        }
        true
    }

    pub fn find_all(&self, rope: &Rope) -> Matches {
        self.find_all_cancellable(rope, &std::sync::atomic::AtomicBool::new(false))
            .unwrap_or_default()
    }

    /// [`Matcher::find_all`] that stops (returning `None`) once `cancel` is
    /// set, checked between matches.
    pub fn find_all_cancellable(
        &self,
        rope: &Rope,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Option<Matches> {
        let mut out = Matches::default();
        let done = self.segments(rope, |text, base| {
            // Walk byte offsets and convert to chars incrementally (matches
            // are ascending), avoiding a rope lookup per match.
            let (mut byte, mut chr) = (0usize, base);
            for m in self.re.find_iter(text) {
                if m.start() == m.end() {
                    continue;
                }
                if out.ranges.len() == MATCH_LIMIT {
                    out.truncated = true;
                    return Seg::Stop;
                }
                if out.ranges.len().is_multiple_of(1024)
                    && cancel.load(std::sync::atomic::Ordering::Relaxed)
                {
                    return Seg::Cancel;
                }
                chr += text[byte..m.start()].chars().count();
                let start = chr;
                chr += m.as_str().chars().count();
                byte = m.end();
                out.ranges.push(start..chr);
            }
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                return Seg::Cancel;
            }
            Seg::Next
        });
        done.then_some(out)
    }

    /// Replace every match with `replacement` in one transaction. In regex
    /// mode, `$1` / `${name}` expand capture groups; otherwise the text is
    /// inserted literally. Returns the transaction and the number replaced.
    pub fn replace_all(&self, rope: &Rope, replacement: &str) -> (Transaction, usize) {
        let mut edits: Vec<(Range<usize>, String)> = Vec::new();
        self.segments(rope, |text, base| {
            let (mut byte, mut chr) = (0usize, base);
            for caps in self.re.captures_iter(text) {
                let Some(m) = caps.get(0) else { continue };
                if m.start() == m.end() {
                    continue;
                }
                let mut with = String::new();
                if self.regex_mode {
                    caps.expand(replacement, &mut with);
                } else {
                    with.push_str(replacement);
                }
                chr += text[byte..m.start()].chars().count();
                let start = chr;
                chr += m.as_str().chars().count();
                byte = m.end();
                edits.push((start..chr, with));
            }
            Seg::Next
        });
        let mut tx = Transaction::new();
        for (range, with) in edits.iter().rev() {
            tx.push(Change::replace(range.clone(), with.clone()));
        }
        (tx, edits.len())
    }

    /// Lines a replace-all transaction changes, as (0-based line in the
    /// original, text before, text after), at most `max` of them, for a
    /// preview. Line breaks are not included.
    pub fn preview(rope: &Rope, tx: &Transaction, max: usize) -> Vec<(usize, String, String)> {
        let mut after = rope.clone();
        if tx.apply(&mut after).is_err() {
            return Vec::new();
        }
        let line_text = |r: &Rope, l: usize| -> String {
            let s: String = r.line(l).chars().collect();
            s.trim_end_matches(['\n', '\r']).to_owned()
        };
        let mut starts: Vec<usize> = tx.changes().iter().map(|c| c.range.start).collect();
        starts.sort_unstable();
        let mut out: Vec<(usize, String, String)> = Vec::new();
        for start in starts {
            let line = rope.char_to_line(start.min(rope.len_chars()));
            if out.last().is_some_and(|(l, _, _)| *l == line) {
                continue;
            }
            if out.len() == max {
                break;
            }
            let new_pos = tx.map_pos(rope.line_to_char(line)).min(after.len_chars());
            let new_line = after.char_to_line(new_pos);
            out.push((line, line_text(rope, line), line_text(&after, new_line)));
        }
        out
    }

    /// The replacement for a single match at `range` (for "Replace").
    pub fn replacement_for(
        &self,
        rope: &Rope,
        range: Range<usize>,
        replacement: &str,
    ) -> Option<String> {
        let found: String = rope.slice(range).into();
        let caps = self.re.captures(&found)?;
        let m = caps.get(0)?;
        if m.start() != 0 || m.end() != found.len() {
            return None;
        }
        let mut with = String::new();
        if self.regex_mode {
            caps.expand(replacement, &mut with);
        } else {
            with.push_str(replacement);
        }
        Some(with)
    }
}

impl Matches {
    /// Index of the first match starting at or after `from` (forward), or
    /// the last match ending at or before `from` (backward), wrapping.
    pub fn next_from(&self, from: usize, forward: bool) -> Option<usize> {
        if self.ranges.is_empty() {
            return None;
        }
        if forward {
            Some(
                self.ranges
                    .iter()
                    .position(|r| r.start >= from)
                    .unwrap_or(0),
            )
        } else {
            Some(
                self.ranges
                    .iter()
                    .rposition(|r| r.end <= from)
                    .unwrap_or(self.ranges.len() - 1),
            )
        }
    }

    /// Index of the match exactly at `range`, if any.
    pub fn index_of(&self, range: &Range<usize>) -> Option<usize> {
        self.ranges
            .binary_search_by_key(&range.start, |r| r.start)
            .ok()
            .filter(|&i| self.ranges[i] == *range)
    }
}

/// What [`Matcher::segments`] does after a piece.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Seg {
    Next,
    /// Enough found (match limit).
    Stop,
    Cancel,
}

/// Whether a match of `hir` could contain `\n`.
fn can_match_newline(hir: &regex_syntax::hir::Hir) -> bool {
    use regex_syntax::hir::{Class, HirKind};
    match hir.kind() {
        HirKind::Empty | HirKind::Look(_) => false,
        HirKind::Literal(l) => l.0.contains(&b'\n'),
        HirKind::Class(Class::Unicode(c)) => c
            .ranges()
            .iter()
            .any(|r| r.start() <= '\n' && '\n' <= r.end()),
        HirKind::Class(Class::Bytes(c)) => c
            .ranges()
            .iter()
            .any(|r| r.start() <= b'\n' && b'\n' <= r.end()),
        HirKind::Repetition(r) => can_match_newline(&r.sub),
        HirKind::Capture(c) => can_match_newline(&c.sub),
        HirKind::Concat(v) | HirKind::Alternation(v) => v.iter().any(can_match_newline),
    }
}

#[cfg(test)]
mod tests {

    /// Pieces give exactly the matches of one search over the whole text.
    #[test]
    fn pieces_match_the_whole_text() {
        let mut text = String::new();
        for i in 0..400 {
            text.push_str(match i % 5 {
                0 => "alpha beta äö gamma\n",
                1 => "Beta\r\n",
                2 => "",
                3 => "a long line without much in it but beta at the end beta\n",
                _ => "😀 beta\tgamma  \n",
            });
        }
        let rope = Rope::from_str(&text);
        let queries = [
            ("beta", false, false, false),
            ("beta", false, true, true),
            ("gamma\\s*$", true, false, false),
            ("^\\w+", true, true, false),
            ("ä.", true, false, false),
            ("beta\\s+gamma", true, false, false),
            ("\\n\\n", true, false, false),
            ("beta\\r?\\nalpha", true, false, false),
            ("[^a]+", true, false, false),
            ("😀", false, false, false),
        ];
        for (pattern, regex, case_sensitive, whole_word) in queries {
            let q = Query {
                pattern: pattern.into(),
                regex,
                case_sensitive,
                whole_word,
            };
            let m = q.compile().unwrap().unwrap();
            let want: Vec<Range<usize>> = m
                .find_in_str(&text)
                .map(|r| text[..r.start].chars().count()..text[..r.end].chars().count())
                .collect();
            assert_eq!(m.find_all(&rope).ranges, want, "{pattern}");
            let (tx, n) = m.replace_all(&rope, "X");
            assert_eq!(n, want.len(), "{pattern}");
            let mut after = rope.clone();
            tx.apply(&mut after).unwrap();
            let whole = m.re.replace_all(&text, regex::NoExpand("X"));
            assert_eq!(after.to_string(), whole, "{pattern}");
        }
    }

    #[test]
    fn line_break_patterns_search_the_whole_text() {
        let within = |p: &str| {
            Query {
                pattern: p.into(),
                regex: true,
                ..Default::default()
            }
            .compile()
            .unwrap()
            .unwrap()
            .within_lines
        };
        assert!(within("beta"));
        assert!(within("a.b"));
        assert!(within("^x$"));
        assert!(within("[a-z]+"));
        assert!(!within("a\\nb"));
        assert!(!within("\\s"));
        assert!(!within("[^a]"));
        assert!(!within("(?s)a.b"));
        assert!(!within("\\W"));
    }

    #[test]
    fn regex_keeps_crlf() {
        let q = |p: &str| {
            Query {
                pattern: p.into(),
                regex: true,
                ..Default::default()
            }
            .compile()
            .unwrap()
            .unwrap()
        };
        let text = "foo bar  \r\nx  \r\n";
        let run = |p: &str, with: &str| {
            let rope = ropey::Rope::from_str(text);
            let (tx, _) = q(p).replace_all(&rope, with);
            let mut doc = crate::Document::from_saved_text(text);
            doc.apply(&tx).unwrap();
            doc.text()
        };
        assert_eq!(
            run(r"[ \t]+$", ""),
            "foo bar\r\nx\r\n",
            "trailing spaces, CRLF kept"
        );
        // `\s` includes line breaks, but a CR is never split from its LF.
        assert!(!run(r"\s+$", "").replace("\r\n", "").contains(['\r', '\n']));
        assert_eq!(run("foo.*$", "Z"), "Z\r\nx  \r\n");
    }
    use super::*;
    use crate::Document;

    fn q(pattern: &str) -> Query {
        Query {
            pattern: pattern.into(),
            ..Default::default()
        }
    }

    fn find(text: &str, query: Query) -> Vec<Range<usize>> {
        query
            .compile()
            .unwrap()
            .unwrap()
            .find_all(&Rope::from_str(text))
            .ranges
    }

    #[test]
    fn cancelled_search_returns_none() {
        let rope = Rope::from_str(&"ab ".repeat(5000));
        let m = Query {
            pattern: "ab".into(),
            ..Query::default()
        }
        .compile()
        .unwrap()
        .unwrap();
        let yes = std::sync::atomic::AtomicBool::new(true);
        assert_eq!(m.find_all_cancellable(&rope, &yes), None);
        let no = std::sync::atomic::AtomicBool::new(false);
        assert_eq!(
            m.find_all_cancellable(&rope, &no).unwrap().ranges.len(),
            5000
        );
        assert_eq!(m.find_all(&rope).ranges.len(), 5000);
    }

    #[test]
    fn replace_preview_lists_changed_lines() {
        let rope = Rope::from_str("let a = 1;\nlet b = a + a;\nfoo\nlet c = a;\n");
        let q = Query {
            pattern: "a".into(),
            whole_word: true,
            ..Query::default()
        };
        let m = q.compile().unwrap().unwrap();
        let (tx, n) = m.replace_all(&rope, "alpha");
        assert_eq!(n, 4);
        let p = Matcher::preview(&rope, &tx, 10);
        assert_eq!(
            p,
            vec![
                (0, "let a = 1;".into(), "let alpha = 1;".into()),
                (1, "let b = a + a;".into(), "let b = alpha + alpha;".into()),
                (3, "let c = a;".into(), "let c = alpha;".into()),
            ]
        );
        assert_eq!(Matcher::preview(&rope, &tx, 2).len(), 2, "capped");
    }

    #[test]
    fn literal_is_escaped_and_case_insensitive_by_default() {
        assert_eq!(find("a.b A.B axb", q("a.b")), vec![0..3, 4..7]);
        let cs = Query {
            case_sensitive: true,
            ..q("a.b")
        };
        assert_eq!(find("a.b A.B", cs), vec![0..3]);
    }

    #[test]
    fn char_offsets_with_multibyte_text() {
        assert_eq!(find("🙂ä x 🙂ä", q("ä")), vec![1..2, 6..7]);
    }

    #[test]
    fn whole_word_and_regex() {
        let ww = Query {
            whole_word: true,
            ..q("cat")
        };
        assert_eq!(find("cat concat cat_ cat.", ww), vec![0..3, 16..19]);
        let re = Query {
            regex: true,
            ..q(r"^\w+")
        };
        assert_eq!(find("ab cd\r\nef", re), vec![0..2, 7..9]);
    }

    #[test]
    fn empty_pattern_and_empty_matches() {
        assert!(q("").compile().unwrap().is_none());
        let re = Query {
            regex: true,
            ..q("x*")
        };
        assert_eq!(find("axxb", re), vec![1..3]);
    }

    #[test]
    fn invalid_regex_is_an_error_not_a_panic() {
        let re = Query {
            regex: true,
            ..q("(unclosed")
        };
        assert!(matches!(re.compile(), Err(SearchError::InvalidPattern(_))));
    }

    #[test]
    fn replace_all_is_one_undoable_step_with_captures() {
        let mut doc = Document::from_saved_text("key=1\nother=22\n");
        let m = Query {
            regex: true,
            ..q(r"(\w+)=(\d+)")
        }
        .compile()
        .unwrap()
        .unwrap();
        let (tx, n) = m.replace_all(doc.rope(), "$2:$1");
        assert_eq!(n, 2);
        doc.apply(&tx).unwrap();
        assert_eq!(doc.text(), "1:key\n22:other\n");
        doc.undo();
        assert_eq!(doc.text(), "key=1\nother=22\n");
        // Literal mode inserts `$1` as-is.
        let lit = q("=").compile().unwrap().unwrap();
        let (tx, _) = lit.replace_all(doc.rope(), "$1");
        doc.apply(&tx).unwrap();
        assert_eq!(doc.text(), "key$11\nother$122\n");
    }

    #[test]
    fn navigation_wraps() {
        let m = Matches {
            ranges: vec![2..3, 5..6, 9..10],
            truncated: false,
        };
        assert_eq!(m.next_from(0, true), Some(0));
        assert_eq!(m.next_from(6, true), Some(2));
        assert_eq!(m.next_from(10, true), Some(0), "wraps forward");
        assert_eq!(m.next_from(5, false), Some(0));
        assert_eq!(m.next_from(1, false), Some(2), "wraps backward");
        assert_eq!(m.index_of(&(5..6)), Some(1));
        assert_eq!(m.index_of(&(5..7)), None);
    }

    #[test]
    fn pathological_pattern_is_fast() {
        // Classic catastrophic-backtracking pattern for backtracking engines.
        let text = "a".repeat(5_000) + "!";
        let re = Query {
            regex: true,
            ..q("(a+)+$")
        };
        let t = std::time::Instant::now();
        let _ = find(&text, re);
        assert!(t.elapsed() < std::time::Duration::from_secs(2));
    }
}
