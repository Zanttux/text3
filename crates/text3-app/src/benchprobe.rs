//! Workload measurement (#206, docs/PERFORMANCE.md "Large files",
//! scripts/bench/workloads.py).
//!
//! With `TEXT3_BENCH=<scenario>`, `TEXT3_BENCH_FILE=<file>` and
//! `TEXT3_BENCH_OUT=<results>` set, Text³ runs one scripted scenario once
//! the first frame is on screen, through the same functions the window
//! uses (opening a file, the find bar, the log viewer's filter, key and
//! text events delivered to the editor as egui input), appends one JSON
//! line with the results to `<results>` and exits. The driver itself is
//! `shell/bench.rs`; this module reads the configuration, keeps the
//! numbers and writes them.
//!
//! Without `TEXT3_BENCH` nothing is read after the first call to
//! [`config`] (a cached `None`) and the window behaves as always.
//!
//! Scenarios:
//! - `open`: open the file (an editor tab, or the log viewer from
//!   256 MB up) and wait for the first frame showing it.
//! - `log`: open the file in the log viewer, wait until it is indexed,
//!   then filter for `TEXT3_BENCH_QUERY` (plain) and `TEXT3_BENCH_REGEX`
//!   (regular expression), each until the filter has finished.
//! - `find`: open the file in an editor tab, open the find bar and count
//!   every match of `TEXT3_BENCH_QUERY`, then of `TEXT3_BENCH_REGEX`.
//! - `type`: open the file, put the caret in the middle and type
//!   `TEXT3_BENCH_STEPS` characters, one per frame.
//! - `scroll`: open the file and move down one screen per frame
//!   (`TEXT3_BENCH_STEPS` times) with the Down arrow key.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

/// Which workload to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scenario {
    Open,
    Log,
    Find,
    Type,
    Scroll,
}

impl Scenario {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "open" => Self::Open,
            "log" => Self::Log,
            "find" => Self::Find,
            "type" => Self::Type,
            "scroll" => Self::Scroll,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Log => "log",
            Self::Find => "find",
            Self::Type => "type",
            Self::Scroll => "scroll",
        }
    }
}

/// What to run, from the environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub scenario: Scenario,
    pub file: PathBuf,
    pub out: PathBuf,
    /// Plain search text (`find`, `log`).
    pub query: String,
    /// Regular expression (`find`, `log`); empty skips that part.
    pub regex: String,
    /// Frames typed or scrolled (`type`, `scroll`).
    pub steps: usize,
    /// Give up (and exit with an error) after this long.
    pub timeout: Duration,
}

static CONFIG: OnceLock<Option<Config>> = OnceLock::new();

/// The scenario to run, if `TEXT3_BENCH` is set (read once).
pub fn config() -> Option<&'static Config> {
    CONFIG
        .get_or_init(|| from_env(crate::migrate::var))
        .as_ref()
}

/// Build the configuration from `get` (variable names without the
/// `TEXT3_` prefix). Unknown scenarios and missing paths are reported on
/// stderr and leave measuring off.
pub fn from_env(get: impl Fn(&str) -> Option<String>) -> Option<Config> {
    let name = get("BENCH")?;
    let Some(scenario) = Scenario::parse(&name) else {
        eprintln!("text3: unknown TEXT3_BENCH scenario {name:?}");
        return None;
    };
    let (Some(file), Some(out)) = (get("BENCH_FILE"), get("BENCH_OUT")) else {
        eprintln!("text3: TEXT3_BENCH needs TEXT3_BENCH_FILE and TEXT3_BENCH_OUT");
        return None;
    };
    Some(Config {
        scenario,
        file: PathBuf::from(file),
        out: PathBuf::from(out),
        query: get("BENCH_QUERY").unwrap_or_else(|| "ERROR".into()),
        regex: get("BENCH_REGEX").unwrap_or_default(),
        steps: get("BENCH_STEPS")
            .and_then(|s| s.parse().ok())
            .unwrap_or(200),
        timeout: Duration::from_secs(
            get("BENCH_TIMEOUT")
                .and_then(|s| s.parse().ok())
                .unwrap_or(600),
        ),
    })
}

/// Median, 95th percentile (nearest rank) and maximum.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stats {
    pub p50: f64,
    pub p95: f64,
    pub max: f64,
}

pub fn stats(values: &[f64]) -> Option<Stats> {
    if values.is_empty() {
        return None;
    }
    let mut s = values.to_vec();
    s.sort_by(f64::total_cmp);
    let rank = |q: f64| s[((q * s.len() as f64).ceil() as usize).clamp(1, s.len()) - 1];
    Some(Stats {
        p50: rank(0.5),
        p95: rank(0.95),
        max: s[s.len() - 1],
    })
}

/// Milliseconds, for results.
pub fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// One JSON object written as a single line.
#[derive(Debug, Default)]
pub struct Line(String);

impl Line {
    pub fn new(scenario: Scenario) -> Self {
        let mut l = Self(String::from("{"));
        l.push_key("scenario");
        l.push_str(scenario.name());
        l
    }

    fn push_key(&mut self, key: &str) {
        if self.0.len() > 1 {
            self.0.push(',');
        }
        self.push_str(key);
        self.0.push(':');
    }

    fn push_str(&mut self, s: &str) {
        self.0.push('"');
        for c in s.chars() {
            match c {
                '"' => self.0.push_str("\\\""),
                '\\' => self.0.push_str("\\\\"),
                c if (c as u32) < 0x20 => self.0.push_str(&format!("\\u{:04x}", c as u32)),
                c => self.0.push(c),
            }
        }
        self.0.push('"');
    }

    pub fn text(mut self, key: &str, value: &str) -> Self {
        self.push_key(key);
        self.push_str(value);
        self
    }

    pub fn int(mut self, key: &str, value: u64) -> Self {
        self.push_key(key);
        self.0.push_str(&value.to_string());
        self
    }

    pub fn flag(mut self, key: &str, value: bool) -> Self {
        self.push_key(key);
        self.0.push_str(if value { "true" } else { "false" });
        self
    }

    /// A number with microsecond precision; NaN and infinity become null.
    pub fn num(mut self, key: &str, value: f64) -> Self {
        self.push_key(key);
        if value.is_finite() {
            self.0.push_str(&format!("{value:.3}"));
        } else {
            self.0.push_str("null");
        }
        self
    }

    /// `<key>_p50`, `<key>_p95` and `<key>_max`.
    pub fn stats(self, key: &str, values: &[f64]) -> Self {
        match stats(values) {
            Some(s) => self
                .num(&format!("{key}_p50"), s.p50)
                .num(&format!("{key}_p95"), s.p95)
                .num(&format!("{key}_max"), s.max),
            None => self,
        }
    }

    pub fn finish(mut self) -> String {
        self.0.push('}');
        self.0
    }
}

/// Append `line` and a newline to `file`.
pub fn append(file: &Path, line: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file)?;
    writeln!(f, "{line}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |k| m.get(k).cloned()
    }

    #[test]
    fn off_without_the_variable() {
        assert_eq!(from_env(env(&[])), None);
        // The test environment never sets TEXT3_BENCH.
        assert!(config().is_none());
    }

    #[test]
    fn needs_a_known_scenario_and_both_paths() {
        assert_eq!(
            from_env(env(&[
                ("BENCH", "fly"),
                ("BENCH_FILE", "a"),
                ("BENCH_OUT", "b")
            ])),
            None
        );
        assert_eq!(
            from_env(env(&[("BENCH", "find"), ("BENCH_FILE", "a")])),
            None
        );
        let c = from_env(env(&[
            ("BENCH", "type"),
            ("BENCH_FILE", "a.txt"),
            ("BENCH_OUT", "r.jsonl"),
            ("BENCH_STEPS", "50"),
        ]))
        .unwrap();
        assert_eq!(c.scenario, Scenario::Type);
        assert_eq!(c.file, PathBuf::from("a.txt"));
        assert_eq!(c.steps, 50);
        assert_eq!(c.query, "ERROR");
        assert_eq!(c.regex, "");
        assert_eq!(c.timeout, Duration::from_secs(600));
    }

    #[test]
    fn nearest_rank_percentiles() {
        let v: Vec<f64> = (1..=100).map(f64::from).collect();
        let s = stats(&v).unwrap();
        assert_eq!((s.p50, s.p95, s.max), (50.0, 95.0, 100.0));
        let one = stats(&[3.0]).unwrap();
        assert_eq!((one.p50, one.p95, one.max), (3.0, 3.0, 3.0));
        assert!(stats(&[]).is_none());
    }

    #[test]
    fn line_is_one_json_object() {
        let l = Line::new(Scenario::Find)
            .text("file", "C:\\a \"b\"\n")
            .int("matches", 3)
            .flag("truncated", false)
            .num("find_ms", 1.23456)
            .num("bad", f64::NAN)
            .stats("ui_ms", &[1.0, 2.0])
            .finish();
        assert_eq!(
            l,
            "{\"scenario\":\"find\",\"file\":\"C:\\\\a \\\"b\\\"\\u000a\",\"matches\":3,\
             \"truncated\":false,\"find_ms\":1.235,\"bad\":null,\"ui_ms_p50\":1.000,\
             \"ui_ms_p95\":2.000,\"ui_ms_max\":2.000}"
        );
    }

    #[test]
    fn appends_lines() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("r.jsonl");
        append(&f, "{}").unwrap();
        append(&f, "{\"a\":1}").unwrap();
        assert_eq!(std::fs::read_to_string(f).unwrap(), "{}\n{\"a\":1}\n");
    }
}
