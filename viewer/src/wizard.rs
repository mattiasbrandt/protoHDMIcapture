//! The setup wizard inside the viewer window (#13): lib/setup.sh's stages
//! and wording, drawn as an overlay page (osd.rs, egui) over the window,
//! with no terminal. A state machine over what it is told about the
//! machine (`System`), so the stages run in tests on made-up cards.
//!
//! Two windows, one after the other (`Phase`): the stages up to the
//! picture format run in a window without a feed (the card is not chosen
//! yet); from the signal check on, the window shows the chosen card's feed,
//! the audio sync tunes while it plays, and the review saves. main.rs opens
//! the window each phase needs.
//!
//! Like lib/setup.sh it decides only the settings flagged `wizard`; every
//! other line of the file is carried over. Each stage starts on the current
//! value, so Enter keeps it, and nothing is written until the review. Not
//! carried over from lib/setup.sh: the Markdown diagnostics report (it is
//! made of shell tools: lsusb, v4l2-ctl, journalctl), the preflight check
//! (mpv, pw-loopback and ffmpeg are view.sh's, not this viewer's), the
//! "focused monitor now" line, and the test feed (the feed starts from the
//! last screen instead).

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use regex::Regex;

use crate::capture::{Format, FrameSource, Pixels, Raw, Reason, Signal, Stream};
use crate::capture_cards::{self, Known};
use crate::card::{AudioInput, Node};
pub use crate::card::{Mode, Usb};
use crate::live_changes;
use crate::overlay::wrap;
use crate::settings;
use crate::sources;

/// The signal check: frames counted for this many seconds' worth.
const MEASURE_SECONDS: u32 = 2;
/// No frames at all this long into the signal check: "no signal" (lib/setup.sh
/// bounds its capture with `timeout 6`).
pub const SIGNAL_TIMEOUT: Duration = Duration::from_secs(6);
/// Frames the picture check looks at (ffmpeg's `-frames:v 5`).
const PICTURE_FRAMES: usize = 5;
/// The width the page's text wraps at, in characters.
const WIDTH: usize = 72;
/// The card's details at most, in lines, so its page fits a 720-line
/// window at the overlay's font size (window height / 40).
const MAX_USB_LINES: usize = 15;
/// A whole page at most, in lines, for a 720-line window: osd.rs draws
/// pages from 3.5 font sizes down, with a font size of padding above and
/// below, at 1.3 font sizes a line, and the font is 720 / 40 = 18 px:
/// (720 - 63 - 36) / 23 = 27, less one for rounding.
const PAGE_LINES: usize = 26;

/// Which window a stage needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// No feed: the card and its mode are being chosen.
    Choose,
    /// The chosen card's feed.
    Feed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Welcome,
    Card,
    Source,
    Format,
    Signal,
    Audio,
    Display,
    Refresh,
    Player,
    Sync,
    Debug,
    Review,
    Done,
}

impl Stage {
    fn title(self) -> &'static str {
        match self {
            Stage::Welcome => "Welcome",
            Stage::Card => "Capture card",
            Stage::Source => "Source",
            Stage::Format => "Picture format",
            Stage::Signal => "Signal check",
            Stage::Audio => "Audio",
            Stage::Display => "Display mode",
            Stage::Refresh => "Refresh rate",
            Stage::Player => "Player",
            Stage::Sync => "Audio sync",
            Stage::Debug => "Troubleshooting",
            Stage::Review => "Review",
            Stage::Done => "Done",
        }
    }

    /// The window the stage needs; None: whichever is open.
    fn phase(self) -> Option<Phase> {
        match self {
            Stage::Welcome | Stage::Card | Stage::Source | Stage::Format => Some(Phase::Choose),
            Stage::Done => None,
            _ => Some(Phase::Feed),
        }
    }
}

/// How the wizard ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Start the feed with the saved settings.
    Play,
    Close,
}

impl Mode {
    fn size(&self) -> String {
        format!("{}x{}", self.width, self.height)
    }

    /// "fmt WxH fps": the choice's key, as lib/setup.sh keys it.
    fn key(&self) -> String {
        format!("{} {} {}", self.fmt, self.size(), self.fps)
    }
}

/// The audio inputs the audio stage offers.
#[derive(Debug, Clone, Default)]
pub struct AudioInputs {
    /// The card's own input, found on its USB device: shown by its name,
    /// opened by its id.
    pub card: Option<AudioInput>,
    /// Every input (no monitors).
    pub all: Vec<String>,
}

/// The audio sync stage's loopback, playing while it tunes.
pub trait Tuner {
    fn delay_ms(&self) -> u32;
    /// The delay moved by `by_ms` within DELAY's range; the new delay.
    fn nudge(&self, by_ms: i32) -> u32;
}

/// What the wizard asks of the machine.
pub trait System {
    /// Every video capture node, as `card_list`.
    fn cards(&self) -> Vec<Node>;
    /// The node's USB device, None when it is not on USB.
    fn usb(&self, node: &Node) -> Option<Usb>;
    /// The modes the node lists, in its own order.
    fn modes(&self, node: &Node) -> Vec<Mode>;
    fn audio_inputs(&self, node: &Node) -> Result<AudioInputs, String>;
    /// Whether this desktop can match the refresh rate (`desktop_can
    /// refresh_match`).
    fn can_match_refresh(&self) -> bool;
    /// The loopback from `source`, held back by `delay_ms`.
    fn start_audio(&self, source: &str, delay_ms: u32) -> Result<Box<dyn Tuner>, String>;
    /// How a node is named on the pages: its path (Windows shortens its
    /// device links, which run to two lines).
    fn label(&self, node: &Node) -> String {
        node.path.display().to_string()
    }
    /// view.sh runs here too (Linux): its player and its commands are named
    /// on the pages. Windows has no view.sh.
    fn has_view_sh(&self) -> bool {
        true
    }
}

/// A key, as the wizard reads them (app.rs maps the toolkit's).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    Left,
    Right,
    /// With shift: the tuner's 50 ms steps.
    ShiftLeft,
    ShiftRight,
    Enter,
    Escape,
    Backspace,
    Char(String),
}

#[derive(Debug, Clone)]
struct Item {
    key: String,
    label: String,
    tag: Option<String>,
}

fn item(key: &str, label: &str) -> Item {
    Item { key: key.into(), label: label.into(), tag: None }
}

fn tagged(key: &str, label: &str, tag: &str) -> Item {
    Item { key: key.into(), label: label.into(), tag: Some(tag.into()) }
}

/// What the screen asks.
#[derive(Debug, Clone)]
enum Ask {
    /// Enter continues.
    Continue,
    /// One of the items; Enter takes the selected one.
    Choose { prompt: String, items: Vec<Item>, default: Option<usize>, default_label: &'static str, sel: usize },
    /// A typed value; Enter on nothing takes the default.
    Input { prompt: String, value: String, default: String, valid: fn(&str) -> bool, invalid: Option<String> },
    /// The signal check is measuring.
    Wait,
    /// The audio sync tuner.
    Tune,
}

/// Which question of a stage is on screen.
#[derive(Debug, Clone, PartialEq)]
enum Step {
    Welcome,
    CardNone,
    CardPick,
    /// Ignore this other device?
    CardIgnore(usize),
    CardResult,
    CardDetails,
    SourcePick,
    SourceTips,
    FormatPick,
    FormatNone,
    FormatRes,
    FormatFps,
    FormatFmt,
    SignalWait,
    SignalRetry,
    SignalResult,
    AudioPick,
    DisplayPick,
    DisplaySize,
    DisplayCustom,
    RefreshPick,
    PlayerSwitch,
    PlayerScaling,
    SyncNone,
    SyncAsk,
    SyncTune,
    SyncResult,
    DebugPick,
    ReviewAsk,
    ReviewResult,
    Done,
}

/// The signal check's count, from the first frame it sees.
#[derive(Debug, Default)]
struct Measure {
    started: Option<Instant>,
    first: Option<Instant>,
    frames: u32,
    done_at: Option<Instant>,
    /// Luma minimum, maximum and the largest frame mean, over the frames
    /// looked at; None until one was.
    picture: Option<(u8, u8, f64)>,
    looked_at: usize,
    /// Why the picture could not be checked.
    unchecked: Option<String>,
    /// Why there is no picture, from the frame policy's latest state.
    reason: Option<Reason>,
}

/// The card stage's ignore questions: the pattern so far and what was said.
#[derive(Debug, Default)]
struct Ignoring {
    pattern: String,
    nodes: Vec<Node>,
    pick: usize,
}

pub struct Wizard {
    sys: Box<dyn System>,
    pub config_file: PathBuf,
    first_run: bool,
    /// The settings the wizard decides, in lib/settings.sh's order.
    managed: Vec<String>,
    defaults: HashMap<String, String>,
    /// What the file says now.
    cur: HashMap<String, String>,
    /// What it will say: "" is "not in the file".
    new: HashMap<String, String>,
    env_overrides: Vec<String>,
    stages: Vec<Stage>,
    pub stage: Stage,
    step: Step,
    /// Text above the question.
    body: Vec<String>,
    ask: Ask,
    /// The window main.rs has open.
    pub running: Phase,
    outcome: Option<Outcome>,
    cancelled: bool,

    // What the stages found, for the later ones and the last screen.
    card: Option<Node>,
    nodes: Vec<Node>,
    usb: Option<Usb>,
    ignoring: Ignoring,
    card_lines: Vec<String>,
    /// Other devices with the card's name, which cannot be ignored by name.
    ignore_notes: Vec<String>,
    modes: Vec<Mode>,
    feed_error: Option<String>,
    measure: Measure,
    signal_result: String,
    audio_inputs: AudioInputs,
    tuner: Option<Box<dyn Tuner>>,
    /// The tuner started from 0.050 because DELAY was not valid: why.
    sync_note: Option<String>,
    kept_delay: Option<String>,
    saved: bool,
    changed: usize,
}

/// `_regex_literal`: a device name as a regex matching only itself, for
/// IGNORE_CARDS_RE. A single quote becomes "." (any character): the config
/// cannot hold one.
pub fn regex_literal(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        match c {
            '\'' => out.push('.'),
            '[' | ']' | '\\' | '.' | '*' | '^' | '$' | '+' | '?' | '(' | ')' | '{' | '}' | '|' => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out
}

/// `grep -Eq PATTERN <<<TEXT`; a pattern that does not compile matches
/// nothing.
fn matches(pattern: &str, text: &str) -> bool {
    Regex::new(pattern).is_ok_and(|re| re.is_match(text))
}

/// `card_ignored`: an empty pattern ignores nothing.
fn ignored(pattern: &str, card_type: &str) -> bool {
    !pattern.is_empty() && matches(pattern, card_type)
}

/// `_ignore_without`: PATTERN without its alternatives that match NAME.
/// Splits on the "|" between alternatives, not on an escaped one inside a
/// name; a hand-written group like (a|b) is split too, which only matters
/// if it matches the card.
pub fn ignore_without(pattern: &str, name: &str) -> String {
    let p = pattern.replace("\\|", "\x01");
    // `IFS='|' read -ra`: a trailing empty field is dropped.
    let mut parts: Vec<&str> = p.split('|').collect();
    if parts.last() == Some(&"") {
        parts.pop();
    }
    let kept: Vec<String> =
        parts.into_iter().map(|a| a.replace('\x01', "\\|")).filter(|a| !matches(a, name)).collect();
    kept.join("|")
}

/// lib/setup.sh's `list_modes`: best first (highest fps, then the largest
/// picture, then uncompressed over compressed), one per fps, size and
/// compression, at most 12; only the formats the Rust viewer captures
/// (`Pixels`). lib/setup.sh also offers nv12, which mpv plays; offered
/// here it would fail at the feed, and as uncompressed it could hide the
/// card's yuyv422 at the same fps and size.
pub fn best_modes(modes: &[Mode]) -> Vec<Mode> {
    let raw = |m: &Mode| u8::from(m.fmt != Pixels::Mjpeg.setting());
    let mut rows: Vec<Mode> = modes.iter().filter(|m| is_fmt(&m.fmt)).cloned().collect();
    let key = |m: &Mode| (m.fps, u64::from(m.width) * u64::from(m.height), raw(m));
    // Stable, so equal keys keep the card's order; `sort -u` keeps the first.
    rows.sort_by_key(|m| std::cmp::Reverse(key(m)));
    rows.dedup_by(|b, a| key(a) == key(b));
    rows.truncate(12);
    rows
}

/// `_closest_mode`: the row nearest the source's suggested size and fps,
/// frame rate first, then picture size; ties keep list order.
pub fn closest_mode(width: u32, height: u32, fps: u32, rows: &[Mode]) -> Option<&Mode> {
    let area = |w: u32, h: u32| i64::from(w) * i64::from(h);
    let mut best: Option<(&Mode, i64, i64)> = None;
    for m in rows {
        let df = (i64::from(m.fps) - i64::from(fps)).abs();
        let da = (area(m.width, m.height) - area(width, height)).abs();
        if best.is_none_or(|(_, bf, ba)| df < bf || (df == bf && da < ba)) {
            best = Some((m, df, da));
        }
    }
    best.map(|(m, _, _)| m)
}

fn is_size(v: &str) -> bool {
    v.split_once('x').is_some_and(|(w, h)| !w.is_empty() && !h.is_empty() && (w.to_owned() + h).bytes().all(|b| b.is_ascii_digit()))
}

fn is_whole(v: &str) -> bool {
    !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit())
}

fn is_fmt(v: &str) -> bool {
    Pixels::from_setting(v).is_ok()
}

/// The first letter in upper case (bash's `${v^}`).
fn capital(v: &str) -> String {
    let mut c = v.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

fn bullets(items: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for i in items {
        for (n, l) in wrap(i, WIDTH - 2).into_iter().enumerate() {
            out.push(format!("{}{l}", if n == 0 { "• " } else { "  " }));
        }
    }
    out
}

fn yes_no(prompt: &str, yes: bool) -> Ask {
    Ask::Choose {
        prompt: prompt.into(),
        items: vec![item("yes", "Yes"), item("no", "No")],
        default: Some(if yes { 0 } else { 1 }),
        default_label: "default",
        sel: if yes { 0 } else { 1 },
    }
}

/// A choice starting on the item keyed `default`, if there is one.
fn choose(prompt: &str, default: &str, items: Vec<Item>) -> Ask {
    choose_labelled(prompt, default, "current", items)
}

fn choose_labelled(prompt: &str, default: &str, label: &'static str, items: Vec<Item>) -> Ask {
    let d = items.iter().position(|i| i.key == default);
    Ask::Choose { prompt: prompt.into(), items, default: d, default_label: label, sel: d.unwrap_or(0) }
}

impl Wizard {
    /// `first_run`: no settings file yet; the welcome offers to skip.
    /// Err: the settings file is there but cannot be read, so its values
    /// cannot be the starting point (nor saved over at the end).
    pub fn new(sys: Box<dyn System>, config_file: PathBuf, first_run: bool, env: &dyn Fn(&str) -> bool) -> Result<Self, String> {
        let rows = settings::rows();
        let managed = settings::with_flag("wizard");
        let defaults = rows.iter().map(|r| (r.name.clone(), r.default.clone())).collect();
        let cur = settings::read_file(&config_file)?;
        let mut new: HashMap<String, String> =
            managed.iter().map(|k| (k.clone(), cur.get(k).cloned().unwrap_or_default())).collect();
        // FULLSCREEN is the old spelling of WINDOW; the wizard always writes WINDOW.
        if !new["FULLSCREEN"].is_empty() {
            if new["WINDOW"].is_empty() {
                let window = match new["FULLSCREEN"].as_str() {
                    "yes" => "fullscreen",
                    "no" => "windowed",
                    _ => "",
                };
                new.insert("WINDOW".into(), window.into());
            }
            new.insert("FULLSCREEN".into(), String::new());
        }
        // A setting in the environment beats the file at play time.
        let env_overrides = rows.iter().filter(|r| env(&r.name)).map(|r| r.name.clone()).collect();
        let mut stages = vec![
            Stage::Welcome,
            Stage::Card,
            Stage::Source,
            Stage::Format,
            Stage::Signal,
            Stage::Audio,
            Stage::Display,
        ];
        if sys.can_match_refresh() {
            stages.push(Stage::Refresh);
        }
        stages.extend([Stage::Player, Stage::Sync, Stage::Debug, Stage::Review]);
        let mut w = Self {
            sys,
            config_file,
            first_run,
            managed,
            defaults,
            cur,
            new,
            env_overrides,
            stages,
            stage: Stage::Welcome,
            step: Step::Welcome,
            body: Vec::new(),
            ask: Ask::Continue,
            running: Phase::Choose,
            outcome: None,
            cancelled: false,
            card: None,
            nodes: Vec::new(),
            usb: None,
            ignoring: Ignoring::default(),
            card_lines: Vec::new(),
            ignore_notes: Vec::new(),
            modes: Vec::new(),
            feed_error: None,
            measure: Measure::default(),
            signal_result: String::new(),
            audio_inputs: AudioInputs::default(),
            tuner: None,
            sync_note: None,
            kept_delay: None,
            saved: false,
            changed: 0,
        };
        w.enter(Stage::Welcome);
        Ok(w)
    }

    /// The value the file will say, else the setting's default (`cur`).
    fn cur(&self, k: &str) -> String {
        match self.new.get(k).filter(|v| !v.is_empty()) {
            Some(v) => v.clone(),
            None => self.defaults.get(k).cloned().unwrap_or_default(),
        }
    }

    fn set(&mut self, k: &str, v: &str) {
        self.new.insert(k.into(), v.into());
    }

    pub fn outcome(&self) -> Option<Outcome> {
        self.outcome
    }

    /// Saved at the review.
    pub fn saved(&self) -> bool {
        self.saved
    }

    /// The window the current stage needs.
    pub fn phase(&self) -> Phase {
        self.stage.phase().unwrap_or(self.running)
    }

    /// The window has to close: the wizard ended, or the next stage needs
    /// the other window.
    pub fn wants_exit(&self) -> bool {
        self.outcome.is_some() || self.phase() != self.running
    }

    /// The chosen card and the mode to open it at.
    pub fn feed(&self) -> Option<(Node, Result<Format, String>)> {
        let node = self.card.clone()?;
        Some((node, Format::from_settings(&self.cur("RES"), &self.cur("FPS"), &self.cur("FMT"))))
    }

    /// The feed could not start (the card busy, the mode refused): back to
    /// the format stage, saying why.
    pub fn feed_failed(&mut self, why: String) {
        // The sync stage's loopback, if the card went mid-tuning: not
        // left playing behind the format page.
        self.tuner = None;
        self.feed_error = Some(why);
        self.enter(Stage::Format);
    }

    /// Ends the wizard without saving (the window closed, a signal).
    pub fn abandon(&mut self) {
        self.tuner = None;
        self.outcome = Some(Outcome::Close);
    }

    fn position(&self) -> Option<(usize, usize)> {
        let i = self.stages.iter().position(|s| *s == self.stage)?;
        // The welcome screen is not numbered.
        (i > 0).then_some((i, self.stages.len() - 1))
    }

    fn next_stage(&mut self) {
        let i = self.stages.iter().position(|s| *s == self.stage);
        match i.and_then(|i| self.stages.get(i + 1)) {
            Some(s) => self.enter(*s),
            None => self.enter(Stage::Done),
        }
    }

    /// Backspace: the stage before (its question again, on the value now
    /// chosen).
    fn previous_stage(&mut self) {
        let Some(i) = self.stages.iter().position(|s| *s == self.stage) else { return };
        if i > 1 {
            self.tuner = None;
            self.enter(self.stages[i - 1]);
        }
    }

    fn show(&mut self, step: Step, body: Vec<String>, ask: Ask) {
        self.step = step;
        self.body = body;
        self.ask = ask;
    }

    fn enter(&mut self, stage: Stage) {
        self.stage = stage;
        match stage {
            Stage::Welcome => self.welcome(),
            Stage::Card => self.card_stage(),
            Stage::Source => self.source_stage(),
            Stage::Format => self.format_stage(),
            Stage::Signal => self.signal_stage(),
            Stage::Audio => self.audio_stage(),
            Stage::Display => self.display_stage(),
            Stage::Refresh => self.refresh_stage(),
            Stage::Player => self.player_stage(),
            Stage::Sync => self.sync_stage(),
            Stage::Debug => self.debug_stage(),
            Stage::Review => self.review_stage(),
            Stage::Done => self.done_stage(),
        }
    }

    // --- stages ---------------------------------------------------------

    fn welcome(&mut self) {
        let mut body = wrap(
            "Picks your capture card and its best mode, checks the signal and the audio, and sets how the \
             picture is shown. Nothing is written until the review at the end.",
            WIDTH,
        );
        body.push(String::new());
        body.extend(wrap(
            "Before you start: the HDMI source is on and outputting, HDCP is off on the source (consoles ship \
             with it on), and the card sits on a USB 3 port.",
            WIDTH,
        ));
        body.push(String::new());
        if self.config_file.exists() {
            body.push(format!("Editing {}", self.config_file.display()));
            body.push("Every question starts on its current value; Enter keeps it.".into());
        } else {
            body.push(format!("No settings yet; they will be written to {}", self.config_file.display()));
        }
        if !self.env_overrides.is_empty() {
            body.push(String::new());
            body.extend(wrap(&format!("Warning: set in your environment, and winning over the file: {}", self.env_overrides.join(" ")), WIDTH));
        }
        let ask = if self.first_run {
            Ask::Choose {
                prompt: "Set up now?".into(),
                items: vec![item("start", "Start the setup"), item("skip", "Skip it: play with the defaults")],
                default: Some(0),
                default_label: "default",
                sel: 0,
            }
        } else {
            Ask::Continue
        };
        self.show(Step::Welcome, body, ask);
    }

    fn node_label(&self, n: &Node) -> String {
        let id = if !n.serial.is_empty() {
            format!(" · serial {}", n.serial)
        } else if !n.port.is_empty() {
            format!(" · port {}", n.port)
        } else {
            String::new()
        };
        format!("{}  {}{id}", n.card_type, self.sys.label(n))
    }

    fn usb_known(&self, n: &Node) -> bool {
        self.sys.usb(n).is_some_and(|u| capture_cards::lookup(&u.id).0 != Known::Unknown)
    }

    fn card_stage(&mut self) {
        self.nodes = self.sys.cards();
        if self.nodes.is_empty() {
            let body = vec![
                "No video capture device found.".into(),
                "Plug the card in (USB 3 port), then press Enter to look again.".into(),
            ];
            return self.show(Step::CardNone, body, Ask::Continue);
        }
        let ignore = self.new["IGNORE_CARDS_RE"].clone();
        let mut def = None;
        let mut items = Vec::new();
        for (i, n) in self.nodes.iter().enumerate() {
            let tag = ignored(&ignore, &n.card_type).then_some("ignored");
            items.push(Item { key: i.to_string(), label: self.node_label(n), tag: tag.map(str::to_string) });
            let cur = |k: &str| self.cur.get(k).map(String::as_str).unwrap_or("");
            if !n.serial.is_empty() && n.serial == cur("CARD_SERIAL") {
                def = def.or(Some(i));
            }
            if def.is_none() && !n.port.is_empty() && n.port == cur("CARD_PORT") {
                def = Some(i);
            }
            if def.is_none() && n.path.to_str() == Some(cur("DEV")) {
                def = Some(i);
            }
        }
        // Nothing chosen before: recommend the first device whose USB id is
        // a capture chip this wizard knows, rather than whichever camera is
        // first.
        if let Some(rec) = self.nodes.iter().position(|n| self.usb_known(n)) {
            let t = &mut items[rec].tag;
            *t = Some(t.as_ref().map_or("capture chip".into(), |t| format!("{t} · capture chip")));
            def = def.or(Some(rec));
        }
        let body = vec!["Capture devices found (cameras show up here too):".into()];
        let ask = choose("Which one is the HDMI capture card?", &def.map(|d| d.to_string()).unwrap_or_default(), items);
        self.show(Step::CardPick, body, ask);
    }

    fn card_picked(&mut self, pick: usize) {
        let node = self.nodes[pick].clone();
        let mut lines = Vec::new();
        if !node.serial.is_empty() {
            self.set("CARD_SERIAL", &node.serial.clone());
            self.set("CARD_PORT", "");
            self.set("DEV", "");
            lines.push(format!("OK: pinned by USB serial {}: found wherever it enumerates.", node.serial));
        } else if !node.port.is_empty() {
            self.set("CARD_SERIAL", "");
            self.set("CARD_PORT", &node.port.clone());
            self.set("DEV", "");
            lines.push("OK: this card reports no serial, so it is pinned by its USB port.".into());
            lines.push("Found again after a replug into the same port; re-run setup if you move it.".into());
        } else {
            self.set("CARD_SERIAL", "");
            self.set("CARD_PORT", "");
            self.set("DEV", &node.path.display().to_string());
            lines.push(format!("Warning: this card reports no serial, so it is pinned by node ({}).", self.sys.label(&node)));
            lines.push("Node numbers can change when devices are replugged; re-run setup if it moves.".into());
        }
        // Ignored devices (IGNORE_CARDS_RE): the other video devices,
        // offered one by one so no scan opens them as the capture card. A
        // chosen card the list already matched is taken off it.
        let mut pattern = self.new["IGNORE_CARDS_RE"].clone();
        if ignored(&pattern, &node.card_type) {
            pattern = ignore_without(&pattern, &node.card_type);
            lines.push("Warning: this card was on the ignored list; it is taken off.".into());
        }
        self.card = Some(node);
        self.card_lines = lines;
        self.ignore_notes.clear();
        self.ignoring = Ignoring { pattern, nodes: self.nodes.clone(), pick };
        self.card_details();
    }

    /// The next other device to ask about, from index `from`; the card's
    /// result once none is left.
    fn ignore_next(&mut self, from: usize) {
        let card_type = self.card.as_ref().map(|c| c.card_type.clone()).unwrap_or_default();
        for i in from..self.ignoring.nodes.len() {
            if i == self.ignoring.pick {
                continue;
            }
            let n = self.ignoring.nodes[i].clone();
            if ignored(&self.ignoring.pattern, &n.card_type) {
                continue;
            }
            if matches(&regex_literal(&n.card_type), &card_type) {
                self.ignore_notes.push(format!(
                    "{} ({}) has the capture card's name, so it cannot be ignored by name.",
                    n.card_type,
                    self.sys.label(&n)
                ));
                continue;
            }
            let mut body = std::mem::take(&mut self.ignore_notes);
            if !body.is_empty() {
                body.push(String::new());
            }
            let ask = if self.usb_known(&n) {
                body.push(format!("{} ({}) is a capture card too.", n.card_type, self.sys.label(&n)));
                yes_no("Ignore it from now on?", false)
            } else {
                body.push(format!("{} ({}) is not the capture card.", n.card_type, self.sys.label(&n)));
                yes_no("Ignore it from now on (a camera)?", true)
            };
            return self.show(Step::CardIgnore(i), body, ask);
        }
        let pattern = std::mem::take(&mut self.ignoring.pattern);
        self.set("IGNORE_CARDS_RE", &pattern);
        // Only a name that could not be ignored is left to say.
        let body = std::mem::take(&mut self.ignore_notes);
        if body.is_empty() {
            return self.next_stage();
        }
        self.show(Step::CardResult, body, Ask::Continue);
    }

    /// The chosen card's details, right after the pick: how it is pinned,
    /// its USB id, chip, support, notes and link. The other devices are
    /// asked about after it (`ignore_next`).
    fn card_details(&mut self) {
        let Some(card) = self.card.clone() else { return self.enter(Stage::Card) };
        self.usb = self.sys.usb(&card);
        let mut body = self.card_lines.clone();
        body.push(String::new());
        let Some(u) = self.usb.clone() else {
            body.push(format!("Warning: {} is not on USB; nothing to check here.", self.sys.label(&card)));
            return self.show(Step::CardDetails, body, Ask::Continue);
        };
        let (_, cc) = capture_cards::lookup(&u.id);
        let mut support = String::new();
        if !cc.status.is_empty() {
            let mut f = cc.status.splitn(3, ' ');
            let (level, date, rest) = (f.next().unwrap_or(""), f.next().unwrap_or(""), f.next().unwrap_or(""));
            let date = if date == "-" { "" } else { date };
            support = format!(
                "{level}{}{}",
                if date.is_empty() { String::new() } else { format!(" {date}") },
                if rest.is_empty() { String::new() } else { format!(" · {rest}") }
            );
        }
        body.extend([
            format!("USB id  : {} · {}", u.id, cc.chip),
            match u.version.trim() {
                // Windows gives no bcdUSB without asking the hub (#19).
                "" => format!("Port    : {}", u.port),
                v => format!("Port    : {} · USB {v}", u.port),
            },
        ]);
        if !support.is_empty() {
            body.push(format!("Support : {}", capital(&support)));
        }
        body.extend(wrap(&cc.hint, WIDTH));
        if !cc.quirks.is_empty() {
            body.push(String::new());
            body.push(format!("Known about this card (capture-cards/{}.md):", cc.file.clone().unwrap_or_default()));
            // Markdown code ticks read as noise here. As many as the page
            // holds at the window's size; the file has the rest.
            let mut shown = 0;
            for q in &cc.quirks {
                let lines = bullets(&[q.replace('`', "")]);
                if shown > 0 && body.len() + lines.len() > MAX_USB_LINES {
                    break;
                }
                body.extend(lines);
                shown += 1;
            }
            if shown < cc.quirks.len() {
                body.push(format!("  … and {} more in the file", cc.quirks.len() - shown));
            }
        }
        body.push(String::new());
        // A USB 2 chip on a USB 2 link is as good as it gets, not a fault.
        match (u.speed.as_str(), cc.link.as_str()) {
            ("5000" | "10000" | "20000", _) => {
                body.push(format!("OK: link {} Mbit/s, enough for uncompressed 1080p60", u.speed));
            }
            ("480", "480") => body.push("OK: link 480 Mbit/s (USB 2), all this chip uses".into()),
            ("480", _) => {
                body.push("Failed: link 480 Mbit/s, a USB 2 port or hub".into());
                body.push("Uncompressed 1080p60 needs USB 3: move the card to a rear USB 3 port.".into());
            }
            ("", _) => body.push(match u.unknown.as_str() {
                "" => "Warning: link speed unknown".into(),
                why => format!("Warning: link speed unknown: {why}"),
            }),
            (s, _) => body.push(format!("Warning: link {s} Mbit/s")),
        }
        self.show(Step::CardDetails, body, Ask::Continue);
    }

    fn source_stage(&mut self) {
        let mut items = Vec::new();
        for id in sources::ids() {
            if id == "generic" {
                items.push(item("generic", "Something else · the checklist for any HDMI source"));
            } else if let Some(p) = sources::get(id) {
                let level = p.status.split_whitespace().next().unwrap_or("").to_string();
                items.push(Item { key: id.into(), label: p.name, tag: (!level.is_empty()).then_some(level) });
            }
        }
        let mut def = self.new["SOURCE"].clone();
        if def.is_empty() || sources::get(&def).is_none() {
            def = "generic".into();
        }
        let body = vec!["What is plugged into the capture card's HDMI input?".into()];
        self.show(Step::SourcePick, body, choose("Source", &def, items));
    }

    fn source_tips(&mut self, pick: &str) {
        let p = sources::get(pick).or_else(|| sources::get("generic"));
        let name = p.as_ref().map_or(pick.to_string(), |p| p.name.clone());
        let mut body = vec![format!("Set these on the {name} now; the signal check comes next:")];
        body.extend(bullets(&p.map(|p| p.tips).unwrap_or_default()));
        self.show(Step::SourceTips, body, Ask::Continue);
    }

    fn format_stage(&mut self) {
        let Some(card) = self.card.clone() else { return self.enter(Stage::Card) };
        let mut body = Vec::new();
        if let Some(e) = &self.feed_error {
            body.extend(wrap(&format!("Failed: {e}"), WIDTH));
            body.push(String::new());
        }
        self.modes = best_modes(&self.sys.modes(&card));
        if self.modes.is_empty() {
            body.push("Warning: the card lists no modes it can deliver.".into());
            body.push("Some cards only list modes while a source is locked. Is the source on?".into());
            return self.show(Step::FormatNone, body, yes_no("Look again?", true));
        }
        // The source's suggestion, if it has one, picks the starting row;
        // the card's own list always decides what can be chosen. A
        // malformed suggestion (a typo in a profile) is ignored.
        let profile = sources::get(&self.new["SOURCE"]);
        let (mut sres, mut sfps) = (String::new(), String::new());
        for kv in profile.as_ref().map(|p| p.suggest.clone()).unwrap_or_default() {
            if let Some(v) = kv.strip_prefix("RES=") {
                sres = v.into();
            } else if let Some(v) = kv.strip_prefix("FPS=") {
                sfps = v.into();
            }
        }
        if !is_size(&sres) {
            sres.clear();
        }
        if !is_whole(&sfps) {
            sfps.clear();
        }
        let mut near: Option<Mode> = None;
        let sname = profile.as_ref().map(|p| p.name.clone()).unwrap_or_default();
        if !sres.is_empty() && !sfps.is_empty() {
            let (w, h) = sres.split_once('x').expect("checked by is_size");
            let (w, h, f) = (w.parse().unwrap_or(0), h.parse().unwrap_or(0), sfps.parse().unwrap_or(0));
            near = closest_mode(w, h, f, &self.modes).cloned();
        }
        let cur_key = format!("{} {} {}", self.cur("FMT"), self.cur("RES"), self.cur("FPS"));
        let mut items = Vec::new();
        for (i, m) in self.modes.iter().enumerate() {
            let desc = match Pixels::from_setting(&m.fmt) {
                Ok(Pixels::Yuyv) => "uncompressed YUYV".to_string(),
                Ok(Pixels::Mjpeg) => "MJPEG (compressed, needs decoding)".to_string(),
                Err(_) => m.fmt.clone(),
            };
            let mut tag = if i == 0 { Some("recommended".to_string()) } else { None };
            if near.as_ref() == Some(m) {
                let why = if m.size() == sres && m.fps.to_string() == sfps {
                    format!("{sname} suggests this")
                } else {
                    format!("closest to {sname}'s suggestion")
                };
                tag = Some(tag.map_or(why.clone(), |t| format!("{t} · {why}")));
            }
            items.push(Item { key: m.key(), label: format!("{} @ {} fps · {desc}", m.size(), m.fps), tag });
        }
        // Start on the saved mode if this card offers it, else on the
        // source's nearest, else on the best.
        let (def, label) = if self.modes.iter().any(|m| m.key() == cur_key) {
            (cur_key, "current")
        } else if let Some(n) = &near {
            (n.key(), "suggested")
        } else {
            (self.modes[0].key(), "best")
        };
        let card_type = card.card_type.clone();
        body.push(format!("Modes {card_type} offers, best first:"));
        body.push("Uncompressed skips a decode step; it needs the USB 3 bandwidth.".into());
        if let Some(n) = &near
            && (n.size() != sres || n.fps.to_string() != sfps)
        {
            body.push(String::new());
            body.push(format!("Warning: {sname} suggests {sres} @ {sfps}; this card can't capture that."));
            body.push(format!("Closest at the best frame rate: {} @ {}.", n.size(), n.fps));
            if n.size() != sres {
                body.push(format!("Tip: set the {sname}'s output resolution to {}p, so the card is not scaling.", n.height));
            }
        }
        // As many modes as the page holds, best first; the one the cursor
        // starts on always stays.
        let mut keep = items.len();
        loop {
            let shown: Vec<Item> = items.iter().enumerate().filter(|(i, it)| *i < keep || it.key == def).map(|(_, it)| it.clone()).collect();
            let mut page = body.clone();
            if shown.len() < items.len() {
                page.push(format!("({} lower modes left out: the window is too small for them.)", items.len() - shown.len()));
            }
            self.show(Step::FormatPick, page, choose_labelled("Mode", &def, label, shown));
            if self.lines().len() <= PAGE_LINES || keep <= 1 {
                break;
            }
            keep -= 1;
        }
    }

    fn format_input(&mut self, step: Step) {
        let (prompt, key, valid): (String, &str, fn(&str) -> bool) = match step {
            Step::FormatRes => ("Resolution".into(), "RES", is_size),
            Step::FormatFps => ("Frames per second".into(), "FPS", is_whole),
            _ => (format!("Format ({})", Pixels::settings().join(" | ")), "FMT", is_fmt),
        };
        let default = self.cur(key);
        let body = vec!["The card lists no modes: type the mode the source sends.".into()];
        self.show(step, body, Ask::Input { prompt, value: String::new(), default, valid, invalid: None });
    }

    fn signal_stage(&mut self) {
        self.measure = Measure::default();
        self.signal_result.clear();
        self.show(Step::SignalWait, Vec::new(), Ask::Wait);
    }

    /// The signal check's text; with `progress`, the count so far.
    fn signal_lines(&self, progress: bool) -> Vec<String> {
        let mut out = vec![
            format!(
                "Capturing {MEASURE_SECONDS} seconds at {} @ {} fps ({}) to see",
                self.cur("RES"),
                self.cur("FPS"),
                self.cur("FMT")
            ),
            "whether frames really arrive at that rate.".into(),
        ];
        if progress {
            out.push(String::new());
            out.push(match self.measure.first {
                None => "Waiting for the first frame…".into(),
                Some(_) => format!("{} frames so far…", self.measure.frames),
            });
        }
        out
    }

    /// The frame policy's signal state: with no frame at all, the signal
    /// check words its failure from the reason (frames that do not decode
    /// or cannot be used are not "no frames").
    pub fn signal(&mut self, signal: &Signal) {
        if self.step == Step::SignalWait && self.running == Phase::Feed {
            self.measure.reason = match signal {
                Signal::Picture => None,
                Signal::NoSignal { reason, .. } => Some(reason.clone()),
            };
        }
    }

    /// A picture from the card, as the frame policy delivers it (`app.rs`,
    /// every one, not only the ones drawn).
    pub fn frame(&mut self, now: Instant) {
        if self.step != Step::SignalWait || self.running != Phase::Feed {
            return;
        }
        let m = &mut self.measure;
        m.started.get_or_insert(now);
        match m.first {
            None => m.first = Some(now),
            Some(_) => m.frames += 1,
        }
        if self.measure.frames >= self.frames_wanted() {
            self.measure.done_at = Some(now);
            self.signal_done();
        }
    }

    /// A drawn frame's bytes, for the picture check: flat (luma spread
    /// within 8 levels) and dark (mean at most 40 of 255) across the frames
    /// looked at is "black", as lib/setup.sh reads ffmpeg's signalstats.
    /// MJPEG frames (#17) arrive here decoded to YUYV, so both are read;
    /// a full-range black (luma 0) passes the same test as limited (16).
    pub fn picture(&mut self, data: &[u8], format: Format) {
        if self.step != Step::SignalWait || self.running != Phase::Feed || self.measure.looked_at >= PICTURE_FRAMES {
            return;
        }
        if !is_fmt(&self.cur("FMT")) || data.len() != format.frame_size() {
            self.measure.unchecked = Some(format!("FMT={} frames are not read here", self.cur("FMT")));
            return;
        }
        let (mut lo, mut hi, mut sum) = (u8::MAX, 0u8, 0u64);
        // Y0 U Y1 V: every other byte is a luma sample.
        for y in data.iter().step_by(2) {
            lo = lo.min(*y);
            hi = hi.max(*y);
            sum += u64::from(*y);
        }
        let mean = sum as f64 / (data.len() / 2).max(1) as f64;
        let (plo, phi, pavg) = self.measure.picture.unwrap_or((u8::MAX, 0, 0.0));
        self.measure.picture = Some((plo.min(lo), phi.max(hi), pavg.max(mean)));
        self.measure.looked_at += 1;
    }

    /// Time passing without frames: the signal check gives up at
    /// `SIGNAL_TIMEOUT`, counted from the feed's window, not from the
    /// choice that closed the one before. True when the page changed.
    pub fn tick(&mut self, now: Instant) -> bool {
        if self.step != Step::SignalWait || self.running != Phase::Feed {
            return false;
        }
        let started = *self.measure.started.get_or_insert(now);
        if now - started < SIGNAL_TIMEOUT {
            return false;
        }
        self.measure.done_at = Some(now);
        self.signal_done();
        true
    }

    /// When the page next changes on its own (the signal check's timeout).
    pub fn next_change(&self, now: Instant) -> Option<Instant> {
        (self.step == Step::SignalWait && self.running == Phase::Feed).then(|| self.measure.started.map_or(now, |s| s + SIGNAL_TIMEOUT))
    }

    /// The frames the signal check waits for: `MEASURE_SECONDS` at FPS
    /// (validated by the format stage), as lib/setup.sh's stream count.
    fn frames_wanted(&self) -> u32 {
        (self.cur("FPS").parse::<f64>().unwrap_or(60.0).round() as u32 * MEASURE_SECONDS).max(1)
    }

    fn signal_done(&mut self) {
        let m = &self.measure;
        let mut body = self.signal_lines(false);
        body.push(String::new());
        let (Some(first), Some(end)) = (m.first, m.done_at) else {
            let (failed, advice) = match &m.reason {
                Some(Reason::Undecodable(why)) => (
                    format!("frames arrive but none decode as MJPEG ({why})."),
                    "The card's MJPEG may be broken: go back and pick a yuyv422 mode.",
                ),
                Some(Reason::Unusable(why)) => (
                    format!("frames arrive but the viewer cannot use them ({why})."),
                    if self.sys.has_view_sh() { "Go back and pick another mode, or use view.sh." } else { "Go back and pick another mode." },
                ),
                Some(Reason::Nothing) | None => (
                    "no frames in 6 seconds: no signal, or HDCP is on at the source.".into(),
                    "Check the source is on and outputting, HDCP is off, and the card is plugged in.",
                ),
            };
            self.signal_result = format!("failed: {failed}");
            body.extend(wrap(&format!("Failed: {}", capital(&failed)), WIDTH));
            body.push(advice.into());
            return self.show(Step::SignalRetry, body, yes_no("Try again?", true));
        };
        let secs = (end - first).as_secs_f64();
        let measured = if m.frames == 0 || secs <= 0.0 { 0.0 } else { f64::from(m.frames) / secs };
        let fps: f64 = self.cur("FPS").parse().unwrap_or(0.0);
        let got = format!("{measured:.1}");
        // The timeout with frames short: lib/setup.sh's `timeout 6` fails
        // the check then too, with Try again; only a full count is measured.
        let want = self.frames_wanted();
        if m.frames < want {
            let failed = format!(
                "only {} of {} frames in 6 seconds ({got} fps for {} requested): the mode is not what the card delivers, or the signal drops.",
                m.frames + 1,
                want + 1,
                self.cur("FPS")
            );
            self.signal_result = format!("failed: {failed}");
            body.extend(wrap(&format!("Failed: {}", capital(&failed)), WIDTH));
            return self.show(Step::SignalRetry, body, yes_no("Try again?", true));
        }
        self.signal_result = format!("{got} fps measured");
        if measured >= fps * 0.8 && measured <= fps * 1.2 {
            body.push(format!("OK: {got} fps measured ({})", self.cur("RES")));
        } else {
            body.push(format!("Warning: {got} fps measured for {} requested ({}).", self.cur("FPS"), self.cur("RES")));
            body.push("A number far off means the mode is not what the card delivers.".into());
        }
        // Some cards (the MS2109) stream black frames at full rate with
        // nothing plugged in, so frames alone do not prove a source.
        match (m.picture, &m.unchecked) {
            (Some((lo, hi, avg)), _) if hi.saturating_sub(lo) <= 8 && avg <= 40.0 => {
                self.signal_result = format!("{got} fps measured, but the picture is uniformly black");
                body.push("Warning: the picture is uniformly black.".into());
                body.extend(wrap(
                    "No source, the source is asleep, or HDCP is on. Some cards stream black when nothing is plugged in.",
                    WIDTH,
                ));
                return self.show(Step::SignalRetry, body, yes_no("Try again?", true));
            }
            (Some(_), _) => {}
            (None, why) => {
                let why = why.clone().unwrap_or_else(|| "no frame was drawn to look at".into());
                self.signal_result = format!("{}; picture not checked ({why})", self.signal_result);
                body.push(format!("Warning: could not check the picture: {why}"));
            }
        }
        self.show(Step::SignalResult, body, Ask::Continue);
    }

    fn audio_stage(&mut self) {
        let Some(card) = self.card.clone() else { return self.enter(Stage::Card) };
        let mut body = Vec::new();
        // A PipeWire snapshot: its round trips give up after 2 s each.
        self.audio_inputs = self.sys.audio_inputs(&card).unwrap_or_else(|e| {
            body.push(format!("Warning: the audio inputs could not be listed: {e}"));
            AudioInputs::default()
        });
        let mut items = Vec::new();
        let matched = self.audio_inputs.card.as_ref().map(|c| c.name.clone());
        match &matched {
            Some(m) => {
                body.push("The card's own audio input, found on its USB device:".into());
                body.push(m.clone());
                items.push(tagged("auto", "Use the card's audio (found automatically each start)", "recommended"));
            }
            None => body.push("Warning: no audio input found on the card's USB device.".into()),
        }
        for s in &self.audio_inputs.all {
            if Some(s) != matched.as_ref() {
                items.push(item(&format!("src:{s}"), s));
            }
        }
        items.push(item("none", "No audio (video only)"));
        let def = if self.new["AUDIO"] == "none" {
            "none".to_string()
        } else if !self.new["SRC"].is_empty() {
            format!("src:{}", self.new["SRC"])
        } else {
            "auto".into()
        };
        self.show(Step::AudioPick, body, choose("Audio", &def, items));
    }

    fn display_stage(&mut self) {
        let body = vec!["How the picture is shown, as in a game's display settings:".into()];
        let items = vec![
            tagged("fullscreen", "Fullscreen · lowest latency (direct scanout)", "recommended"),
            item("borderless", "Borderless · maximized, no border, the bar stays visible"),
            item("windowed", "Windowed · a normal window you can move and resize"),
        ];
        let ask = choose("Display mode", &self.cur("WINDOW"), items);
        self.show(Step::DisplayPick, body, ask);
    }

    fn display_size(&mut self) {
        let cs = self.cur("WINDOW_SIZE");
        let mut items = vec![item("1280x720", "1280 × 720"), item("1600x900", "1600 × 900"), item("1920x1080", "1920 × 1080")];
        if !["1280x720", "1600x900", "1920x1080"].contains(&cs.as_str()) {
            items.push(item(&cs, &cs));
        }
        items.push(item("custom", "Custom size…"));
        let ask = choose("Window size", &cs, items);
        self.show(Step::DisplaySize, Vec::new(), ask);
    }

    fn refresh_stage(&mut self) {
        let fps = self.cur("FPS");
        let body = wrap(
            &format!(
                "A {fps} fps feed on a display that refreshes at a different rate judders: frames are held for an \
                 uneven number of refreshes. While the feed runs, the monitor that has focus can switch to its mode \
                 nearest {fps} Hz."
            ),
            WIDTH,
        );
        // With nothing in the file, start on "match": that is what a menu
        // launch does today, and writing "keep" by default would quietly
        // turn it off.
        let def = Some(self.new["REFRESH"].clone()).filter(|v| !v.is_empty()).unwrap_or_else(|| "match".into());
        let items = vec![
            tagged("match", "Match the feed while it runs, restore afterwards", "recommended"),
            item("keep", "Leave the display alone"),
        ];
        self.show(Step::RefreshPick, body, choose("Refresh", &def, items));
    }

    /// mpv is view.sh's player: the overlay and the live keys exist only
    /// there. gst and ffplay stay for comparisons, set by hand; a config
    /// that already says so is asked about.
    fn player_stage(&mut self) {
        let backend = self.new["BACKEND"].clone();
        if !backend.is_empty() && backend != "mpv" {
            let body = vec![
                format!("Warning: the config plays with BACKEND={backend}, a diagnostic player."),
                "It has no overlay and no live keys (Tab, audio sync, volume); mpv has all of them.".into(),
            ];
            return self.show(Step::PlayerSwitch, body, yes_no("Switch to mpv?", true));
        }
        self.player_scaling("mpv");
    }

    fn player_scaling(&mut self, backend: &str) {
        self.set("BACKEND", backend);
        if backend != "mpv" {
            self.set("SCALING", "");
            return self.next_stage();
        }
        let mut items = vec![
            tagged("balanced", "Balanced · sharp upscale, low GPU cost", "recommended"),
            item("fast", "Fast · bilinear, lowest GPU time, softer"),
            item("high-quality", "High quality · EWA Lanczos upscale, more GPU time"),
        ];
        let text = if self.sys.has_view_sh() {
            items.push(item("config", "Your mpv.conf · whatever it sets (may be expensive)"));
            "How the picture is scaled to the window. view.sh's mpv reads it, and this viewer too (where \
             \"Your mpv.conf\" reads as balanced). The feed behind this page keeps the saved scaling; \
             the choice shows from the next start."
        } else {
            "How the picture is scaled to the window. The feed behind this page keeps the saved scaling; \
             the choice shows from the next start."
        };
        let ask = choose("Scaling", &self.cur("SCALING"), items);
        let body = wrap(text, WIDTH);
        self.show(Step::PlayerScaling, body, ask);
    }

    fn sync_stage(&mut self) {
        if self.new["AUDIO"] == "none" {
            let body = vec!["No audio was chosen, so there is nothing to line up.".into()];
            return self.show(Step::SyncNone, body, Ask::Continue);
        }
        let mut body = wrap(
            "Audio through the loopback arrives before the picture, which takes the card, USB and display path. \
             DELAY holds the audio back to match.",
            WIDTH,
        );
        body.extend(wrap(
            &format!("Current: DELAY={} s. Needs a live picture with sound, e.g. console menu clicks.", self.cur("DELAY")),
            WIDTH,
        ));
        self.show(Step::SyncAsk, body, yes_no("Line them up now? (the sound plays while you watch)", true));
    }

    fn sync_start(&mut self) {
        let source = match &self.new["SRC"] {
            s if !s.is_empty() => Some(s.clone()),
            // By its id: a Windows name can be another device's too.
            _ => self.audio_inputs.card.as_ref().map(|c| c.id.clone()),
        };
        // A DELAY lib/settings.sh refuses starts the tuner from the default.
        let delay = self.cur("DELAY");
        let default = live_changes::DEFAULT_DELAY_MS;
        self.sync_note =
            settings::check_value("DELAY", &delay).err().map(|e| format!("{e}: starting from {}", live_changes::seconds(default)));
        let delay_ms = match self.sync_note {
            None => live_changes::delay_ms(&delay).unwrap_or(default),
            Some(_) => default,
        };
        // Audio::start: its PipeWire round trips give up after 2 s each.
        let started = match source {
            None if self.sys.has_view_sh() => Err("the card has no PipeWire source; pick one in the audio stage".to_string()),
            None => Err("the card has no audio input of its own; pick one in the audio stage".to_string()),
            Some(s) => self.sys.start_audio(&s, delay_ms),
        };
        match started {
            Ok(t) => {
                self.tuner = Some(t);
                self.show(Step::SyncTune, Vec::new(), Ask::Tune);
            }
            Err(e) => {
                let mut body = wrap(&format!("Failed: the audio did not start: {e}"), WIDTH);
                body.push(format!("DELAY left at {}.", self.cur("DELAY")));
                self.show(Step::SyncResult, body, Ask::Continue);
            }
        }
    }

    fn tune_lines(&self) -> Vec<String> {
        let ms = self.tuner.as_ref().map_or(0, |t| t.delay_ms());
        vec![
            "Watch something with sharp transients (console menu clicks are ideal):".into(),
            String::new(),
            "    j / k  or  ← / →          audio 10 ms earlier / later".into(),
            "    h / l  or  shift ← / →    audio 50 ms earlier / later".into(),
            "    0                         reset to 0".into(),
            "    Enter                     keep this DELAY".into(),
            "    Esc                       leave DELAY as it was".into(),
            String::new(),
            format!("DELAY = {}", live_changes::seconds(ms)),
        ]
        .into_iter()
        .chain(self.sync_note.clone())
        .collect()
    }

    fn sync_end(&mut self, keep: bool) {
        let kept = self.tuner.take().map(|t| live_changes::seconds(t.delay_ms()));
        let body = match kept.filter(|_| keep) {
            Some(v) => {
                self.set("DELAY", &v);
                self.kept_delay = Some(v.clone());
                vec![format!("OK: DELAY={v} will be saved.")]
            }
            None => vec![format!("DELAY left at {}.", self.cur("DELAY"))],
        };
        self.show(Step::SyncResult, body, Ask::Continue);
    }

    fn debug_stage(&mut self) {
        let text = if self.sys.has_view_sh() {
            "Normally off. Turn it on while chasing stutter, drops or audio drift; ./view.sh --debug does the same \
             for a single run."
        } else {
            "Normally off. Turn it on while chasing stutter, drops or audio drift."
        };
        let body = wrap(text, WIDTH);
        let items = vec![
            tagged("off", "Off", "recommended"),
            item("log", "Log · every decision, verbose player output, a perf line every 5 s"),
            item("window", "Log + live dashboard in its own terminal"),
        ];
        let ask = choose("Debug output", &self.cur("DEBUG"), items);
        self.show(Step::DebugPick, body, ask);
    }

    /// The managed settings that change: (name, old, new).
    fn diff(&self) -> Vec<(String, String, String)> {
        self.managed
            .iter()
            .map(|k| (k.clone(), self.cur.get(k).cloned().unwrap_or_default(), self.new[k].clone()))
            .filter(|(_, old, new)| old != new)
            .collect()
    }

    fn review_stage(&mut self) {
        let diff = self.diff();
        self.changed = diff.len();
        if diff.is_empty() {
            let body = vec![format!("OK: no changes: {} already says all of this.", self.config_file.display())];
            return self.show(Step::ReviewResult, body, Ask::Continue);
        }
        let body = diff
            .iter()
            .map(|(k, old, new)| {
                if old.is_empty() {
                    format!("+ {k}={new}")
                } else if new.is_empty() {
                    format!("- {k}={old}")
                } else {
                    format!("~ {k}: {old} → {new}")
                }
            })
            .collect();
        let ask = yes_no(&format!("Write these to {}?", self.config_file.display()), true);
        self.show(Step::ReviewAsk, body, ask);
    }

    /// lib/setup.sh's `apply_to`: config_write of the settings with a
    /// value, then config_unset of those without.
    fn apply(&self) -> Result<(), String> {
        let set: Vec<(&str, &str)> =
            self.managed.iter().filter(|k| !self.new[*k].is_empty()).map(|k| (k.as_str(), self.new[k].as_str())).collect();
        let unset: Vec<&str> = self.managed.iter().filter(|k| self.new[*k].is_empty()).map(String::as_str).collect();
        settings::write(&self.config_file, &set)?;
        settings::unset(&self.config_file, &unset)
    }

    fn done_stage(&mut self) {
        self.tuner = None;
        let title = if self.saved {
            "Setup complete"
        } else if self.changed == 0 && !self.cancelled {
            "Nothing to change"
        } else {
            "Setup closed without saving"
        };
        let mut body = vec![title.to_string(), String::new()];
        if let Some(c) = &self.card {
            // An unknown link speed is said on the card's page, not here.
            let speed = match self.usb.as_ref().map(|u| u.speed.as_str()) {
                Some(s) if !s.is_empty() => format!(" · {s} Mbit/s"),
                _ => String::new(),
            };
            body.push(format!("Card    : {} ({}){speed}", c.card_type, self.sys.label(c)));
            body.push(format!(
                "Mode    : {} @ {} fps, {}{}",
                self.cur("RES"),
                self.cur("FPS"),
                self.cur("FMT"),
                if self.signal_result.is_empty() { String::new() } else { format!(" · {}", self.signal_result) }
            ));
            let size = if self.cur("WINDOW") == "windowed" { format!(" {}", self.cur("WINDOW_SIZE")) } else { String::new() };
            // BACKEND is view.sh's player.
            let player = if self.sys.has_view_sh() { format!(" · player {}", self.cur("BACKEND")) } else { String::new() };
            body.push(format!("Display : {}{size}{player}", self.cur("WINDOW")));
            body.push(String::new());
        }
        body.push(if self.sys.has_view_sh() {
            "Change any of this: protohdmicapture --setup (or ./view.sh --setup)".into()
        } else {
            "Change any of this: protohdmicapture --setup".into()
        });
        let can_play = self.saved || (self.changed == 0 && !self.cancelled && self.card.is_some());
        let ask = if can_play {
            Ask::Choose {
                prompt: "Now".into(),
                items: vec![item("play", "Start the feed"), item("close", "Close")],
                default: Some(0),
                default_label: "default",
                sel: 0,
            }
        } else {
            Ask::Continue
        };
        self.show(Step::Done, body, ask);
    }

    // --- answers ------------------------------------------------------------

    /// The answer to the screen's question: the chosen item's key, the
    /// typed text, or "" for Continue.
    fn answer(&mut self, a: &str) {
        match self.step.clone() {
            Step::Welcome => match a {
                "skip" => self.skip(),
                _ => self.next_stage(),
            },
            Step::CardNone => self.enter(Stage::Card),
            Step::CardPick => {
                if let Ok(i) = a.parse() {
                    self.card_picked(i);
                }
            }
            Step::CardIgnore(i) => {
                if a == "yes" {
                    let lit = regex_literal(&self.ignoring.nodes[i].card_type);
                    let p = &mut self.ignoring.pattern;
                    if !p.is_empty() {
                        p.push('|');
                    }
                    p.push_str(&lit);
                }
                self.ignore_next(i + 1);
            }
            Step::CardDetails => self.ignore_next(0),
            Step::CardResult | Step::SourceTips | Step::SignalResult | Step::SyncNone | Step::SyncResult => {
                self.next_stage()
            }
            Step::SourcePick => {
                self.set("SOURCE", if a == "generic" { "" } else { a });
                self.source_tips(a);
            }
            Step::FormatPick => {
                let mut f = a.split(' ');
                let (fmt, res, fps) = (f.next().unwrap_or(""), f.next().unwrap_or(""), f.next().unwrap_or(""));
                self.set("FMT", fmt);
                self.set("RES", res);
                self.set("FPS", fps);
                self.feed_error = None;
                self.next_stage();
            }
            Step::FormatNone => match a {
                "yes" => self.enter(Stage::Format),
                _ => self.format_input(Step::FormatRes),
            },
            Step::FormatRes => {
                self.set("RES", a);
                self.format_input(Step::FormatFps);
            }
            Step::FormatFps => {
                self.set("FPS", a);
                self.format_input(Step::FormatFmt);
            }
            Step::FormatFmt => {
                self.set("FMT", a);
                self.feed_error = None;
                self.next_stage();
            }
            Step::SignalWait => {}
            Step::SignalRetry => match a {
                "yes" => self.signal_stage(),
                _ => self.next_stage(),
            },
            Step::AudioPick => {
                let (audio, src) = match a {
                    "auto" => ("", ""),
                    "none" => ("none", ""),
                    src => ("", src.strip_prefix("src:").unwrap_or(src)),
                };
                self.set("AUDIO", audio);
                self.set("SRC", src);
                self.next_stage();
            }
            Step::DisplayPick => {
                self.set("WINDOW", a);
                if a == "windowed" {
                    self.display_size();
                } else {
                    self.set("WINDOW_SIZE", "");
                    self.next_stage();
                }
            }
            Step::DisplaySize => {
                if a == "custom" {
                    let default = self.cur("WINDOW_SIZE");
                    let ask = Ask::Input {
                        prompt: "Window size (WIDTHxHEIGHT)".into(),
                        value: String::new(),
                        default,
                        valid: is_size,
                        invalid: None,
                    };
                    self.show(Step::DisplayCustom, Vec::new(), ask);
                } else {
                    self.set("WINDOW_SIZE", a);
                    self.next_stage();
                }
            }
            Step::DisplayCustom => {
                self.set("WINDOW_SIZE", a);
                self.next_stage();
            }
            Step::RefreshPick => {
                self.set("REFRESH", a);
                self.next_stage();
            }
            Step::PlayerSwitch => {
                let backend = if a == "yes" { "mpv".to_string() } else { self.new["BACKEND"].clone() };
                self.player_scaling(&backend);
            }
            Step::PlayerScaling => {
                self.set("SCALING", a);
                self.next_stage();
            }
            Step::SyncAsk => match a {
                "yes" => self.sync_start(),
                _ => self.next_stage(),
            },
            Step::SyncTune => {}
            Step::DebugPick => {
                self.set("DEBUG", if a == "off" { "" } else { a });
                self.next_stage();
            }
            Step::ReviewAsk => {
                let body = if a == "yes" {
                    match self.apply() {
                        Ok(()) => {
                            self.saved = true;
                            vec!["OK: saved.".to_string()]
                        }
                        Err(e) => vec![format!("Failed: could not write: {e}")],
                    }
                } else {
                    vec!["Nothing written.".to_string()]
                };
                self.show(Step::ReviewResult, body, Ask::Continue);
            }
            Step::ReviewResult => self.enter(Stage::Done),
            Step::Done => {
                self.outcome = Some(if a == "play" { Outcome::Play } else { Outcome::Close });
            }
        }
    }

    /// First run, "skip": a settings file that only says so, so the next
    /// start does not ask again, then the feed with the defaults (view.sh's
    /// first-run "n").
    fn skip(&mut self) {
        let written = settings::write(&self.config_file, &[]).and_then(|()| {
            use std::io::Write as _;
            std::fs::OpenOptions::new()
                .append(true)
                .open(&self.config_file)
                .and_then(|mut f| f.write_all(b"# Setup was skipped; run ./view.sh --setup (or protohdmicapture --setup) to choose settings.\n"))
                .map_err(|e| format!("cannot write {}: {e}", self.config_file.display()))
        });
        match written {
            Ok(()) => self.outcome = Some(Outcome::Play),
            Err(e) => {
                let body = vec![format!("Failed: {e}")];
                self.show(Step::Done, body, Ask::Continue);
                self.stage = Stage::Done;
            }
        }
    }

    /// Esc: the wizard ends here, nothing written.
    fn cancel(&mut self) {
        self.tuner = None;
        self.cancelled = true;
        self.enter(Stage::Done);
    }

    /// A key on the wizard's page.
    pub fn key(&mut self, key: &Key) {
        if self.outcome.is_some() {
            return;
        }
        let ask = std::mem::replace(&mut self.ask, Ask::Continue);
        // The question stays on screen unless the key answered it or moved on.
        if let Some(a) = self.on_key(key, ask) {
            self.ask = a;
        }
    }

    fn on_key(&mut self, key: &Key, ask: Ask) -> Option<Ask> {
        match ask {
            Ask::Input { prompt, mut value, default, valid, mut invalid } => {
                match key {
                    Key::Char(c) => {
                        value.push_str(c);
                        invalid = None;
                    }
                    Key::Backspace => {
                        value.pop();
                    }
                    // Off the typed question: the stage again.
                    Key::Escape => {
                        self.enter(self.stage);
                        return None;
                    }
                    Key::Enter => {
                        let v = if value.is_empty() { default.clone() } else { value.clone() };
                        if valid(&v) {
                            self.answer(&v);
                            return None;
                        }
                        invalid = Some(format!("\"{v}\" is not valid here"));
                    }
                    _ => {}
                }
                Some(Ask::Input { prompt, value, default, valid, invalid })
            }
            Ask::Choose { prompt, items, default, default_label, mut sel } => {
                let yes_no = items.len() == 2 && items[0].key == "yes";
                let picked = match key {
                    Key::Up => {
                        sel = sel.saturating_sub(1);
                        None
                    }
                    Key::Down => {
                        sel = (sel + 1).min(items.len().saturating_sub(1));
                        None
                    }
                    Key::Enter => Some(items[sel].key.clone()),
                    Key::Char(c) if c.parse::<usize>().is_ok_and(|n| (1..=items.len()).contains(&n)) => {
                        Some(items[c.parse::<usize>().expect("checked") - 1].key.clone())
                    }
                    Key::Char(c) if yes_no && (c == "y" || c == "n") => Some(if c == "y" { "yes" } else { "no" }.to_string()),
                    _ => {
                        let ask = Ask::Choose { prompt, items, default, default_label, sel };
                        return self.common(key, ask);
                    }
                };
                match picked {
                    Some(k) => {
                        self.answer(&k);
                        None
                    }
                    None => Some(Ask::Choose { prompt, items, default, default_label, sel }),
                }
            }
            Ask::Continue => match key {
                Key::Enter => {
                    self.answer("");
                    None
                }
                _ => self.common(key, Ask::Continue),
            },
            Ask::Wait => self.common(key, Ask::Wait),
            Ask::Tune => {
                let now = self.tuner.as_ref().map_or(0, |t| t.delay_ms());
                let by = match key {
                    Key::Char(c) if c == "j" => Some(-10),
                    Key::Char(c) if c == "k" => Some(10),
                    Key::Char(c) if c == "h" => Some(-50),
                    Key::Char(c) if c == "l" => Some(50),
                    Key::Char(c) if c == "0" => Some(-i32::try_from(now).unwrap_or(i32::MAX)),
                    Key::Left => Some(-10),
                    Key::Right => Some(10),
                    Key::ShiftLeft => Some(-50),
                    Key::ShiftRight => Some(50),
                    _ => None,
                };
                match (by, key) {
                    (Some(by), _) => {
                        if let Some(t) = &self.tuner {
                            t.nudge(by);
                        }
                        Some(Ask::Tune)
                    }
                    (None, Key::Enter) => {
                        self.sync_end(true);
                        None
                    }
                    (None, Key::Escape) => {
                        self.sync_end(false);
                        None
                    }
                    _ => self.common(key, Ask::Tune),
                }
            }
        }
    }

    /// Esc, Backspace and q on a screen without typing. Returns the screen's
    /// question unless the key moved on.
    fn common(&mut self, key: &Key, ask: Ask) -> Option<Ask> {
        match key {
            Key::Escape if self.stage == Stage::Done => {
                self.outcome = Some(Outcome::Close);
                None
            }
            Key::Escape => {
                self.cancel();
                None
            }
            Key::Backspace if self.stage != Stage::Done => {
                self.previous_stage();
                None
            }
            Key::Char(c) if c == "q" => {
                self.tuner = None;
                if self.stage != Stage::Done {
                    self.cancelled = true;
                }
                self.outcome = Some(Outcome::Close);
                None
            }
            _ => Some(ask),
        }
    }

    // --- the page -----------------------------------------------------------

    /// The page's text: the title line, the stage's text, the question and
    /// the keys.
    pub fn lines(&self) -> Vec<String> {
        let mut out = vec![match self.position() {
            Some((i, n)) => format!("protoHDMIcapture setup · {i}/{n} · {}", self.stage.title()),
            None => "protoHDMIcapture setup".into(),
        }];
        out.push(String::new());
        let body = match self.step {
            Step::SignalWait => self.signal_lines(true),
            Step::SyncTune => self.tune_lines(),
            _ => self.body.clone(),
        };
        out.extend(body);
        match &self.ask {
            Ask::Continue | Ask::Wait | Ask::Tune => {}
            Ask::Choose { prompt, items, default, default_label, sel } => {
                out.push(String::new());
                out.push(if prompt.ends_with('?') { prompt.clone() } else { format!("{prompt}:") });
                let w = items.iter().map(|i| i.label.chars().count()).max().unwrap_or(0);
                for (n, i) in items.iter().enumerate() {
                    let mut mark = String::new();
                    if *default == Some(n) {
                        mark.push_str(&format!("  ◂ {default_label}"));
                    }
                    if let Some(t) = &i.tag {
                        mark.push_str(&format!("  ◂ {t}"));
                    }
                    let pad = w - i.label.chars().count();
                    let cursor = if n == *sel { "▸" } else { " " };
                    out.push(format!("{cursor} {:>2}  {}{}{mark}", n + 1, i.label, " ".repeat(pad)).trim_end().to_string());
                }
            }
            Ask::Input { prompt, value, default, invalid, .. } => {
                out.push(String::new());
                out.push(format!("{prompt} [Enter = \"{default}\"]: {value}▏"));
                if let Some(i) = invalid {
                    out.push(i.clone());
                }
            }
        }
        // The keys: what answers this screen, then the way back and out.
        let answer = match &self.ask {
            Ask::Input { .. } => "type · Enter OK · Backspace delete · Esc back",
            Ask::Tune => "Enter keep · Esc leave as it was",
            Ask::Wait => "",
            Ask::Continue if self.stage == Stage::Done => "Enter close",
            Ask::Continue => "Enter continue",
            Ask::Choose { .. } => "↑↓ or the number · Enter OK",
        };
        let leave = match (&self.ask, self.stage) {
            (Ask::Input { .. } | Ask::Tune, _) => "",
            (_, Stage::Done) => "q close",
            (_, Stage::Welcome) => "Esc cancel · q quit",
            _ => "Backspace back · Esc cancel · q quit",
        };
        out.push(String::new());
        out.push([answer, leave].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" · "));
        out
    }
}

/// The window before a card is chosen: a source that never sends a frame,
/// at the size the window opens.
pub struct Idle;

impl Idle {
    pub const FORMAT: Format = Format { width: 1280, height: 720, fps: 60.0, pixels: Pixels::Yuyv };
}

impl FrameSource for Idle {
    fn format(&self) -> Format {
        Self::FORMAT
    }

    fn start(&mut self) -> io::Result<Box<dyn Stream + '_>> {
        Ok(Box::new(Idle))
    }
}

/// Idle's stream: every wait runs out. The policy calls it "never" after
/// 5 s, which the wizard's page hides (it shows no banner).
impl Stream for Idle {
    fn next(&mut self, wait: Duration, _: &mut dyn FnMut(Raw<'_>)) -> io::Result<()> {
        std::thread::sleep(wait);
        Ok(())
    }
}

/// The machine as the wizard sees it (#19): the capture card through
/// card.rs, the sync stage's loopback on the platform's audio.
pub struct Machine {
    /// SINK and LAT_MS, for the sync stage's loopback.
    pub sink: String,
    pub buffer_ms: u32,
    /// Whether this desktop can match the refresh rate (Hyprland's).
    pub refresh: bool,
}

struct Loopback {
    _audio: crate::audio::Audio,
    controls: crate::audio::Controls,
}

impl Loopback {
    /// The sync stage's loopback from `source`, held back by `delay_ms`.
    fn start(source: &str, sink: &str, delay_ms: u32, buffer_ms: u32) -> Result<Box<dyn Tuner>, String> {
        let sink = Some(sink).filter(|s| !s.is_empty());
        let audio = crate::audio::Audio::start(source, sink, delay_ms, buffer_ms)?;
        eprintln!("{}", audio.describe());
        let controls = audio.controls();
        Ok(Box::new(Self { _audio: audio, controls }))
    }
}

impl Tuner for Loopback {
    fn delay_ms(&self) -> u32 {
        self.controls.delay_ms()
    }

    fn nudge(&self, by_ms: i32) -> u32 {
        self.controls.nudge_delay(by_ms)
    }
}

impl System for Machine {
    fn cards(&self) -> Vec<Node> {
        crate::card::list()
    }

    fn usb(&self, node: &Node) -> Option<Usb> {
        crate::card::usb(node)
    }

    fn modes(&self, node: &Node) -> Vec<Mode> {
        crate::card::modes(node)
    }

    fn audio_inputs(&self, node: &Node) -> Result<AudioInputs, String> {
        let inputs = crate::card::audio_inputs(Some(node))?;
        let card = inputs.card.map_err(|e| eprintln!("warning: the card's audio input: {e}")).ok();
        Ok(AudioInputs { card, all: inputs.all })
    }

    fn can_match_refresh(&self) -> bool {
        self.refresh
    }

    fn start_audio(&self, source: &str, delay_ms: u32) -> Result<Box<dyn Tuner>, String> {
        Loopback::start(source, &self.sink, delay_ms, self.buffer_ms)
    }

    fn label(&self, node: &Node) -> String {
        crate::card::label(node)
    }

    /// view.sh runs on Linux only.
    fn has_view_sh(&self) -> bool {
        cfg!(target_os = "linux")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    struct FakeTuner(Rc<Cell<u32>>);

    impl Tuner for FakeTuner {
        fn delay_ms(&self) -> u32 {
            self.0.get()
        }
        fn nudge(&self, by: i32) -> u32 {
            self.0.set((i64::from(self.0.get()) + i64::from(by)).clamp(0, 1000) as u32);
            self.0.get()
        }
    }

    struct Fake {
        nodes: Vec<Node>,
        modes: Vec<Mode>,
        refresh: bool,
        delay: Rc<Cell<u32>>,
        /// Why the card's link is not known; empty: a 5000 Mbit/s USB 3.20 link.
        link_unknown: &'static str,
    }

    fn node(path: &str, serial: &str, port: &str, t: &str) -> Node {
        Node { path: path.into(), serial: serial.into(), port: port.into(), card_type: t.into() }
    }

    fn mode(fmt: &str, w: u32, h: u32, fps: u32) -> Mode {
        Mode { fmt: fmt.into(), width: w, height: h, fps }
    }

    impl System for Fake {
        fn cards(&self) -> Vec<Node> {
            self.nodes.clone()
        }
        fn usb(&self, n: &Node) -> Option<Usb> {
            let id = match n.card_type.as_str() {
                "UVC Camera (1e4e:7016)" => "1e4e:7016",
                "USB Video" => "534d:2109",
                _ => "046d:0825",
            };
            Some(match self.link_unknown {
                "" => Usb { id: id.into(), port: "3-2".into(), version: " 3.20".into(), speed: "5000".into(), unknown: String::new() },
                why => Usb { id: id.into(), port: "3-2".into(), version: String::new(), speed: String::new(), unknown: why.into() },
            })
        }
        fn modes(&self, _: &Node) -> Vec<Mode> {
            self.modes.clone()
        }
        fn audio_inputs(&self, _: &Node) -> Result<AudioInputs, String> {
            let card = AudioInput { id: "alsa_input.usb-card".into(), name: "alsa_input.usb-card".into() };
            Ok(AudioInputs { card: Some(card), all: vec!["alsa_input.usb-card".into(), "alsa_input.mic".into()] })
        }
        fn can_match_refresh(&self) -> bool {
            self.refresh
        }
        fn start_audio(&self, _: &str, delay_ms: u32) -> Result<Box<dyn Tuner>, String> {
            self.delay.set(delay_ms);
            Ok(Box::new(FakeTuner(self.delay.clone())))
        }
    }

    fn fake() -> Fake {
        Fake {
            nodes: vec![
                node("/dev/video0", "CAM1", "pci-a", "OBSBOT Meet 2"),
                node("/dev/video2", "20000130041415", "pci-b", "UVC Camera (1e4e:7016)"),
            ],
            modes: vec![mode("yuyv422", 1920, 1080, 60), mode("mjpeg", 1920, 1080, 60), mode("yuyv422", 1280, 720, 30)],
            refresh: true,
            delay: Rc::new(Cell::new(0)),
            link_unknown: "",
        }
    }

    fn scratch(name: &str, config: Option<&str>) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("phc-wizard-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("config");
        if let Some(c) = config {
            std::fs::write(&f, c).unwrap();
        }
        f
    }

    fn wizard(sys: Fake, file: &std::path::Path, first_run: bool) -> Wizard {
        Wizard::new(Box::new(sys), file.to_path_buf(), first_run, &|_| false).unwrap()
    }

    fn press(w: &mut Wizard, keys: &[&str]) {
        for k in keys {
            let key = match *k {
                "Enter" => Key::Enter,
                "Esc" => Key::Escape,
                "Up" => Key::Up,
                "Down" => Key::Down,
                "Left" => Key::Left,
                "Right" => Key::Right,
                "BS" => Key::Backspace,
                c => Key::Char(c.into()),
            };
            w.key(&key);
        }
    }

    /// The selected item's key on a choice screen.
    fn selected(w: &Wizard) -> String {
        match &w.ask {
            Ask::Choose { items, sel, .. } => items[*sel].key.clone(),
            other => panic!("not a choice: {other:?}"),
        }
    }

    /// The signal check passes: a frame every 1/60 s with a picture.
    fn signal_ok(w: &mut Wizard) {
        let t0 = Instant::now();
        w.tick(t0);
        let mut frame = vec![0u8; Idle::FORMAT.frame_size()];
        for (i, b) in frame.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        for i in 0..=120u32 {
            if i < 5 {
                w.picture(&frame, Idle::FORMAT);
            }
            w.frame(t0 + Duration::from_micros(16_667 * u64::from(i)));
        }
    }

    #[test]
    fn a_first_run_walks_every_stage_and_saves_as_the_bash_wizard_does() {
        let file = scratch("first", None);
        let sys = fake();
        let delay = sys.delay.clone();
        let mut w = wizard(sys, &file, true);
        assert_eq!(w.phase(), Phase::Choose);
        assert_eq!(selected(&w), "start");
        press(&mut w, &["Enter"]);
        // Card: the capture chip is recommended over the camera listed first.
        assert_eq!(w.stage, Stage::Card);
        assert_eq!(selected(&w), "1");
        assert!(w.lines().iter().any(|l| l.contains("UVC Camera (1e4e:7016)") && l.contains("◂ capture chip")), "{:#?}", w.lines());
        press(&mut w, &["Enter"]);
        // The card's details come straight after the pick.
        assert_eq!((w.stage, w.step.clone()), (Stage::Card, Step::CardDetails));
        assert!(w.lines().iter().any(|l| l.starts_with("OK: pinned by USB serial")), "{:#?}", w.lines());
        assert!(w.lines().iter().any(|l| l == "USB id  : 1e4e:7016 · MacroSilicon MS2130"), "{:#?}", w.lines());
        assert!(w.lines().iter().any(|l| l == "Port    : 3-2 · USB 3.20"));
        press(&mut w, &["Enter"]);
        // Then the camera: ignore it (yes by default).
        assert_eq!(w.step, Step::CardIgnore(0));
        press(&mut w, &["Enter"]);
        // Source: the PS5, then its tips.
        assert_eq!(w.stage, Stage::Source);
        assert_eq!(selected(&w), "generic");
        let ps5 = sources::ids().iter().position(|i| *i == "ps5").unwrap();
        press(&mut w, &[&(ps5 + 1).to_string()]);
        assert_eq!(w.step, Step::SourceTips);
        press(&mut w, &["Enter"]);
        // Format: the PS5's suggestion is the best mode.
        assert_eq!(w.stage, Stage::Format);
        assert_eq!(selected(&w), "yuyv422 1920x1080 60");
        assert!(w.lines().iter().any(|l| l.contains("◂ recommended · PlayStation 5 suggests this")), "{:#?}", w.lines());
        press(&mut w, &["Enter"]);
        // The next stage needs the feed: the window changes.
        assert_eq!(w.stage, Stage::Signal);
        assert!(w.wants_exit());
        let (n, f) = w.feed().unwrap();
        assert_eq!(n.path, std::path::Path::new("/dev/video2"));
        assert_eq!(f.unwrap().width, 1920);
        w.running = Phase::Feed;
        assert!(!w.wants_exit());
        signal_ok(&mut w);
        assert_eq!(w.step, Step::SignalResult, "{:#?}", w.lines());
        assert!(w.lines().iter().any(|l| l == "OK: 60.0 fps measured (1920x1080)"), "{:#?}", w.lines());
        press(&mut w, &["Enter"]);
        assert_eq!(w.stage, Stage::Audio);
        assert_eq!(selected(&w), "auto");
        press(&mut w, &["Enter"]);
        assert_eq!(w.stage, Stage::Display);
        assert_eq!(selected(&w), "fullscreen");
        press(&mut w, &["Enter"]);
        assert_eq!(w.stage, Stage::Refresh);
        assert_eq!(selected(&w), "match");
        press(&mut w, &["Enter"]);
        assert_eq!(w.stage, Stage::Player);
        assert_eq!(selected(&w), "balanced");
        press(&mut w, &["Enter"]);
        // Sync: the loopback starts at the current DELAY; 6 steps later is kept.
        assert_eq!(w.stage, Stage::Sync);
        press(&mut w, &["Enter"]);
        assert_eq!(w.step, Step::SyncTune);
        assert_eq!(delay.get(), 50);
        press(&mut w, &["k", "k", "l", "j", "Right", "Left"]);
        assert!(w.lines().iter().any(|l| l == "DELAY = 0.110"), "{:#?}", w.lines());
        press(&mut w, &["Enter"]);
        assert!(w.tuner.is_none(), "the loopback stops after the stage");
        press(&mut w, &["Enter"]);
        assert_eq!(w.stage, Stage::Debug);
        press(&mut w, &["Enter"]);
        assert_eq!(w.stage, Stage::Review);
        assert!(w.lines().iter().any(|l| l == "+ CARD_SERIAL=20000130041415"));
        assert!(w.lines().iter().any(|l| l == "+ IGNORE_CARDS_RE=OBSBOT Meet 2"));
        press(&mut w, &["Enter", "Enter"]);
        assert!(w.saved());
        assert_eq!(w.stage, Stage::Done);
        assert_eq!(selected(&w), "play");
        press(&mut w, &["Enter"]);
        assert_eq!(w.outcome(), Some(Outcome::Play));

        let saved = std::fs::read_to_string(&file).unwrap();
        assert_eq!(
            saved,
            "# protoHDMIcapture settings, written by the setup wizard.\n\
             # One KEY=value per line; see config.example in the repo.\n\
             SOURCE=ps5\nCARD_SERIAL=20000130041415\nIGNORE_CARDS_RE='OBSBOT Meet 2'\nRES=1920x1080\nFPS=60\n\
             FMT=yuyv422\nDELAY=0.110\nBACKEND=mpv\nWINDOW=fullscreen\nSCALING=balanced\nREFRESH=match\n"
        );
        // view.sh reads the wizard's file as saved and valid (Linux: view.sh is bash).
        #[cfg(target_os = "linux")]
        {
            let out = std::process::Command::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../view.sh"))
                .arg("--settings")
                .env("CONFIG_FILE", &file)
                .output()
                .unwrap();
            let report = String::from_utf8_lossy(&out.stdout);
            let row = |k: &str| {
                report.lines().find(|l| l.split_whitespace().next() == Some(k)).unwrap_or("").split_whitespace().last().unwrap_or("").to_string()
            };
            for k in ["SOURCE", "CARD_SERIAL", "IGNORE_CARDS_RE", "RES", "FPS", "FMT", "DELAY", "WINDOW", "SCALING", "REFRESH"] {
                assert_eq!(row(k), "saved", "{k}: {report}");
            }
            assert!(out.status.success() && report.contains("all valid"), "{report}");
        }
        std::fs::remove_dir_all(file.parent().unwrap()).unwrap();
    }

    /// The same answers through lib/setup.sh's apply_to on a copy of the
    /// same start: byte for byte, FULLSCREEN's removal included.
    #[test]
    #[cfg(target_os = "linux")] // runs bash, python or hyprctl
    fn rerunning_edits_the_file_as_apply_to_does() {
        let start = "# mine\nFULLSCREEN=no\nDEV=/dev/video9\nLEVELS=full\nBACKEND=gst\nDEBUG=log\n";
        let file = scratch("rerun", Some(start));
        let mut w = wizard(fake(), &file, false);
        assert_eq!(w.new["WINDOW"], "windowed", "FULLSCREEN=no reads as WINDOW=windowed");
        press(&mut w, &["Enter", "Enter"]); // welcome, card
        press(&mut w, &["n", "Enter", "Enter", "Enter", "Enter", "Enter"]); // keep the camera; usb; source; tips; format
        w.running = Phase::Feed;
        signal_ok(&mut w);
        press(&mut w, &["Enter", "2"]); // signal; audio: the mic
        assert_eq!(w.stage, Stage::Display);
        assert_eq!(selected(&w), "windowed");
        press(&mut w, &["Enter", "4"]); // windowed; custom size
        assert_eq!(w.step, Step::DisplayCustom);
        press(&mut w, &["1", "0", "2", "4", "q", "Enter"]);
        assert!(matches!(&w.ask, Ask::Input { invalid: Some(_), .. }), "1024q is refused");
        press(&mut w, &["BS", "x", "7", "6", "8", "Enter"]);
        assert_eq!(w.new["WINDOW_SIZE"], "1024x768");
        press(&mut w, &["2"]); // refresh: keep
        assert_eq!(w.step, Step::PlayerSwitch);
        press(&mut w, &["n"]); // stay on gst: no scaling question
        assert_eq!(w.stage, Stage::Sync);
        press(&mut w, &["n", "1"]); // no sync; debug off
        assert_eq!(w.stage, Stage::Review);
        assert!(w.lines().iter().any(|l| l == "- FULLSCREEN=no"));
        assert!(w.lines().iter().any(|l| l == "- DEBUG=log"));
        press(&mut w, &["Enter", "Enter"]);
        assert!(w.saved());
        let rust = std::fs::read_to_string(&file).unwrap();

        // bash: apply_to with the same NEW.
        let copy = file.with_file_name("bash-config");
        std::fs::write(&copy, start).unwrap();
        let mut set = Vec::new();
        let mut unset = Vec::new();
        for k in &w.managed {
            if w.new[k].is_empty() {
                unset.push(k.clone());
            } else {
                set.push(format!("{k}={}", w.new[k]));
            }
        }
        let ok = std::process::Command::new("bash")
            .arg("-c")
            .arg(r#". "$1/lib/card.sh"; shift; n=$1; shift; config_write "${@:1:$n}" && shift "$n" && config_unset "$@""#)
            .arg("bash")
            .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/.."))
            .arg(set.len().to_string())
            .args(&set)
            .args(&unset)
            .env("CONFIG_FILE", &copy)
            .status()
            .unwrap()
            .success();
        assert!(ok);
        assert_eq!(rust, std::fs::read_to_string(&copy).unwrap());
        assert!(rust.contains("WINDOW=windowed\n") && !rust.contains("FULLSCREEN") && !rust.contains("DEV="), "{rust}");
        assert!(rust.contains("SRC=alsa_input.mic\n") && rust.contains("BACKEND=gst\n") && rust.contains("LEVELS=full\n"), "{rust}");
        std::fs::remove_dir_all(file.parent().unwrap()).unwrap();
    }

    #[test]
    fn signal_failures_offer_a_retry_and_the_feed_failing_goes_back_to_the_mode() {
        let file = scratch("signal", Some("SOURCE=ps5\n"));
        let mut w = wizard(fake(), &file, false);
        press(&mut w, &["Enter", "Enter", "y", "Enter", "Enter", "Enter", "Enter", "Enter"]);
        assert_eq!(w.stage, Stage::Signal);
        // The card was busy: the format stage again, saying so.
        w.feed_failed("the card is busy: a feed is probably running. Close it first.".into());
        assert_eq!((w.stage, w.phase()), (Stage::Format, Phase::Choose));
        assert!(w.lines().iter().any(|l| l.starts_with("Failed: the card is busy")), "{:#?}", w.lines());
        press(&mut w, &["Enter"]);
        w.running = Phase::Feed;
        // No frames for 6 s.
        let t0 = Instant::now();
        assert!(!w.tick(t0));
        assert_eq!(w.next_change(t0), Some(t0 + SIGNAL_TIMEOUT));
        assert!(w.tick(t0 + SIGNAL_TIMEOUT));
        assert_eq!(w.step, Step::SignalRetry);
        assert!(w.lines().iter().any(|l| l == "Failed: No frames in 6 seconds: no signal, or HDCP is on at the source."));
        // Again: black frames at full rate.
        press(&mut w, &["Enter"]);
        assert_eq!(w.step, Step::SignalWait);
        let t1 = t0 + Duration::from_secs(10);
        w.tick(t1);
        let black = vec![16u8; Idle::FORMAT.frame_size()];
        for i in 0..=120u32 {
            w.picture(&black, Idle::FORMAT);
            w.frame(t1 + Duration::from_micros(16_667 * u64::from(i)));
        }
        assert_eq!(w.step, Step::SignalRetry);
        assert!(w.lines().iter().any(|l| l == "Warning: the picture is uniformly black."), "{:#?}", w.lines());
        // No: on to the audio, the result kept for the last screen.
        press(&mut w, &["n"]);
        assert_eq!(w.stage, Stage::Audio);
        assert!(w.signal_result.ends_with("but the picture is uniformly black"));
        // Backspace: the signal check again; twice more, the source.
        press(&mut w, &["BS"]);
        assert_eq!(w.stage, Stage::Signal);
        press(&mut w, &["BS", "BS"]);
        assert_eq!((w.stage, w.phase()), (Stage::Source, Phase::Choose));
        std::fs::remove_dir_all(file.parent().unwrap()).unwrap();
    }

    #[test]
    fn an_mjpeg_card_streaming_full_range_black_is_caught() {
        // The MS2109 streams black with nothing plugged in; its MJPEG frames
        // reach the wizard decoded to YUYV (#17), full-range black is luma 0.
        let file = scratch("mjpeg-black", Some("SOURCE=ps5\n"));
        let mut w = wizard(fake(), &file, false);
        press(&mut w, &["Enter", "Enter", "y", "Enter", "Enter", "Enter", "Enter", "Enter"]);
        assert_eq!(w.stage, Stage::Signal);
        w.new.insert("FMT".into(), "mjpeg".into());
        w.running = Phase::Feed;
        let t0 = Instant::now();
        w.tick(t0);
        let black = vec![0u8; Idle::FORMAT.frame_size()];
        for i in 0..=120u32 {
            w.picture(&black, Idle::FORMAT);
            w.frame(t0 + Duration::from_micros(16_667 * u64::from(i)));
        }
        assert_eq!(w.step, Step::SignalRetry);
        assert!(w.lines().iter().any(|l| l == "Warning: the picture is uniformly black."), "{:#?}", w.lines());
        std::fs::remove_dir_all(file.parent().unwrap()).unwrap();
    }

    #[test]
    fn frames_that_do_not_decode_are_not_called_no_signal() {
        // The MS2130 1e4e:7016's MJPEG: frames arrive, none decode.
        let file = scratch("undecodable", Some("SOURCE=ps5\n"));
        let mut w = wizard(fake(), &file, false);
        press(&mut w, &["Enter", "Enter", "y", "Enter", "Enter", "Enter", "Enter", "Enter"]);
        assert_eq!(w.stage, Stage::Signal);
        w.running = Phase::Feed;
        let t0 = Instant::now();
        w.tick(t0);
        let no_signal = |reason| Signal::NoSignal { since: crate::capture::Since::Never, reason };
        w.signal(&no_signal(Reason::Undecodable("not a JPEG".into())));
        assert!(w.tick(t0 + SIGNAL_TIMEOUT));
        assert_eq!(w.step, Step::SignalRetry);
        assert!(
            w.lines().iter().any(|l| l == "Failed: Frames arrive but none decode as MJPEG (not a JPEG)."),
            "{:#?}",
            w.lines()
        );
        // Again, with short buffers: the policy's reason words it.
        press(&mut w, &["Enter"]);
        let t1 = t0 + Duration::from_secs(10);
        w.tick(t1);
        w.signal(&no_signal(Reason::Unusable("10 bytes, not a 1280x720 YUYV frame".into())));
        assert!(w.tick(t1 + SIGNAL_TIMEOUT));
        // Wrapped at the page's width.
        assert!(
            w.lines().join(" ").contains("Failed: Frames arrive but the viewer cannot use them (10 bytes, not a 1280x720 YUYV frame)."),
            "{:#?}",
            w.lines()
        );
        // And with nothing arriving, the old words.
        press(&mut w, &["Enter"]);
        let t2 = t1 + Duration::from_secs(10);
        w.tick(t2);
        w.signal(&no_signal(Reason::Nothing));
        assert!(w.tick(t2 + SIGNAL_TIMEOUT));
        assert!(w.lines().iter().any(|l| l == "Failed: No frames in 6 seconds: no signal, or HDCP is on at the source."));
        std::fs::remove_dir_all(file.parent().unwrap()).unwrap();
    }

    #[test]
    fn too_few_frames_by_the_timeout_fail_the_signal_check() {
        // 15 fps for 60 requested: lib/setup.sh's `timeout 6` fails, so here.
        let file = scratch("slow", Some("SOURCE=ps5\n"));
        let mut w = wizard(fake(), &file, false);
        press(&mut w, &["Enter", "Enter", "y", "Enter", "Enter", "Enter", "Enter", "Enter"]);
        assert_eq!(w.stage, Stage::Signal);
        w.running = Phase::Feed;
        let t0 = Instant::now();
        w.tick(t0);
        for i in 0..90u32 {
            w.frame(t0 + Duration::from_micros(66_667 * u64::from(i)));
        }
        assert_eq!(w.step, Step::SignalWait);
        assert!(w.tick(t0 + SIGNAL_TIMEOUT));
        assert_eq!(w.step, Step::SignalRetry);
        assert!(w.signal_result.starts_with("failed: only 90 of 121 frames in 6 seconds (14.8 fps"), "{}", w.signal_result);
        std::fs::remove_dir_all(file.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_saved_delay_that_is_not_valid_is_said_so_in_the_tuner() {
        let file = scratch("baddelay", Some("SOURCE=ps5\nDELAY=abc\n"));
        let sys = fake();
        let delay = sys.delay.clone();
        let mut w = wizard(sys, &file, false);
        press(&mut w, &["Enter", "Enter", "y", "Enter", "Enter", "Enter", "Enter", "Enter"]);
        w.running = Phase::Feed;
        signal_ok(&mut w);
        press(&mut w, &["Enter", "Enter", "Enter", "Enter", "Enter", "Enter"]);
        assert_eq!(w.step, Step::SyncTune);
        assert_eq!(delay.get(), 50);
        assert!(w.lines().iter().any(|l| l.starts_with("DELAY=abc: ") && l.ends_with(": starting from 0.050")), "{:#?}", w.lines());
        // An unreadable settings file stops the wizard before it starts.
        assert!(Wizard::new(Box::new(fake()), file.parent().unwrap().to_path_buf(), false, &|_| false).is_err());
        std::fs::remove_dir_all(file.parent().unwrap()).unwrap();
    }

    #[test]
    fn the_format_page_fits_a_720_line_window() {
        // A long error, the suggestion's warning and tip, and 12 modes: the
        // lowest go, the saved one (the lowest of all) stays.
        let file = scratch("fit", Some("SOURCE=ps5\nFMT=yuyv422\nRES=640x480\nFPS=10\n"));
        let mut sys = fake();
        sys.modes = [(1280, 720), (640, 480)]
            .into_iter()
            .flat_map(|(w, h)| [60, 50, 30, 25, 20, 10].map(|f| mode("yuyv422", w, h, f)))
            .collect();
        let mut w = wizard(sys, &file, false);
        press(&mut w, &["Enter", "Enter", "y", "Enter", "Enter", "Enter", "Enter", "Enter"]);
        assert_eq!(w.stage, Stage::Signal);
        w.feed_failed("capture: /dev/video2: the card gives 1280x720 YUYV at 59.940 fps, not 60 (FPS), and something else went wrong too".into());
        assert_eq!(w.step, Step::FormatPick);
        let lines = w.lines();
        assert!(lines.len() <= PAGE_LINES, "{} lines: {lines:#?}", lines.len());
        assert!(lines.iter().any(|l| l.contains("lower modes left out")), "{lines:#?}");
        assert!(lines.iter().any(|l| l.contains("640x480 @ 10 fps") && l.contains("◂ current")), "{lines:#?}");
        assert!(lines.iter().any(|l| l.starts_with("Warning: ") && l.contains("suggests 1920x1080 @ 60")), "{lines:#?}");
        std::fs::remove_dir_all(file.parent().unwrap()).unwrap();
    }

    #[test]
    fn cancel_skip_and_quit() {
        // Esc: nothing written, nothing to play.
        let file = scratch("cancel", None);
        let mut w = wizard(fake(), &file, true);
        press(&mut w, &["Enter", "Esc"]);
        assert_eq!(w.stage, Stage::Done);
        assert!(w.lines().iter().any(|l| l == "Setup closed without saving"));
        press(&mut w, &["Enter"]);
        assert_eq!(w.outcome(), Some(Outcome::Close));
        assert!(!file.exists());

        // Skip on the first run: a file that says so, then play.
        let mut w = wizard(fake(), &file, true);
        press(&mut w, &["Down", "Enter"]);
        assert_eq!(w.outcome(), Some(Outcome::Play));
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.ends_with("# Setup was skipped; run ./view.sh --setup (or protohdmicapture --setup) to choose settings.\n"), "{text}");
        // view.sh then plays without asking: the file exists, and is valid
        // (Linux: view.sh is bash).
        #[cfg(target_os = "linux")]
        {
            let out = std::process::Command::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../view.sh"))
                .arg("--settings")
                .env("CONFIG_FILE", &file)
                .output()
                .unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
        }

        // q in the tuner: the loopback stops, the wizard ends unsaved.
        let mut w = wizard(fake(), &file, false);
        press(&mut w, &["Enter", "Enter", "y", "Enter", "Enter", "Enter", "Enter", "Enter"]);
        w.running = Phase::Feed;
        signal_ok(&mut w);
        press(&mut w, &["Enter", "Enter", "Enter", "Enter", "Enter", "Enter"]);
        assert_eq!(w.step, Step::SyncTune);
        press(&mut w, &["q"]);
        assert!(w.tuner.is_none());
        assert_eq!(w.outcome(), Some(Outcome::Close));
        assert!(!w.saved());

        // The card gone while tuning: the loopback stops with the feed.
        let mut w = wizard(fake(), &file, false);
        press(&mut w, &["Enter", "Enter", "y", "Enter", "Enter", "Enter", "Enter", "Enter"]);
        w.running = Phase::Feed;
        signal_ok(&mut w);
        press(&mut w, &["Enter", "Enter", "Enter", "Enter", "Enter", "Enter"]);
        assert_eq!(w.step, Step::SyncTune);
        assert!(w.tuner.is_some());
        w.feed_failed("capture: the card is gone".into());
        assert_eq!(w.stage, Stage::Format);
        assert!(w.tuner.is_none(), "the loopback stops when the feed fails");
        std::fs::remove_dir_all(file.parent().unwrap()).unwrap();
    }

    #[test]
    fn modes_best_first_and_the_closest_to_a_suggestion() {
        let modes = vec![
            mode("mjpeg", 1280, 720, 60),
            // Listed before its yuyv422 twin: must not hide it.
            mode("nv12", 1280, 720, 60),
            mode("yuyv422", 1920, 1080, 5),
            mode("mjpeg", 1920, 1080, 30),
            mode("yuyv422", 1280, 720, 10),
            mode("nv12", 1280, 720, 10),
            mode("yuyv422", 1280, 720, 60),
            mode("rgb", 640, 480, 60),
        ];
        let best = best_modes(&modes);
        let keys: Vec<String> = best.iter().map(Mode::key).collect();
        assert_eq!(
            keys,
            ["yuyv422 1280x720 60", "mjpeg 1280x720 60", "mjpeg 1920x1080 30", "yuyv422 1280x720 10", "yuyv422 1920x1080 5"],
            "fps, size, uncompressed first; nv12 is not offered: the Rust viewer cannot capture it"
        );
        assert!(is_fmt("yuyv422") && is_fmt("mjpeg") && !is_fmt("nv12"));
        assert_eq!(closest_mode(1920, 1080, 60, &best).unwrap().key(), "yuyv422 1280x720 60", "frame rate first");
        assert_eq!(closest_mode(1920, 1080, 30, &best).unwrap().key(), "mjpeg 1920x1080 30");
        assert_eq!(closest_mode(1920, 1080, 60, &[]), None);
    }

    /// The lib/setup.sh function `name`, as source text.
    fn bash_fn(name: &str) -> String {
        let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../lib/setup.sh")).unwrap();
        let start = text.find(&format!("\n{name}() {{")).unwrap() + 1;
        let rest = &text[start..];
        // One line (`f() { ...; }`), or up to the closing brace's line.
        let end = if rest.lines().next().unwrap().trim_end().ends_with('}') { rest.find('\n').unwrap() } else { rest.find("\n}\n").unwrap() + 2 };
        rest[..end].to_string()
    }

    fn bash(script: &str, args: &[&str]) -> String {
        let out = std::process::Command::new("bash").arg("-c").arg(script).arg("bash").args(args).output().unwrap();
        assert!(out.status.success(), "{script}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    #[test]
    #[cfg(target_os = "linux")] // runs bash, python or hyprctl
    fn ignore_patterns_as_lib_setup_sh_writes_them() {
        let lit = bash_fn("_regex_literal");
        for name in ["UVC Camera (1e4e:7016)", "Cam [x] 2.0*", "it's a $cam^ {1}|b", "a\\b+?"] {
            let b = bash(&format!("{lit}\n_regex_literal \"$1\""), &[name]);
            assert_eq!(regex_literal(name), b, "{name}");
            // The literal matches the name in Rust's regex, as in grep -E.
            assert!(matches(&regex_literal(name), name), "{name}");
        }
        let without = bash_fn("_ignore_without");
        for (pattern, name) in [
            ("OBSBOT|UVC Camera \\(1e4e:7016\\)|Meet", "UVC Camera (1e4e:7016)"),
            ("a\\|b|c", "a|b"),
            ("Meet|", "x"),
            ("Cam|Cam 2", "Cam 2"),
        ] {
            let b = bash(&format!("{without}\n_ignore_without \"$1\" \"$2\""), &[pattern, name]);
            assert_eq!(ignore_without(pattern, name), b, "{pattern} without {name}");
        }
    }

    #[test]
    fn the_cards_link_or_why_it_is_unknown() {
        let file = scratch("link", None);
        let mut w = wizard(fake(), &file, false);
        press(&mut w, &["Enter", "Enter"]);
        assert_eq!(w.step, Step::CardDetails);
        assert!(w.lines().iter().any(|l| l == "OK: link 5000 Mbit/s, enough for uncompressed 1080p60"), "{:#?}", w.lines());
        // Windows, the hub not asked (#36): the page says the step that failed.
        let mut w = wizard(Fake { link_unknown: "opening its hub: Access is denied.", ..fake() }, &file, false);
        press(&mut w, &["Enter", "Enter"]);
        assert!(w.lines().iter().any(|l| l == "Port    : 3-2"), "{:#?}", w.lines());
        assert!(w.lines().iter().any(|l| l == "Warning: link speed unknown: opening its hub: Access is denied."), "{:#?}", w.lines());
        std::fs::remove_dir_all(file.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_card_on_the_ignored_list_is_taken_off_it() {
        let file = scratch("ignored", Some("IGNORE_CARDS_RE='OBSBOT|UVC Camera'\n"));
        let mut w = wizard(fake(), &file, false);
        press(&mut w, &["Enter"]);
        assert!(w.lines().iter().any(|l| l.contains("UVC Camera (1e4e:7016)") && l.contains("◂ ignored · capture chip")), "{:#?}", w.lines());
        press(&mut w, &["Enter"]);
        // The details say it; the camera stays ignored, so nothing is asked
        // about it.
        assert_eq!(w.step, Step::CardDetails);
        assert!(w.lines().iter().any(|l| l == "Warning: this card was on the ignored list; it is taken off."));
        press(&mut w, &["Enter"]);
        assert_eq!(w.stage, Stage::Source);
        assert_eq!(w.new["IGNORE_CARDS_RE"], "OBSBOT");
        std::fs::remove_dir_all(file.parent().unwrap()).unwrap();
    }

    #[test]
    fn no_cards_and_no_modes() {
        let file = scratch("none", None);
        let mut sys = fake();
        sys.nodes.truncate(1);
        sys.nodes[0] = node("/dev/video4", "", "", "USB Video");
        sys.modes.clear();
        let mut w = wizard(sys, &file, false);
        press(&mut w, &["Enter", "Enter"]);
        assert_eq!(w.new["DEV"], "/dev/video4", "no serial, no port: pinned by node");
        press(&mut w, &["Enter", "Enter", "Enter"]); // details, source, tips
        assert_eq!(w.step, Step::FormatNone);
        press(&mut w, &["n"]);
        assert_eq!(w.step, Step::FormatRes);
        press(&mut w, &["Enter", "Enter", "m", "j", "p", "e", "g", "Enter"]);
        assert_eq!((w.new["RES"].as_str(), w.new["FPS"].as_str(), w.new["FMT"].as_str()), ("1920x1080", "60", "mjpeg"));
        assert_eq!(w.stage, Stage::Signal);
        std::fs::remove_dir_all(file.parent().unwrap()).unwrap();
    }
}
