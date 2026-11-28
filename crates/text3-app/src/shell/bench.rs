//! The workload driver (#206): runs one `TEXT3_BENCH` scenario
//! ([`text3_app::benchprobe`]) through the window's own code paths and
//! writes its numbers.
//!
//! Each frame goes: [`Shell::bench_input`] (eframe's raw input hook,
//! before egui sees the frame's input: typed text and keys are added
//! here, so they reach the editor exactly as keyboard input does), then
//! [`Shell::bench_frame_start`] at the start of `ui` (the previous frame
//! is on screen; the next step is taken), then [`Shell::bench_frame_end`]
//! when `ui` returns (the frame is painted and presented after that).
//!
//! Times: `ui_ms` is from the input hook to the end of `ui` (the app's own
//! work for the frame, before painting); `frame_ms` is from one input
//! hook to the next, which includes painting and presenting the frame,
//! i.e. how long one keystroke or step took to appear on screen.

use std::time::Instant;

use eframe::egui;
use text3_app::Command;
use text3_app::benchprobe::{self, Config, Line, Scenario, ms};
use text3_core::search::Query;

use super::Shell;
use crate::menu::MenuAction;

/// Lines moved per scroll step: about one screen of the default
/// 960×680 pt window at the default font size.
const SCROLL_LINES: usize = 40;
/// Frames to let the view settle before typing or scrolling.
const SETTLE_FRAMES: u32 = 5;

enum Phase {
    /// Until the first frame is on screen.
    Start,
    /// The file was opened at `t0`; waiting for a frame showing it.
    Shown {
        t0: Instant,
        drawn: bool,
    },
    /// The log viewer is indexing (since `t0`).
    Indexing {
        t0: Instant,
    },
    /// A log filter (`regex` or plain) started at `t0`.
    Filter {
        t0: Instant,
        regex: bool,
    },
    /// The find bar is counting matches since `t0`.
    Find {
        t0: Instant,
        regex: bool,
        frames: u32,
    },
    /// Waiting `left` more frames before stepping.
    Settle {
        left: u32,
    },
    /// Typing or scrolling: `done` steps taken.
    Steps {
        done: usize,
    },
    Finished,
}

pub(super) struct Driver {
    cfg: &'static Config,
    started: Instant,
    phase: Phase,
    line: Line,
    /// This frame's input hook.
    frame_t0: Option<Instant>,
    /// Events were added to this frame's input.
    injected: bool,
    /// The previous frame had injected events (its `frame_ms` ends now).
    prev_injected: Option<Instant>,
    ui_ms: Vec<f64>,
    frame_ms: Vec<f64>,
    frames: u64,
    /// Document length and caret line before stepping.
    before: (usize, usize),
}

impl Driver {
    pub(super) fn new(cfg: &'static Config) -> Self {
        let bytes = std::fs::metadata(&cfg.file).map_or(0, |m| m.len());
        Self {
            cfg,
            started: Instant::now(),
            phase: Phase::Start,
            line: Line::new(cfg.scenario)
                .text("file", &cfg.file.display().to_string())
                .int("bytes", bytes),
            frame_t0: None,
            injected: false,
            prev_injected: None,
            ui_ms: Vec::new(),
            frame_ms: Vec::new(),
            frames: 0,
            before: (0, 0),
        }
    }

    fn take_line(&mut self) -> Line {
        std::mem::take(&mut self.line)
    }

    fn input(&mut self, raw: &mut egui::RawInput) {
        let now = Instant::now();
        if let Some(t) = self.prev_injected.take() {
            self.frame_ms.push(ms(now - t));
        }
        self.frame_t0 = Some(now);
        self.injected = false;
        let Phase::Steps { done } = self.phase else {
            return;
        };
        if done >= self.cfg.steps {
            return;
        }
        match self.cfg.scenario {
            Scenario::Type => {
                let c = char::from(b'a' + (done % 26) as u8);
                raw.events.push(egui::Event::Text(c.to_string()));
            }
            Scenario::Scroll => {
                for pressed in std::iter::repeat_n([true, false], SCROLL_LINES).flatten() {
                    raw.events.push(egui::Event::Key {
                        key: egui::Key::ArrowDown,
                        physical_key: None,
                        pressed,
                        repeat: false,
                        modifiers: egui::Modifiers::NONE,
                    });
                }
            }
            _ => return,
        }
        self.injected = true;
    }
}

impl Shell {
    /// eframe's raw input hook: note the frame's start and add the
    /// scenario's input.
    pub(super) fn bench_input(&mut self, raw: &mut egui::RawInput) {
        if let Some(d) = &mut self.bench {
            d.input(raw);
        }
    }

    /// Start of `ui`: the previous frame is on screen. Takes the next step.
    pub(super) fn bench_frame_start(&mut self, ctx: &egui::Context) {
        let Some(mut d) = self.bench.take() else {
            return;
        };
        if d.started.elapsed() > d.cfg.timeout {
            let line = d.take_line().text("error", "timeout");
            finish(d.cfg, line, 1);
        }
        self.bench_step(&mut d, ctx);
        self.bench = Some(d);
    }

    /// End of `ui`; `shown` says whether the frame drew the file.
    pub(super) fn bench_frame_end(&mut self, ctx: &egui::Context) {
        let Some(d) = &mut self.bench else {
            return;
        };
        d.frames += 1;
        if let Phase::Shown { drawn, .. } = &mut d.phase {
            *drawn = self.log.is_some() || self.ws.active().editor.document().len_chars() > 0;
        }
        if d.injected {
            if let Some(t0) = d.frame_t0 {
                d.ui_ms.push(ms(t0.elapsed()));
                d.prev_injected = Some(t0);
            }
            if let Phase::Steps { done } = &mut d.phase {
                *done += 1;
            }
        }
        ctx.request_repaint();
    }

    fn bench_step(&mut self, d: &mut Driver, ctx: &egui::Context) {
        let cfg = d.cfg;
        match d.phase {
            Phase::Start => {
                // The first frame is on screen once the second starts.
                if d.frames < 1 {
                    return;
                }
                let t0 = Instant::now();
                if cfg.scenario == Scenario::Log {
                    // File › Open Log…
                    self.open_log(&cfg.file, ctx);
                } else {
                    // File › Open… (large files go to the log viewer).
                    self.open(&cfg.file);
                }
                let open_ms = ms(t0.elapsed());
                let viewer = if self.log.is_some() { "log" } else { "editor" };
                let opened = self.log.is_some() || self.editor().file().is_some();
                d.line = d
                    .take_line()
                    .text("viewer", viewer)
                    .num("open_call_ms", open_ms);
                if !opened {
                    finish(cfg, d.take_line().text("error", "could not open"), 2);
                }
                let needs_editor = matches!(
                    cfg.scenario,
                    Scenario::Find | Scenario::Type | Scenario::Scroll
                );
                if needs_editor && self.log.is_some() {
                    let line = d.take_line().text("error", "opened in the log viewer");
                    finish(cfg, line, 2);
                }
                d.phase = Phase::Shown { t0, drawn: false };
            }
            Phase::Shown { t0, drawn } => {
                if !drawn {
                    return;
                }
                d.line = d.take_line().num("shown_ms", ms(t0.elapsed()));
                if self.log.is_some() {
                    d.phase = Phase::Indexing { t0 };
                    return self.bench_step(d, ctx);
                }
                let len = self.editor().document().len_chars();
                d.line = d
                    .take_line()
                    .int("chars", len as u64)
                    .int("lines", self.editor().document().rope().len_lines() as u64);
                match cfg.scenario {
                    Scenario::Open => d.phase = Phase::Finished,
                    Scenario::Find => {
                        self.dispatch(MenuAction::Find, ctx);
                        self.bench_find(d, false);
                    }
                    Scenario::Type | Scenario::Scroll => {
                        let at = if cfg.scenario == Scenario::Type {
                            len / 2
                        } else {
                            0
                        };
                        self.editor_mut()
                            .execute(Command::PlaceCaret { at, extend: false });
                        let id = self.ws.active().id;
                        self.active_view(id).reveal();
                        self.focus_active();
                        d.phase = Phase::Settle {
                            left: SETTLE_FRAMES,
                        };
                    }
                    Scenario::Log => d.phase = Phase::Finished,
                }
            }
            Phase::Indexing { t0 } => {
                let Some(log) = &self.log else {
                    return;
                };
                if !log.indexed() {
                    return;
                }
                d.line = d
                    .take_line()
                    .num("indexed_ms", ms(t0.elapsed()))
                    .int("lines", log.line_count() as u64);
                if cfg.scenario == Scenario::Log {
                    self.bench_filter(d, false);
                } else {
                    d.phase = Phase::Finished;
                }
            }
            Phase::Filter { t0, regex } => {
                let Some(log) = &self.log else {
                    return;
                };
                if log.bad_pattern {
                    finish(cfg, d.take_line().text("error", "bad pattern"), 2);
                }
                if !log.filter_finished() {
                    return;
                }
                let key = if regex { "regex" } else { "filter" };
                let n = log.matches().map_or(0, |(n, _)| n);
                d.line = d
                    .take_line()
                    .num(&format!("{key}_ms"), ms(t0.elapsed()))
                    .int(&format!("{key}_matches"), n as u64);
                if !regex && !cfg.regex.is_empty() {
                    self.bench_filter(d, true);
                } else {
                    d.phase = Phase::Finished;
                }
            }
            Phase::Find { t0, regex, frames } => {
                // The search starts in the frame that sets the query and
                // its result is taken in by a later frame.
                if frames == 0 || self.find.searching() {
                    d.phase = Phase::Find {
                        t0,
                        regex,
                        frames: frames + 1,
                    };
                    return;
                }
                if let Some(e) = &self.find.error {
                    let line = d.take_line().text("error", e);
                    finish(cfg, line, 2);
                }
                let key = if regex { "find_regex" } else { "find" };
                d.line = d
                    .take_line()
                    .num(&format!("{key}_ms"), ms(t0.elapsed()))
                    .int(
                        &format!("{key}_matches"),
                        self.find.matches.ranges.len() as u64,
                    )
                    .flag(&format!("{key}_truncated"), self.find.matches.truncated);
                if !regex && !cfg.regex.is_empty() {
                    self.bench_find(d, true);
                } else {
                    d.phase = Phase::Finished;
                }
            }
            Phase::Settle { left } => {
                if left > 0 {
                    d.phase = Phase::Settle { left: left - 1 };
                    return;
                }
                let ed = self.editor();
                let head = ed.selections().primary().head;
                d.before = (
                    ed.document().len_chars(),
                    ed.document().rope().char_to_line(head),
                );
                d.phase = Phase::Steps { done: 0 };
            }
            Phase::Steps { done } => {
                // The last stepped frame is on screen once its frame_ms
                // has been noted.
                if done < cfg.steps || d.frame_ms.len() < d.ui_ms.len() {
                    return;
                }
                let ed = self.editor();
                let head = ed.selections().primary().head;
                let (len0, line0) = d.before;
                let line = d
                    .take_line()
                    .int("steps", done as u64)
                    .stats("ui_ms", &d.ui_ms)
                    .stats("frame_ms", &d.frame_ms);
                d.line = if cfg.scenario == Scenario::Type {
                    line.int(
                        "inserted",
                        ed.document().len_chars().saturating_sub(len0) as u64,
                    )
                } else {
                    let now = ed.document().rope().char_to_line(head);
                    line.int("lines_per_step", SCROLL_LINES as u64)
                        .int("lines_moved", now.saturating_sub(line0) as u64)
                };
                d.phase = Phase::Finished;
            }
            Phase::Finished => {}
        }
        if matches!(d.phase, Phase::Finished) {
            let line = d.take_line().num("total_ms", ms(d.started.elapsed()));
            finish(cfg, line, 0);
        }
    }

    /// Type the query into the log viewer's filter field.
    fn bench_filter(&mut self, d: &mut Driver, regex: bool) {
        let Some(log) = &mut self.log else {
            return;
        };
        log.query = if regex {
            d.cfg.regex.clone()
        } else {
            d.cfg.query.clone()
        };
        log.regex = regex;
        let t0 = Instant::now();
        log.set_query();
        d.phase = Phase::Filter { t0, regex };
    }

    /// Type the query into the find bar.
    fn bench_find(&mut self, d: &mut Driver, regex: bool) {
        self.find.query = Query {
            pattern: if regex {
                d.cfg.regex.clone()
            } else {
                d.cfg.query.clone()
            },
            regex,
            case_sensitive: false,
            whole_word: false,
        };
        d.phase = Phase::Find {
            t0: Instant::now(),
            regex,
            frames: 0,
        };
    }
}

/// Write the results and exit with `code`.
fn finish(cfg: &Config, line: Line, code: i32) -> ! {
    let line = line.finish();
    if let Err(e) = benchprobe::append(&cfg.out, &line) {
        eprintln!("text3: could not write {}: {e}", cfg.out.display());
        std::process::exit(3);
    }
    std::process::exit(code);
}
