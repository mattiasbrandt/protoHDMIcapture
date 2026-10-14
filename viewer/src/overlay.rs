//! The overlay's content and state, as the mpv script has them
//! (lib/mpv/protohdmicapture.lua): the control panel and the setup tips
//! (Tab), the key bar for the first seconds, short messages (mpv's OSD
//! messages), the no-signal banner, and what w would save. Text only: osd.rs
//! draws it.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use crate::sources::{self, Profile};

/// Tells the window something to show from another thread: a message and
/// how long, or None just to draw again (a value it shows changed).
pub type Notify = std::sync::Arc<dyn Fn(Option<(String, Duration)>) + Send + Sync>;

/// How long the key bar shows at start.
pub const KEY_BAR_FOR: Duration = Duration::from_secs(10);
/// No frame this long after the feed opened: "never".
pub const NEVER_AFTER: Duration = Duration::from_secs(5);
/// No frame this long after the last one: "stopped".
pub const STOPPED_AFTER: Duration = Duration::from_secs(2);
/// The width the tips wrap at, in characters.
const TIPS_WIDTH: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Panel,
    Tips,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoSignal {
    /// No frame since the feed opened.
    Never,
    /// Frames came, then stopped.
    Stopped,
}

/// Frame arrivals -> the banner's state.
#[derive(Debug, Clone, Copy)]
pub struct SignalWatch {
    opened: Instant,
    last_frame: Option<Instant>,
}

impl SignalWatch {
    pub fn new(opened: Instant) -> Self {
        Self { opened, last_frame: None }
    }

    pub fn frame(&mut self, at: Instant) {
        self.last_frame = Some(at);
    }

    pub fn state(&self, now: Instant) -> Option<NoSignal> {
        match self.last_frame {
            None if now - self.opened >= NEVER_AFTER => Some(NoSignal::Never),
            Some(t) if now - t >= STOPPED_AFTER => Some(NoSignal::Stopped),
            _ => None,
        }
    }

    /// When the state can next change without a frame arriving.
    fn next_change(&self, now: Instant) -> Option<Instant> {
        let at = match self.last_frame {
            None => self.opened + NEVER_AFTER,
            Some(t) => t + STOPPED_AFTER,
        };
        (at > now).then_some(at)
    }
}

/// What the overlay shows besides the feed.
pub struct Overlay {
    pub page: Option<Page>,
    /// The profile the tips page shows: SOURCE, else generic.
    pub tips_id: String,
    /// The wake hint of SOURCE's profile, for the banner.
    wake: Option<String>,
    message: Option<(String, Instant)>,
    key_bar_until: Option<Instant>,
    pub signal: SignalWatch,
}

impl Overlay {
    /// `source` is SOURCE (may be empty).
    pub fn new(source: &str, now: Instant) -> Self {
        let profile = (!source.is_empty()).then(|| sources::get(source)).flatten();
        Self {
            page: None,
            tips_id: if profile.is_some() { source.to_string() } else { "generic".into() },
            wake: profile.and_then(|p| p.wake),
            message: None,
            key_bar_until: Some(now + KEY_BAR_FOR),
            signal: SignalWatch::new(now),
        }
    }

    /// Tab: open the panel, or close whichever page is open.
    pub fn toggle(&mut self) {
        self.page = match self.page {
            None => Some(Page::Panel),
            Some(_) => None,
        };
        // Gone early once the overlay opens.
        if self.page.is_some() {
            self.key_bar_until = None;
        }
    }

    /// t: the tips page and back.
    pub fn toggle_tips(&mut self) {
        self.page = match self.page {
            Some(Page::Tips) => Some(Page::Panel),
            _ => Some(Page::Tips),
        };
    }

    /// Left/Right on the tips page: the previous or next profile, wrapping.
    pub fn cycle_source(&mut self, step: i32) {
        let ids = sources::ids();
        let i = ids.iter().position(|i| *i == self.tips_id);
        self.tips_id = match i {
            Some(i) => ids[(i as i32 + step).rem_euclid(ids.len() as i32) as usize].to_string(),
            None => ids[0].to_string(),
        };
    }

    /// A short message, replacing the one showing (mpv's osd_message).
    pub fn message(&mut self, text: impl Into<String>, for_: Duration, now: Instant) {
        self.message = Some((text.into(), now + for_));
    }

    pub fn current_message(&self, now: Instant) -> Option<&str> {
        self.message.as_ref().filter(|(_, until)| *until > now).map(|(t, _)| t.as_str())
    }

    pub fn key_bar_showing(&self, now: Instant) -> bool {
        self.key_bar_until.is_some_and(|t| t > now)
    }

    /// The banner's detail line, when it shows: never while a page is open
    /// (its box over the banner is unreadable, and it has the tips).
    pub fn no_signal(&self, now: Instant) -> Option<String> {
        if self.page.is_some() {
            return None;
        }
        self.signal.state(now).map(|s| no_signal_detail(s, self.wake.as_deref()))
    }

    /// Anything to draw besides the feed.
    pub fn visible(&self, now: Instant) -> bool {
        self.page.is_some()
            || self.current_message(now).is_some()
            || self.key_bar_showing(now)
            || self.no_signal(now).is_some()
    }

    /// The next moment something shown changes on its own: a message or
    /// the key bar going, the banner coming.
    pub fn next_change(&self, now: Instant) -> Option<Instant> {
        [
            self.message.as_ref().map(|(_, t)| *t).filter(|t| *t > now),
            self.key_bar_until.filter(|t| *t > now),
            self.signal.next_change(now),
        ]
        .into_iter()
        .flatten()
        .min()
    }
}

fn no_signal_detail(state: NoSignal, wake: Option<&str>) -> String {
    match state {
        NoSignal::Never => "Is the source on and connected?".into(),
        NoSignal::Stopped => match wake {
            Some(w) => {
                let mut c = w.chars();
                let first = c.next().map(|f| f.to_uppercase().collect::<String>()).unwrap_or_default();
                format!("The source stopped sending.  {first}{} to wake it.", c.as_str())
            }
            None => "The source stopped sending.  Is it asleep or switched off?".into(),
        },
    }
}

/// The key bar's text.
pub fn key_bar(loopback: bool) -> String {
    let mut keys = vec!["Tab menu", "f display"];
    if loopback {
        keys.extend(["m mute", "9/0 volume", "ctrl -/+ audio sync"]);
    }
    keys.extend(["s screenshot", "q quit"]);
    keys.join("   ·   ")
}

/// The audio rows' state.
pub enum Audio {
    /// AUDIO is not loopback: its value.
    Other(String),
    /// The loopback did not start.
    Failed(String),
    /// Running: the level (None until first read, or why it could not be
    /// read) and the sync.
    Running { level: Option<Result<crate::pw::Level, String>>, delay_ms: u32 },
}

/// What the panel shows.
pub struct Panel<'a> {
    pub display: &'a str,
    pub levels: &'a str,
    pub scaling: &'a str,
    /// contrast, brightness, gamma, saturation.
    pub colour: [i32; 4],
    pub audio: Audio,
    pub tips_name: String,
    pub changes: &'a [String],
}

pub fn panel_lines(p: &Panel) -> Vec<String> {
    let [c, b, g, s] = p.colour;
    let mut lines = vec![
        "protoHDMIcapture".to_string(),
        String::new(),
        format!("f     Display      {}", p.display),
        format!("l     Levels       {}", p.levels),
        format!("c     Scaling      {}", p.scaling),
        format!("1-8   Colour       contrast {c} · brightness {b} · gamma {g} · saturation {s}"),
    ];
    match &p.audio {
        Audio::Other(v) => lines.push(format!("      Audio        {}", if v.is_empty() { "?" } else { v })),
        Audio::Failed(why) => {
            lines.push(format!("m 9 0 Audio        not running, see the log: {why}"));
        }
        Audio::Running { level, delay_ms } => {
            let row = match level {
                None => "…".to_string(),
                Some(Err(e)) => format!("unavailable: {e}"),
                Some(Ok(l)) => format!("{}volume {}%", if l.mute { "muted · " } else { "" }, l.volume),
            };
            lines.push(format!("m 9 0 Audio        {row}"));
            lines.push(format!("^-/^+ Audio sync   {delay_ms} ms   (alt -/+: 50 ms steps)"));
        }
    }
    lines.extend([
        "r     Reset colour".to_string(),
        format!("t     Setup tips   {}", p.tips_name),
        format!(
            "w     Save         {}",
            if p.changes.is_empty() { "nothing changed".to_string() } else { p.changes.join(" ") }
        ),
        String::new(),
        // d (dashboard) stays unbound until #16.
        "s screenshot   Tab/Esc close   q quit".to_string(),
    ]);
    lines
}

/// Words onto lines of at most `width` characters (a longer word stands
/// alone).
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > width {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

pub fn tips_lines(id: &str, profile: Option<&Profile>) -> Vec<String> {
    let Some(p) = profile else {
        return vec![format!("No setup tips for '{id}'"), String::new(), "◂ ▸ other sources   t back".into()];
    };
    let level = p.status.split_whitespace().next().unwrap_or("");
    let mut out = vec![
        format!(
            "Setup tips · {}{}",
            p.name,
            if level.is_empty() || level == "reported" { String::new() } else { format!(" · {level}") }
        ),
        String::new(),
    ];
    for tip in &p.tips {
        for (i, l) in wrap(tip, TIPS_WIDTH).into_iter().enumerate() {
            out.push(format!("{}{l}", if i == 0 { "• " } else { "  " }));
        }
    }
    out.push(String::new());
    out.push("◂ ▸ other sources   t back   Tab/Esc close".into());
    out
}

/// The saveable settings' live values, as w would write them.
pub type Values = BTreeMap<&'static str, String>;

/// What w would save: each saveable setting (lib/settings.sh's save flag,
/// in its order) whose value differs from the baseline; DELAY only while
/// the loopback carries the sound.
pub fn changes(saveable: &[&str], baseline: &Values, now: &Values, loopback: bool) -> Vec<String> {
    saveable
        .iter()
        .filter(|k| **k != "DELAY" || loopback)
        .filter_map(|k| {
            let v = now.get(k)?;
            (baseline.get(k) != Some(v)).then(|| format!("{k}={v}"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pw::Level;

    fn panel(audio: Audio, changes: &[String]) -> Vec<String> {
        panel_lines(&Panel {
            display: "Fullscreen",
            levels: "auto",
            scaling: "balanced",
            colour: [1, -2, 0, 3],
            audio,
            tips_name: "PlayStation 5".into(),
            changes,
        })
    }

    #[test]
    fn the_panel_reads_as_the_mpv_scripts() {
        let lines = panel(
            Audio::Running { level: Some(Ok(Level { volume: 80, mute: true })), delay_ms: 50 },
            &["LEVELS=full".into(), "DELAY=0.060".into()],
        );
        assert_eq!(
            lines,
            [
                "protoHDMIcapture",
                "",
                "f     Display      Fullscreen",
                "l     Levels       auto",
                "c     Scaling      balanced",
                "1-8   Colour       contrast 1 · brightness -2 · gamma 0 · saturation 3",
                "m 9 0 Audio        muted · volume 80%",
                "^-/^+ Audio sync   50 ms   (alt -/+: 50 ms steps)",
                "r     Reset colour",
                "t     Setup tips   PlayStation 5",
                "w     Save         LEVELS=full DELAY=0.060",
                "",
                "s screenshot   Tab/Esc close   q quit",
            ]
        );
        let other = panel(Audio::Other("none".into()), &[]);
        assert_eq!(other[6], "      Audio        none");
        assert_eq!(other[9], "w     Save         nothing changed");
        assert_eq!(panel(Audio::Failed("no card source".into()), &[])[6], "m 9 0 Audio        not running, see the log: no card source");
        assert_eq!(panel(Audio::Running { level: None, delay_ms: 0 }, &[])[6], "m 9 0 Audio        …");
    }

    #[test]
    fn tips_wrap_with_a_hanging_indent() {
        let ps5 = sources::get("ps5").unwrap();
        let lines = tips_lines("ps5", Some(&ps5));
        assert_eq!(lines[0], "Setup tips · PlayStation 5 · tested");
        assert!(lines[2].starts_with("• HDCP: Off"));
        assert!(lines[3].starts_with("  "), "a long tip continues indented");
        assert!(lines.iter().all(|l| l.chars().count() <= TIPS_WIDTH + 2), "{lines:#?}");
        assert_eq!(lines.last().unwrap(), "◂ ▸ other sources   t back   Tab/Esc close");
        assert_eq!(tips_lines("nope", None)[0], "No setup tips for 'nope'");
        assert_eq!(wrap("aa bb cc", 5), ["aa bb", "cc"]);
    }

    #[test]
    fn w_saves_what_changed_since_the_start() {
        let saveable = ["DELAY", "WINDOW", "SCALING", "LEVELS", "BRIGHTNESS"];
        let base: Values = [("DELAY", "0.050"), ("WINDOW", "fullscreen"), ("LEVELS", "auto"), ("BRIGHTNESS", "0")]
            .map(|(k, v)| (k, v.to_string()))
            .into();
        let mut now = base.clone();
        assert!(changes(&saveable, &base, &now, true).is_empty());
        now.insert("LEVELS", "full".into());
        now.insert("DELAY", "0.060".into());
        assert_eq!(changes(&saveable, &base, &now, true), ["DELAY=0.060", "LEVELS=full"]);
        assert_eq!(changes(&saveable, &base, &now, false), ["LEVELS=full"], "DELAY only with the loopback");
    }

    #[test]
    fn the_banner_comes_and_goes_with_frames() {
        let t0 = Instant::now();
        let s = |o: &Overlay, at: Duration| o.no_signal(t0 + at);
        let mut o = Overlay::new("ps5", t0);
        assert_eq!(s(&o, Duration::from_secs(4)), None);
        assert_eq!(s(&o, NEVER_AFTER).as_deref(), Some("Is the source on and connected?"));
        o.signal.frame(t0 + Duration::from_secs(6));
        assert_eq!(s(&o, Duration::from_millis(7900)), None);
        assert_eq!(
            s(&o, Duration::from_secs(8)).as_deref(),
            Some("The source stopped sending.  Press the PS button on the controller to wake it.")
        );
        o.toggle();
        assert_eq!(s(&o, Duration::from_secs(8)), None, "hidden under the panel");
        let generic = Overlay::new("", t0);
        assert_eq!(generic.tips_id, "generic");
        let mut g = generic;
        g.signal.frame(t0);
        assert_eq!(
            s(&g, Duration::from_secs(3)).as_deref(),
            Some("The source stopped sending.  Is it asleep or switched off?")
        );
    }

    #[test]
    fn pages_messages_and_deadlines() {
        let t0 = Instant::now();
        let mut o = Overlay::new("ps5", t0);
        assert!(o.key_bar_showing(t0) && !o.key_bar_showing(t0 + KEY_BAR_FOR));
        assert_eq!(o.next_change(t0), Some(t0 + NEVER_AFTER));
        o.message("Volume 80%", Duration::from_millis(1500), t0);
        assert_eq!(o.current_message(t0 + Duration::from_secs(1)), Some("Volume 80%"));
        assert_eq!(o.current_message(t0 + Duration::from_secs(2)), None);
        assert_eq!(o.next_change(t0), Some(t0 + Duration::from_millis(1500)));
        o.toggle();
        assert_eq!(o.page, Some(Page::Panel));
        assert!(!o.key_bar_showing(t0), "opening hides the key bar");
        o.toggle_tips();
        assert_eq!(o.page, Some(Page::Tips));
        o.cycle_source(1);
        assert_eq!(o.tips_id, "switch");
        o.tips_id = "generic".into();
        o.cycle_source(1);
        assert_eq!(o.tips_id, sources::ids()[0], "wraps");
        o.cycle_source(-1);
        assert_eq!(o.tips_id, "generic");
        o.toggle();
        assert_eq!(o.page, None);
    }

    #[test]
    fn the_key_bar() {
        assert_eq!(key_bar(false), "Tab menu   ·   f display   ·   s screenshot   ·   q quit");
        assert!(key_bar(true).contains("m mute   ·   9/0 volume   ·   ctrl -/+ audio sync"));
    }
}
