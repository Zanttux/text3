//! Read-only viewer for large or growing files: background indexing and
//! filtering (both cancellable), lines read from disk only when visible,
//! and following a file as it grows.

use eframe::egui;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use text3_core::search::{Matcher, Query};
use text3_io::logfile::{self, CHUNK, LineIndex};

/// Bytes of a line shown (and searched); longer lines are cut.
const MAX_LINE: usize = 16 * 1024;
const POLL: Duration = Duration::from_millis(500);

enum Msg {
    Index {
        epoch: u64,
        starts: Vec<u64>,
        end: u64,
        done: bool,
    },
    Filter {
        epoch: u64,
        lines: Vec<usize>,
        done_bytes: u64,
        finished: bool,
    },
    Error(String),
}

struct Filter {
    matcher: Matcher,
    lines: Vec<usize>,
    /// Bytes of the file the filter has covered (always a line start).
    done: u64,
    /// Indexed length and completeness when the last pass started.
    scanned: (u64, bool),
    running: Option<Arc<AtomicBool>>,
}

pub struct LogView {
    pub path: PathBuf,
    file: File,
    index: LineIndex,
    len: u64,
    epoch: u64,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    indexing: Option<Arc<AtomicBool>>,
    filter: Option<Filter>,
    pub query: String,
    pub regex: bool,
    pub follow: bool,
    pub bad_pattern: bool,
    /// A line to scroll to on the next frame (Go to Line).
    pub jump: Option<usize>,
    pub error: Option<String>,
    /// Display encoding, detected from the start of the file.
    pub encoding: &'static logfile::Encoding,
    last_poll: Instant,
    ctx: egui::Context,
}

impl Drop for LogView {
    fn drop(&mut self) {
        self.cancel_all();
    }
}

fn spawn_index(
    path: PathBuf,
    from: u64,
    epoch: u64,
    enc: &'static logfile::Encoding,
    tx: Sender<Msg>,
    ctx: egui::Context,
) -> Arc<AtomicBool> {
    let cancel = Arc::new(AtomicBool::new(false));
    let stop = cancel.clone();
    let _ = std::thread::Builder::new()
        .name("text3-log-index".into())
        .spawn(move || {
            let run = || -> std::io::Result<()> {
                let mut f = File::open(&path)?;
                f.seek(SeekFrom::Start(from))?;
                let mut buf = vec![0; CHUNK];
                let mut end = from;
                let mut last = Instant::now();
                let mut pending = Vec::new();
                loop {
                    if stop.load(Ordering::Relaxed) {
                        return Ok(());
                    }
                    let mut n = f.read(&mut buf)?;
                    // UTF-16: index whole units only; an odd byte is read
                    // again with the next chunk.
                    if logfile::unit(enc) == 2 && n % 2 == 1 {
                        n -= 1;
                        f.seek(SeekFrom::Current(-1))?;
                        if n == 0 {
                            // Half a unit at the end of a growing file.
                            let _ = tx.send(Msg::Index {
                                epoch,
                                starts: std::mem::take(&mut pending),
                                end,
                                done: true,
                            });
                            ctx.request_repaint();
                            return Ok(());
                        }
                    }
                    if n > 0 {
                        pending.extend(logfile::starts_in_enc(&buf[..n], end, enc));
                        end += n as u64;
                    }
                    if n == 0 || last.elapsed() > Duration::from_millis(100) {
                        let starts = std::mem::take(&mut pending);
                        let _ = tx.send(Msg::Index {
                            epoch,
                            starts,
                            end,
                            done: n == 0,
                        });
                        ctx.request_repaint();
                        last = Instant::now();
                    }
                    if n == 0 {
                        return Ok(());
                    }
                }
            };
            if let Err(e) = run() {
                let _ = tx.send(Msg::Error(e.to_string()));
                ctx.request_repaint();
            }
        });
    cancel
}

#[allow(clippy::too_many_arguments)]
fn spawn_filter(
    path: PathBuf,
    from: u64,
    first_line: usize,
    len: u64,
    include_partial: bool,
    enc: &'static logfile::Encoding,
    matcher: Matcher,
    epoch: u64,
    tx: Sender<Msg>,
    ctx: egui::Context,
) -> Arc<AtomicBool> {
    let cancel = Arc::new(AtomicBool::new(false));
    let stop = cancel.clone();
    let _ = std::thread::Builder::new()
        .name("text3-log-filter".into())
        .spawn(move || {
            let run = || -> std::io::Result<()> {
                let mut f = File::open(&path)?;
                f.seek(SeekFrom::Start(from))?;
                let mut done_bytes = from;
                let finished = logfile::filter_lines(
                    f,
                    first_line,
                    len,
                    MAX_LINE,
                    include_partial,
                    enc,
                    &stop,
                    |l| matcher.find_in_str(l).next().is_some(),
                    |lines, pos| {
                        done_bytes = from + pos;
                        let _ = tx.send(Msg::Filter {
                            epoch,
                            lines,
                            done_bytes,
                            finished: false,
                        });
                        ctx.request_repaint();
                    },
                )?;
                if finished {
                    let _ = tx.send(Msg::Filter {
                        epoch,
                        lines: Vec::new(),
                        done_bytes,
                        finished: true,
                    });
                    ctx.request_repaint();
                }
                Ok(())
            };
            if let Err(e) = run() {
                let _ = tx.send(Msg::Error(e.to_string()));
                ctx.request_repaint();
            }
        });
    cancel
}

impl LogView {
    pub fn open(path: &Path, ctx: &egui::Context) -> std::io::Result<Self> {
        let mut file = File::open(path)?;
        let meta = file.metadata()?;
        if !meta.is_file() {
            return Err(std::io::Error::other("not a file"));
        }
        let mut head = Vec::new();
        (&mut file).take(64 * 1024).read_to_end(&mut head)?;
        let encoding = logfile::detect(&head);
        let (tx, rx) = channel();
        let mut v = Self {
            path: path.to_owned(),
            file,
            index: LineIndex::default(),
            len: meta.len(),
            epoch: 0,
            tx,
            rx,
            indexing: None,
            filter: None,
            query: String::new(),
            regex: false,
            follow: false,
            bad_pattern: false,
            jump: None,
            error: None,
            encoding,
            last_poll: Instant::now(),
            ctx: ctx.clone(),
        };
        v.indexing = Some(spawn_index(
            v.path.clone(),
            0,
            0,
            v.encoding,
            v.tx.clone(),
            ctx.clone(),
        ));
        Ok(v)
    }

    fn cancel_all(&mut self) {
        if let Some(c) = self.indexing.take() {
            c.store(true, Ordering::Relaxed);
        }
        if let Some(c) = self.filter.as_mut().and_then(|f| f.running.take()) {
            c.store(true, Ordering::Relaxed);
        }
    }

    pub fn line_count(&self) -> usize {
        self.index.line_count()
    }

    /// Whether the first indexing pass has finished.
    pub fn indexed(&self) -> bool {
        self.indexing.is_none()
    }

    /// Whether the filter has gone through the whole indexed file.
    pub fn filter_finished(&self) -> bool {
        self.filter
            .as_ref()
            .is_some_and(|f| f.running.is_none() && f.scanned.1 && f.scanned.0 == self.index.end())
    }

    /// Indexing progress 0..=100 while the first pass runs.
    pub fn progress(&self) -> Option<u8> {
        (self.indexing.is_some() && self.len > 0)
            .then(|| (self.index.end().min(self.len) * 100 / self.len) as u8)
    }

    /// Matching lines so far and whether the filter is still running.
    pub fn matches(&self) -> Option<(usize, bool)> {
        self.filter
            .as_ref()
            .map(|f| (f.lines.len(), f.running.is_some()))
    }

    /// Start over: the file shrank or was replaced.
    fn reset(&mut self) {
        self.cancel_all();
        self.epoch += 1;
        self.index = LineIndex::default();
        if let Ok(f) = File::open(&self.path) {
            self.file = f;
        }
        if let Some(f) = &mut self.filter {
            f.lines.clear();
            f.done = 0;
            f.scanned = (0, false);
        }
        self.indexing = Some(spawn_index(
            self.path.clone(),
            0,
            self.epoch,
            self.encoding,
            self.tx.clone(),
            self.ctx.clone(),
        ));
    }

    /// Apply a new filter text (empty clears it).
    pub fn set_query(&mut self) {
        if let Some(c) = self.filter.take().and_then(|f| f.running) {
            c.store(true, Ordering::Relaxed);
        }
        self.bad_pattern = false;
        if self.query.is_empty() {
            return;
        }
        let q = Query {
            pattern: self.query.clone(),
            regex: self.regex,
            case_sensitive: false,
            whole_word: false,
        };
        match q.compile() {
            Ok(Some(matcher)) => {
                self.filter = Some(Filter {
                    matcher,
                    lines: Vec::new(),
                    done: 0,
                    scanned: (0, false),
                    running: None,
                });
            }
            Ok(None) => {}
            Err(_) => self.bad_pattern = true,
        }
    }

    /// Process worker messages, watch the file, and keep the filter up to
    /// date with the indexed part.
    pub fn tick(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Index {
                    epoch,
                    starts,
                    end,
                    done,
                } if epoch == self.epoch => {
                    if end >= self.index.end() {
                        self.index.append(&starts, end);
                    }
                    self.len = self.len.max(end);
                    if done {
                        self.indexing = None;
                    }
                }
                Msg::Filter {
                    epoch,
                    lines,
                    done_bytes,
                    finished,
                } if epoch == self.epoch => {
                    if let Some(f) = &mut self.filter {
                        for l in lines {
                            // The partial last line may be reported twice.
                            if f.lines.last() != Some(&l) {
                                f.lines.push(l);
                            }
                        }
                        f.done = done_bytes;
                        if finished {
                            f.running = None;
                        }
                    }
                }
                Msg::Error(e) => self.error = Some(e),
                _ => {}
            }
        }
        if self.last_poll.elapsed() >= POLL {
            self.last_poll = Instant::now();
            if let Ok(meta) = std::fs::metadata(&self.path) {
                let len = meta.len();
                if len < self.index.end() {
                    self.reset();
                } else if len > self.index.end() && self.indexing.is_none() {
                    self.indexing = Some(spawn_index(
                        self.path.clone(),
                        self.index.end(),
                        self.epoch,
                        self.encoding,
                        self.tx.clone(),
                        self.ctx.clone(),
                    ));
                }
                self.len = len;
            }
        }
        let indexed = self.index.end();
        let complete = self.indexing.is_none();
        if let Some(f) = &mut self.filter
            && f.running.is_none()
            && (indexed > f.scanned.0 || (complete && !f.scanned.1))
            && let Some(first_line) = self.index.line_at(f.done)
        {
            f.scanned = (indexed, complete);
            f.running = Some(spawn_filter(
                self.path.clone(),
                f.done,
                first_line,
                indexed - f.done,
                complete,
                self.encoding,
                f.matcher.clone(),
                self.epoch,
                self.tx.clone(),
                self.ctx.clone(),
            ));
        }
        if self.indexing.is_some() || self.filter.as_ref().is_some_and(|f| f.running.is_some()) {
            self.ctx.request_repaint_after(Duration::from_millis(100));
        } else {
            self.ctx.request_repaint_after(POLL);
        }
    }

    /// Rows shown: all lines, or only matching lines when filtering.
    pub fn rows(&self) -> usize {
        match &self.filter {
            Some(f) => f.lines.len(),
            None => self.index.line_count(),
        }
    }

    /// The display row of document line `line` (with a filter: the first
    /// matching line at or after it).
    pub fn row_of_line(&self, line: usize) -> usize {
        match &self.filter {
            Some(f) => f.lines.partition_point(|&l| l < line),
            None => line,
        }
    }

    /// (line number, text, cut) for display rows.
    pub fn row_lines(&mut self, rows: std::ops::Range<usize>) -> Vec<(usize, String, bool)> {
        let lines: Vec<usize> = match &self.filter {
            Some(f) => f.lines.get(rows).map(<[usize]>::to_vec).unwrap_or_default(),
            None => rows.collect(),
        };
        let mut out = Vec::with_capacity(lines.len());
        for l in lines {
            match logfile::read_lines(
                &mut self.file,
                &self.index,
                l..l + 1,
                MAX_LINE,
                self.encoding,
            ) {
                Ok(mut v) if !v.is_empty() => {
                    let (text, cut) = v.remove(0);
                    out.push((l, text, cut));
                }
                Ok(_) => {}
                Err(e) => {
                    self.error = Some(e.to_string());
                    break;
                }
            }
        }
        out
    }
}
