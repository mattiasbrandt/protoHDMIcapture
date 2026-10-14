//! Settings: the same file, names and rules as view.sh (decided in #10).
//!
//! The rows come from `lib/settings.sh` itself, read at build time, so the
//! table has one source of truth. Reading mirrors `lib/settings.sh`
//! (`settings_resolve`, `settings_validate`) and `lib/card.sh`
//! (`_config_line`, `CONFIG_FILE`): environment, else the saved settings,
//! else the default; the pins are one setting; a deprecated name translates
//! to its replacement. `tools/check` runs every settings fixture through
//! both viewers and requires the same `--settings` output.

use std::collections::HashMap;
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;

const SETTINGS_SH: &str = include_str!("../../lib/settings.sh");

/// One row: name | default | values | players | flags.
#[derive(Debug, Clone)]
pub struct Row {
    pub name: String,
    pub default: String,
    pub values: String,
    pub flags: String,
}

impl Row {
    fn has_flag(&self, flag: &str) -> bool {
        self.flags.split(',').any(|f| f == flag)
    }

    fn deprecated_for(&self) -> Option<&str> {
        self.flags.split(',').find_map(|f| f.strip_prefix("deprecated="))
    }
}

/// The rows of `_SETTINGS_ROWS` in lib/settings.sh, in order.
pub fn rows() -> Vec<Row> {
    let start = SETTINGS_SH.find("_SETTINGS_ROWS='").expect("lib/settings.sh has _SETTINGS_ROWS") + "_SETTINGS_ROWS='".len();
    let end = start + SETTINGS_SH[start..].find('\'').expect("_SETTINGS_ROWS is closed");
    SETTINGS_SH[start..end]
        .lines()
        // `[ -n "${line// /}" ] && [[ "$line" != '#'* ]]`
        .filter(|l| !l.replace(' ', "").is_empty() && !l.starts_with('#'))
        .map(|l| {
            let mut f = l.splitn(5, '|');
            let mut next = || f.next().unwrap_or("").to_string();
            // The players column says which of view.sh's players honour a
            // setting; the Rust viewer is not one of them.
            let (name, default, values, _players, flags) = (next(), next(), next(), next(), next());
            Row {
                name: name.replace(' ', ""),
                // `def%"${def##*[! ]}"`: trailing spaces only
                default: default.trim_end_matches(' ').to_string(),
                values: values.replace(' ', ""),
                flags: flags.replace(' ', ""),
            }
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum From {
    Env,
    Saved,
    Default,
}

impl From {
    fn word(self) -> &'static str {
        match self {
            From::Env => "env",
            From::Saved => "saved",
            From::Default => "default",
        }
    }
}

pub struct Settings {
    pub rows: Vec<Row>,
    values: HashMap<String, String>,
    from: HashMap<String, From>,
    pub notes: Vec<String>,
    pub config_file: PathBuf,
}

/// `CONFIG_FILE`, else `$XDG_CONFIG_HOME/protohdmicapture/config`, else
/// `~/.config/protohdmicapture/config` (lib/card.sh).
pub fn config_file(env: &dyn Fn(&str) -> Option<String>) -> PathBuf {
    if let Some(f) = env("CONFIG_FILE").filter(|f| !f.is_empty()) {
        return f.into();
    }
    let base = env("XDG_CONFIG_HOME")
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| format!("{}/.config", env("HOME").unwrap_or_default()));
    PathBuf::from(base).join("protohdmicapture/config")
}

/// `_config_line`: Some((key, value)) for a setting, None for a blank line
/// or a comment, Err for anything else.
fn config_line<'a>(line: &'a str, rows: &[Row]) -> Result<Option<(&'a str, &'a str)>, ()> {
    // `${1#"${1%%[![:space:]]*}"}`: leading whitespace only.
    let line = line.trim_start_matches(|c: char| c.is_ascii_whitespace());
    if line.is_empty() || line.starts_with('#') {
        return Ok(None);
    }
    let Some((key, value)) = line.split_once('=') else { return Err(()) };
    if !rows.iter().any(|r| r.name == key) {
        return Err(());
    }
    let unquoted = b"\"'"
        .iter()
        .find(|&&q| value.len() >= 2 && value.as_bytes()[0] == q && value.as_bytes()[value.len() - 1] == q)
        .map_or(value, |_| &value[1..value.len() - 1]);
    Ok(Some((key, unquoted)))
}

impl Settings {
    /// `settings_resolve` with this process's environment.
    pub fn resolve() -> Self {
        Self::resolve_with(&|k| env::var(k).ok())
    }

    pub fn resolve_with(env: &dyn Fn(&str) -> Option<String>) -> Self {
        let rows = rows();
        let config_file = config_file(env);
        let mut values = HashMap::new();
        let mut from = HashMap::new();
        let mut notes = Vec::new();

        // Environment first. An empty value counts only where empty means
        // something (IPC= disables the socket); elsewhere it is unset.
        let mut env_pin = false;
        for r in &rows {
            if let Some(v) = env(&r.name).filter(|v| !v.is_empty() || r.has_flag("empty")) {
                if r.has_flag("pin") && !v.is_empty() {
                    env_pin = true;
                }
                values.insert(r.name.clone(), v);
                from.insert(r.name.clone(), From::Env);
            }
        }

        let mut saved: HashMap<&str, String> = HashMap::new();
        let text = fs::read_to_string(&config_file).unwrap_or_default();
        for line in text.lines() {
            match config_line(line, &rows) {
                Err(()) => notes.push(format!("{}: ignoring unknown line: {line}", config_file.display())),
                Ok(Some((k, v))) => {
                    let k = rows.iter().find(|r| r.name == k).map(|r| r.name.as_str()).unwrap_or_default();
                    saved.insert(k, v.to_string());
                }
                Ok(None) => {}
            }
        }

        for r in &rows {
            if from.contains_key(&r.name) {
                continue;
            }
            let saved_v = saved.get(r.name.as_str());
            // The pins are one setting: a card picked in the environment is
            // not overridden by another pinned in the file.
            if env_pin
                && r.has_flag("pin")
                && let Some(v) = saved_v.filter(|v| !v.is_empty())
            {
                notes.push(format!("a capture card is picked in the environment; the saved {}={v} is not used", r.name));
            }
            let usable = saved_v.filter(|v| !(env_pin && r.has_flag("pin")) && (!v.is_empty() || r.has_flag("empty")));
            match usable {
                Some(v) => {
                    values.insert(r.name.clone(), v.clone());
                    from.insert(r.name.clone(), From::Saved);
                }
                None => {
                    values.insert(r.name.clone(), r.default.clone());
                    from.insert(r.name.clone(), From::Default);
                }
            }
        }

        // An old name translates to its replacement unless that was set itself.
        for r in &rows {
            let Some(new) = r.deprecated_for() else { continue };
            let src = from[&r.name];
            if src == From::Default {
                continue;
            }
            let old_v = values[&r.name].clone();
            if from.get(new) != Some(&From::Default) {
                notes.push(format!("{}={old_v} is ignored because {new}={} is set; remove {}", r.name, values[new], r.name));
                continue;
            }
            let translated = match (r.name.as_str(), old_v.as_str()) {
                ("FULLSCREEN", "yes") => "fullscreen",
                ("FULLSCREEN", "no") => "windowed",
                _ => continue, // an invalid value is reported by validate
            };
            values.insert(new.to_string(), translated.to_string());
            from.insert(new.to_string(), src);
            notes.push(format!("{} is deprecated; use {new}={translated}", r.name));
        }

        Self { rows, values, from, notes, config_file }
    }

    pub fn get(&self, name: &str) -> &str {
        self.values.get(name).map(String::as_str).unwrap_or("")
    }

    pub fn from(&self, name: &str) -> From {
        self.from.get(name).copied().unwrap_or(From::Default)
    }

    /// `settings_validate`: one message per value out of bounds.
    pub fn errors(&self) -> Vec<String> {
        let mut out = Vec::new();
        for r in &self.rows {
            let v = self.get(&r.name);
            if let Err(why) = check(r, v) {
                let src = match self.from(&r.name) {
                    From::Env => "environment".to_string(),
                    From::Saved => self.config_file.display().to_string(),
                    From::Default => "default".to_string(),
                };
                out.push(format!("{}={v} ({src}) is not {why}", r.name));
            }
        }
        out
    }

    /// What `view.sh --settings` prints, and its exit status.
    pub fn report(&self) -> (String, i32) {
        let mut s = String::new();
        for r in &self.rows {
            let v = self.get(&r.name);
            let shown = if v.is_empty() { "\"\"" } else { v };
            let _ = writeln!(s, "{:<16} {:<34} {}", r.name, shown, self.from(&r.name).word());
        }
        for n in &self.notes {
            let _ = writeln!(s, "note: {n}");
        }
        let errors = self.errors();
        if errors.is_empty() {
            s.push_str("all valid\n");
            return (s, 0);
        }
        for e in errors {
            let _ = writeln!(s, "invalid: {e}");
        }
        (s, 1)
    }
}

/// `_settings_check`: is `v` allowed for the row? Err says what is.
fn check(r: &Row, v: &str) -> Result<(), String> {
    let spec = r.values.as_str();
    if v.is_empty() && (r.has_flag("empty") || spec == "text" || r.default.is_empty()) {
        return Ok(());
    }
    if spec == "text" {
        return Ok(());
    }
    if spec == "size" {
        let ok = v.split_once('x').is_some_and(|(w, h)| is_digits(w) && is_digits(h));
        return if ok { Ok(()) } else { Err("WIDTHxHEIGHT".into()) };
    }
    if let Some(members) = spec.strip_prefix("enum:") {
        // Member by member: a substring test would let "mpv,gst" through.
        let members: Vec<&str> = members.split(',').collect();
        return if members.contains(&v) { Ok(()) } else { Err(format!("one of: {}", members.join(" "))) };
    }
    let (kind, range) = spec.split_once(':').unwrap_or((spec, ""));
    let (lo, hi) = range.split_once("..").unwrap_or(("", ""));
    let int = kind == "int";
    // `^-?[0-9]+$`, or `^-?[0-9]+(\.[0-9]+)?$` for num.
    let digits = v.strip_prefix('-').unwrap_or(v);
    let shape = if int {
        is_digits(digits)
    } else {
        match digits.split_once('.') {
            Some((a, b)) => is_digits(a) && is_digits(b),
            None => is_digits(digits),
        }
    };
    let in_range = || -> Option<bool> {
        let (x, lo, hi): (f64, f64, f64) = (v.parse().ok()?, lo.parse().ok()?, hi.parse().ok()?);
        Some(x >= lo && x <= hi)
    };
    if shape && in_range() == Some(true) {
        return Ok(());
    }
    Err(format!("{} from {lo} to {hi}", if int { "a whole number" } else { "a number" }))
}

fn is_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with(config: &str, extra: &[(&str, &str)]) -> Settings {
        let dir = std::env::temp_dir().join(format!("phc-settings-{}-{:p}", std::process::id(), config));
        fs::create_dir_all(&dir).unwrap();
        let f = dir.join("config");
        fs::write(&f, config).unwrap();
        let path = f.display().to_string();
        let extra: Vec<(String, String)> = extra.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        Settings::resolve_with(&move |k| {
            if k == "CONFIG_FILE" {
                return Some(path.clone());
            }
            extra.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())
        })
    }

    #[test]
    fn the_table_is_read_from_lib_settings_sh() {
        let names: Vec<String> = rows().into_iter().map(|r| r.name).collect();
        // `bash -c '. lib/settings.sh; settings_with'`, 2026-10-14. A change
        // to the table shows up here first.
        let expected = "SOURCE CARD_SERIAL CARD_PORT DEV IGNORE_CARDS_RE SRC SINK RES FPS FMT DELAY LAT_MS BACKEND AUDIO \
                        WINDOW WINDOW_SIZE SCALING LEVELS BRIGHTNESS CONTRAST SATURATION GAMMA IPC REFRESH GST_IO_MODE DEBUG FULLSCREEN";
        assert_eq!(names.join(" "), expected);
        let ipc = rows().into_iter().find(|r| r.name == "IPC").unwrap();
        assert_eq!(ipc.default, "/tmp/mpv-protohdmicapture.sock");
        assert!(ipc.has_flag("empty"));
    }

    #[test]
    fn quotes_comments_and_unknown_lines() {
        let s = with("  # comment\n\nDELAY='0.08'\nLEVELS=\"full\"\nNOPE=1\nWINDOW\nSOURCE='ps5\n", &[]);
        assert_eq!(s.get("DELAY"), "0.08");
        assert_eq!(s.get("LEVELS"), "full");
        assert_eq!(s.get("SOURCE"), "'ps5"); // one quote is not a pair
        assert_eq!(s.notes.len(), 2);
        assert!(s.notes[0].ends_with("ignoring unknown line: NOPE=1"));
    }

    #[test]
    fn trailing_spaces_are_part_of_the_value() {
        let s = with("DELAY=0.08 \n", &[]);
        assert_eq!(s.get("DELAY"), "0.08 ");
        assert_eq!(s.errors().len(), 1);
    }

    #[test]
    fn numbers_are_checked_like_awk() {
        let s = with("FPS=59.94\nDELAY=1\nBRIGHTNESS=-100\n", &[]);
        assert!(s.errors().is_empty(), "{:?}", s.errors());
        let s = with("FPS=1e2\nBRIGHTNESS=5.5\n", &[]);
        assert_eq!(s.errors().len(), 2);
    }
}
