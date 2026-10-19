//! Settings: the same file, names and rules as protoHDMIcapture legacy
//! (decided in #10).
//!
//! The rows are `settings.rows` at the repo root, compiled in, the one
//! table (#47): protoHDMIcapture legacy reads the same file at run time.
//! Reading mirrors legacy's `legacy/lib/settings.sh` (`settings_resolve`,
//! `settings_validate`) and `legacy/lib/card.sh` (`_config_line`, `CONFIG_FILE`):
//! environment, else the saved settings, else the default; the pins are one
//! setting; a deprecated name translates to its replacement. `tools/check`
//! runs every settings fixture through `--settings`.

use std::collections::HashMap;
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const SETTINGS_ROWS: &str = include_str!("../settings.rows");

/// One row: name | default | values | players | flags.
#[derive(Debug, Clone)]
pub struct Row {
    pub name: String,
    pub default: String,
    pub values: String,
    pub flags: String,
}

impl Row {
    pub fn has_flag(&self, flag: &str) -> bool {
        self.flags.split(',').any(|f| f == flag)
    }

    fn deprecated_for(&self) -> Option<&str> {
        self.flags.split(',').find_map(|f| f.strip_prefix("deprecated="))
    }
}

/// The rows of settings.rows, in order.
pub fn rows() -> Vec<Row> {
    SETTINGS_ROWS
        .lines()
        // `[ -n "${line// /}" ] && [[ "$line" != '#'* ]]`
        .filter(|l| !l.replace(' ', "").is_empty() && !l.starts_with('#'))
        .map(|l| {
            let mut f = l.splitn(5, '|');
            let mut next = || f.next().unwrap_or("").to_string();
            // The players column says which of legacy's players honour a
            // setting; protoHDMIcapture is not one of them.
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
    /// A menu launch's default (`with_menu_defaults`).
    Menu,
    Default,
}

impl From {
    /// As `--settings` and the debug log's settings line spell it.
    pub fn word(self) -> &'static str {
        match self {
            From::Env => "env",
            From::Saved => "saved",
            From::Menu => "menu",
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
/// `~/.config/protohdmicapture/config` (legacy/lib/card.sh); on Windows,
/// `%APPDATA%\protohdmicapture\config` (#18).
pub fn config_file(env: &dyn Fn(&str) -> Option<String>) -> PathBuf {
    if let Some(f) = env("CONFIG_FILE").filter(|f| !f.is_empty()) {
        return f.into();
    }
    #[cfg(windows)]
    if let Some(d) = env("APPDATA").filter(|d| !d.is_empty()) {
        return PathBuf::from(d).join("protohdmicapture").join("config");
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
        // No file: the defaults. A file that cannot be read says so.
        let text = match fs::read_to_string(&config_file) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
            Err(e) => {
                notes.push(format!("{}: cannot read it ({e}); the defaults are used", config_file.display()));
                String::new()
            }
        };
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

    /// A menu launch's defaults (launch.sh): REFRESH=match unless the
    /// environment or the settings file sets REFRESH, since a menu has
    /// nowhere to type it. A 60 fps feed on a faster fixed-refresh panel
    /// judders unless the panel drops to its nearest mode.
    pub fn with_menu_defaults(mut self) -> Self {
        if self.from("REFRESH") == From::Default {
            self.values.insert("REFRESH".into(), "match".into());
            self.from.insert("REFRESH".into(), From::Menu);
        }
        self
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
                    From::Menu => "menu default".to_string(),
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

/// `_config_quote`: plain when it is safe, else in single quotes.
fn quote(v: &str) -> String {
    if v.bytes().all(|b| b.is_ascii_alphanumeric() || b"._/:@+-".contains(&b)) {
        v.to_string()
    } else {
        format!("'{v}'")
    }
}

/// The file's lines as `read -r` sees them: split on newlines only (a
/// carriage return stays part of the line), the last line kept even without
/// a final newline.
fn file_lines(text: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = text.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    lines
}

/// `config_write`: sets the keys in the settings file in one pass. Comments,
/// blank and unknown lines stay where they are; a key already present is
/// replaced in place (duplicates dropped); a new key is appended. Written to
/// a temp file (mode 600, as mktemp makes it) and renamed, so a crash never
/// leaves half a file.
pub fn write(config_file: &Path, pairs: &[(&str, &str)]) -> Result<(), String> {
    for (k, v) in pairs {
        if v.contains('\'') {
            return Err(format!("{k}: values cannot contain a single quote"));
        }
    }
    rewrite(config_file, pairs, false)
}

/// `config_unset`: removes the keys (every line of each) in the same one
/// pass as `write`. With no file yet, it writes one holding only the
/// header, as `config_unset` does.
pub fn unset(config_file: &Path, keys: &[&str]) -> Result<(), String> {
    let pairs: Vec<(&str, &str)> = keys.iter().map(|k| (*k, "")).collect();
    rewrite(config_file, &pairs, true)
}

/// `_config_rewrite`: `set` (write) or `unset` mode.
fn rewrite(config_file: &Path, pairs: &[(&str, &str)], unset: bool) -> Result<(), String> {
    let rows = rows();
    let want = |k: &str| pairs.iter().rev().find(|(n, _)| *n == k).map(|(_, v)| *v);
    let dir = config_file.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;

    // NotFound starts a new file; any other read error stops, so a file we
    // cannot read is never replaced by one holding only the header.
    let text = match fs::read_to_string(config_file) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("cannot read {}: {e}", config_file.display())),
    };
    let mut out = String::new();
    let mut done: Vec<&str> = Vec::new();
    match &text {
        Some(text) => {
            for line in file_lines(text) {
                if let Ok(Some((k, _))) = config_line(line, &rows)
                    && let Some(v) = want(k)
                {
                    if !done.contains(&k) {
                        done.push(k);
                        if !unset {
                            let _ = writeln!(out, "{k}={}", quote(v));
                        }
                    }
                    continue;
                }
                out.push_str(line);
                out.push('\n');
            }
        }
        None => {
            out.push_str("# protoHDMIcapture settings, written by the setup wizard.\n");
            out.push_str("# One KEY=value per line; see config.example in the repo.\n");
        }
    }
    if !unset {
        for (k, _) in pairs {
            if !done.contains(k) {
                done.push(k);
                let _ = writeln!(out, "{k}={}", quote(want(k).unwrap_or_default()));
            }
        }
    }

    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        config_file.file_name().map(|n| n.to_string_lossy()).unwrap_or_default(),
        std::process::id()
    ));
    let written = (|| {
        use std::io::Write as _;
        let mut open = fs::OpenOptions::new();
        open.write(true).create_new(true);
        // Mode 600, as mktemp makes it; on Windows the profile's own ACL
        // keeps the file the user's.
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut open, 0o600);
        let mut f = open.open(&tmp)?;
        f.write_all(out.as_bytes())?;
        f.sync_all()?;
        fs::rename(&tmp, config_file)
    })();
    written.map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("cannot replace {}: {e}", config_file.display())
    })
}

/// `config_read`: the file's own values (the last line of a key wins),
/// without the environment or the defaults, so a caller can tell "set in
/// the file" from "a default". No file: none. Err: it exists but cannot
/// be read.
pub fn read_file(config_file: &Path) -> Result<HashMap<String, String>, String> {
    let rows = rows();
    let text = match fs::read_to_string(config_file) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("cannot read {}: {e}", config_file.display())),
    };
    Ok(file_lines(&text)
        .into_iter()
        .filter_map(|l| config_line(l, &rows).ok().flatten())
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect())
}

/// Is `v` a valid value for the setting `name` (settings.rows' ranges)?
pub fn check_value(name: &str, v: &str) -> Result<(), String> {
    let rows = rows();
    let r = rows.iter().find(|r| r.name == name).ok_or(format!("{name} is not a setting"))?;
    check(r, v).map_err(|e| format!("{name}={v}: {e}"))
}

/// The names carrying `flag`, in settings.rows' order (`settings_with`).
pub fn with_flag(flag: &str) -> Vec<String> {
    rows().into_iter().filter(|r| r.has_flag(flag)).map(|r| r.name).collect()
}

impl Settings {
    /// view.sh --save: only the settings flagged `save` (the overlay's w).
    pub fn save(&self, pairs: &[(&str, &str)]) -> Result<(), String> {
        for (k, _) in pairs {
            if !self.rows.iter().any(|r| r.name == *k && r.has_flag("save")) {
                return Err(format!("{k} is not a setting the overlay saves"));
            }
        }
        write(&self.config_file, pairs)
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
        // as_ptr: a &str's {:p} carries its length after a ':' (no file name on Windows).
        let dir = std::env::temp_dir().join(format!("phc-settings-{}-{:p}", std::process::id(), config.as_ptr()));
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
    fn the_table_is_read_from_settings_rows() {
        let names: Vec<String> = rows().into_iter().map(|r| r.name).collect();
        // settings.rows' names, in order (as `settings_with` lists them on
        // 2026-10-14). A change to the table shows up here first.
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
    fn a_menu_launch_matches_the_refresh_unless_it_is_set() {
        // Nothing sets it: the menu's default, said as such.
        let s = with("LEVELS=full\n", &[]).with_menu_defaults();
        assert_eq!((s.get("REFRESH"), s.from("REFRESH")), ("match", From::Menu));
        assert!(s.report().0.lines().any(|l| l.starts_with("REFRESH") && l.ends_with(" menu")), "{}", s.report().0);
        // The file sets it: the file wins, as launch.sh leaves it alone.
        let s = with("REFRESH=keep\n", &[]).with_menu_defaults();
        assert_eq!((s.get("REFRESH"), s.from("REFRESH")), ("keep", From::Saved));
        // The environment sets it: the environment wins.
        let s = with("REFRESH=match\nLEVELS=limited\n", &[("REFRESH", "keep")]).with_menu_defaults();
        assert_eq!((s.get("REFRESH"), s.from("REFRESH")), ("keep", From::Env));
        // An empty one sets nothing (`[ -n "${REFRESH:-}" ]`).
        let s = with("REFRESH=\n", &[("REFRESH", "")]).with_menu_defaults();
        assert_eq!((s.get("REFRESH"), s.from("REFRESH")), ("match", From::Menu));
        // Without a menu launch, the table's default.
        let s = with("SCALING=fast\n", &[]);
        assert_eq!((s.get("REFRESH"), s.from("REFRESH")), ("keep", From::Default));
    }

    fn rust_write(start: Option<&str>, args: &[&str]) -> (Option<String>, bool) {
        let dir = std::env::temp_dir().join(format!("phc-rust-{}-{}", std::process::id(), args.join("_").len()));
        fs::create_dir_all(&dir).unwrap();
        let f = dir.join("config");
        let _ = fs::remove_file(&f);
        if let Some(t) = start {
            fs::write(&f, t).unwrap();
        }
        let pairs: Vec<(&str, &str)> = args.iter().map(|a| a.split_once('=').unwrap()).collect();
        let ok = write(&f, &pairs).is_ok();
        let out = fs::read_to_string(&f).ok();
        fs::remove_dir_all(&dir).unwrap();
        (out, ok)
    }

    /// What `write` makes of `start` (None: no file), and whether it wrote.
    #[test]
    fn writes_byte_for_byte_as_legacys_config_write() {
        // The bytes protoHDMIcapture legacy's `config_write` (legacy/lib/card.sh)
        // wrote for each case on 2026-10-19 (#47): legacy reads this file.
        let start = "# my settings\n  DELAY=0.05\nNOPE=1\n\nLEVELS=auto\nLEVELS=full\nWINDOW=fullscreen\r\nSOURCE='ps5'";
        let header = "# protoHDMIcapture settings, written by the setup wizard.\n# One KEY=value per line; see config.example in the repo.\n";
        let cases: &[(Option<&str>, &[&str], String, bool)] = &[
            (None, &["DELAY=0.08"], format!("{header}DELAY=0.08\n"), true),
            (
                Some(start),
                &["LEVELS=limited", "DELAY=0.07", "BRIGHTNESS=-5", "SINK=a b"],
                "# my settings\nDELAY=0.07\nNOPE=1\n\nLEVELS=limited\nWINDOW=fullscreen\r\nSOURCE='ps5'\nBRIGHTNESS=-5\nSINK='a b'\n".into(),
                true,
            ),
            (
                Some(start),
                &["WINDOW=windowed", "SOURCE=ps5"],
                "# my settings\n  DELAY=0.05\nNOPE=1\n\nLEVELS=auto\nLEVELS=full\nWINDOW=windowed\nSOURCE=ps5\n".into(),
                true,
            ),
            (Some(""), &["GAMMA=3", "GAMMA=4"], "GAMMA=4\n".into(), true),
            // A value the file cannot quote: refused, the file untouched.
            (Some(start), &["SOURCE=it's"], start.into(), false),
        ];
        for (start, args, want, ok) in cases {
            assert_eq!(rust_write(*start, args), (Some(want.clone()), *ok), "start {start:?}, args {args:?}");
        }
    }

    #[test]
    fn unsets_byte_for_byte_as_legacys_config_unset() {
        // The bytes legacy's `config_unset` (legacy/lib/card.sh) left on
        // 2026-10-19 (#47).
        let start = "# my settings\n  DELAY=0.05\nNOPE=1\n\nFULLSCREEN=yes\nLEVELS=full\nFULLSCREEN=no\r\nSOURCE='ps5'";
        let header = "# protoHDMIcapture settings, written by the setup wizard.\n# One KEY=value per line; see config.example in the repo.\n";
        let cases: &[(Option<&str>, &[&str], &str)] = &[
            (None, &["DEV"], header),
            (Some(start), &["FULLSCREEN", "SOURCE"], "# my settings\n  DELAY=0.05\nNOPE=1\n\nLEVELS=full\n"),
            (Some(start), &[], "# my settings\n  DELAY=0.05\nNOPE=1\n\nFULLSCREEN=yes\nLEVELS=full\nFULLSCREEN=no\r\nSOURCE='ps5'\n"),
            (Some(""), &["GAMMA"], ""),
        ];
        for (start, keys, want) in cases {
            let dir = std::env::temp_dir().join(format!("phc-unset-{}-{}", std::process::id(), keys.join("_").len() + want.len()));
            fs::create_dir_all(&dir).unwrap();
            let f = dir.join("config");
            if let Some(t) = start {
                fs::write(&f, t).unwrap();
            }
            unset(&f, keys).unwrap();
            assert_eq!(fs::read_to_string(&f).ok().as_deref(), Some(*want), "start {start:?}, keys {keys:?}");
            fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[test]
    fn read_file_and_with_flag() {
        // As legacy's `settings_with wizard` listed them on 2026-10-19.
        let wizard = "SOURCE CARD_SERIAL CARD_PORT DEV IGNORE_CARDS_RE SRC RES FPS FMT DELAY BACKEND AUDIO WINDOW WINDOW_SIZE SCALING \
                      REFRESH DEBUG FULLSCREEN";
        assert_eq!(with_flag("wizard").join(" "), wizard);
        let dir = std::env::temp_dir().join(format!("phc-read-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let f = dir.join("config");
        fs::write(&f, "# c\nDELAY=0.05\nNOPE=1\nDELAY='0.07'\nSOURCE=\n").unwrap();
        let v = read_file(&f).unwrap();
        assert_eq!(v.get("DELAY").map(String::as_str), Some("0.07"), "the last line wins");
        assert_eq!(v.get("SOURCE").map(String::as_str), Some(""));
        assert_eq!(v.len(), 2);
        assert!(read_file(&dir.join("none")).unwrap().is_empty());
        // There but unreadable (a directory here): an error, not "empty".
        assert!(read_file(&dir).unwrap_err().starts_with("cannot read"));
        assert!(check_value("DELAY", "0.07").is_ok() && check_value("DELAY", "abc").is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn save_writes_only_save_flagged_settings_mode_600() {
        use std::os::unix::fs::PermissionsExt;
        let s = with("# mine\nLEVELS=auto\n", &[]);
        assert_eq!(s.save(&[("SOURCE", "ps5")]).unwrap_err(), "SOURCE is not a setting the overlay saves");
        s.save(&[("LEVELS", "full"), ("DELAY", "0.07")]).unwrap();
        assert_eq!(fs::read_to_string(&s.config_file).unwrap(), "# mine\nLEVELS=full\nDELAY=0.07\n");
        assert_eq!(fs::metadata(&s.config_file).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn numbers_are_checked_like_awk() {
        let s = with("FPS=59.94\nDELAY=1\nBRIGHTNESS=-100\n", &[]);
        assert!(s.errors().is_empty(), "{:?}", s.errors());
        let s = with("FPS=1e2\nBRIGHTNESS=5.5\n", &[]);
        assert_eq!(s.errors().len(), 2);
    }
}
