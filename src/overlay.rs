//! The overlay's content and state, as the mpv script has them
//! (legacy/lib/mpv/protohdmicapture.lua): the control panel and the setup tips
//! (Tab), the key bar for the first seconds, short messages (mpv's OSD
//! messages), the no-signal banner, and what w would save. Text only: osd.rs
//! draws it.

use std::time::{Duration, Instant};

use crate::capture::{Reason, Signal, Since};
use crate::framegen::Mode;
use crate::scale::{Scaling, Upscaling};
use crate::sources::{self, Profile};

/// Tells the window something to show from another thread: a message and
/// how long, or None just to draw again (a value it shows changed).
pub type Notify = std::sync::Arc<dyn Fn(Option<(String, Duration)>) + Send + Sync>;

/// How long the key bar shows at start.
pub const KEY_BAR_FOR: Duration = Duration::from_secs(10);
/// The width the tips wrap at, in characters.
const TIPS_WIDTH: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Panel,
    Tips,
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
    /// The frame policy's signal state (capture/policy.rs): the banner.
    pub signal: Signal,
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
            signal: Signal::Picture,
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
    pub fn no_signal(&self) -> Option<String> {
        match &self.signal {
            _ if self.page.is_some() => None,
            Signal::Picture => None,
            Signal::NoSignal { since, reason } => Some(no_signal_detail(*since, reason, self.wake.as_deref())),
        }
    }

    /// Anything to draw besides the feed.
    pub fn visible(&self, now: Instant) -> bool {
        self.page.is_some() || self.current_message(now).is_some() || self.key_bar_showing(now) || self.no_signal().is_some()
    }

    /// The next moment something shown changes on its own: a message or
    /// the key bar going. The banner follows the policy's events.
    pub fn next_change(&self, now: Instant) -> Option<Instant> {
        [self.message.as_ref().map(|(_, t)| *t).filter(|t| *t > now), self.key_bar_until.filter(|t| *t > now)].into_iter().flatten().min()
    }
}

/// The banner's detail line: what the reason says to do.
fn no_signal_detail(since: Since, reason: &Reason, wake: Option<&str>) -> String {
    match (reason, since) {
        (Reason::Undecodable(why), _) => format!("The card's MJPEG frames do not decode: {why}. Pick a yuyv422 mode."),
        (Reason::Unusable(why), _) => format!("The card sends frames the viewer cannot use ({why})."),
        (Reason::Nothing, Since::Never) => "Is the source on and connected?".into(),
        (Reason::Nothing, Since::Stopped) => match wake {
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
    /// The loopback is not running: why (it did not start, or it stopped
    /// mid-feed).
    Failed(String),
    /// Running: the level (None until first read, or why it could not be
    /// read) and the sync.
    Running { level: Option<Result<crate::level::Level, String>>, delay_ms: u32 },
}

/// What the panel shows.
pub struct Panel<'a> {
    pub display: &'a str,
    pub levels: &'a str,
    /// SCALING, the resize filter: a detail, without a key (#85).
    pub scaling: &'a str,
    /// UPSCALING's method for people (`Upscaling::name`), and its line under
    /// the row (`upscaling_hint`), #85.
    pub upscaling: &'a str,
    pub upscaling_hint: Option<&'a str>,
    /// Frame generation's mode, or "off": a dev build's `g` row (#83);
    /// None, no row (a release build).
    pub framegen: Option<&'a str>,
    /// contrast, brightness, gamma, saturation.
    pub colour: [i32; 4],
    pub audio: Audio,
    pub tips_name: String,
    pub changes: &'a [String],
}

pub fn panel_lines(p: &Panel) -> Vec<String> {
    let [c, b, g, s] = p.colour;
    // The GPU enhancements lead the page (#85); the picture's details follow.
    let mut lines =
        vec!["protoHDMIcapture".to_string(), String::new(), "GPU enhancements".to_string(), format!("c     Upscaling    {}", p.upscaling)];
    if let Some(hint) = p.upscaling_hint {
        lines.push(format!("                   {hint}"));
    }
    if let Some(fg) = p.framegen {
        lines.push(format!("g     Framegen     {fg}"));
    }
    lines.extend([
        String::new(),
        format!("f     Display      {}", p.display),
        format!("l     Levels       {}", p.levels),
        format!("      Resizing     {}", p.scaling),
        format!("1-8   Colour       contrast {c} · brightness {b} · gamma {g} · saturation {s}"),
    ]);
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
        format!("w     Save         {}", if p.changes.is_empty() { "nothing changed".to_string() } else { p.changes.join(" ") }),
        "u     Setup wizard (the feed stops)".to_string(),
        String::new(),
        "s screenshot   d dashboard   Tab/Esc close   q quit".to_string(),
    ]);
    lines
}

/// Words onto lines of at most `width` characters (a longer word stands
/// alone).
pub(crate) fn wrap(text: &str, width: usize) -> Vec<String> {
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
        format!("Setup tips · {}{}", p.name, if level.is_empty() || level == "reported" { String::new() } else { format!(" · {level}") }),
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

/// The line under Upscaling's row (#85), from how the picture fits the
/// monitor (scale.rs `fit`): the overlay only suggests. `running` is the
/// Upscaling that runs (Off where it does not enlarge).
pub fn upscaling_hint(setting: Upscaling, running: Upscaling, fit: &crate::scale::Fit) -> Option<String> {
    match (setting, running) {
        (Upscaling::Off, _) if fit.enlarged => Some(format!("suggested: on, the picture is {}", fit.reason)),
        (Upscaling::Off, _) => None,
        (_, Upscaling::Off) if fit.enlarged => {
            Some(format!("{} works up to 2x a side: the resize filter enlarges this one ({})", setting.name(), fit.reason))
        }
        (_, Upscaling::Off) => Some(format!("does nothing here: the picture is {}", fit.reason)),
        _ => Some(format!("the picture is {}", fit.reason)),
    }
}

/// What runs on the picture (#80, #85), for the window's top-right corner
/// in dev builds, so a judging run and its screenshots say what is on:
/// frame generation's mode, and the Upscaling that runs (`upscaling`, Off
/// where the picture is not enlarged), else SCALING's preset whenever the
/// picture is scaled (`scaled`; at 1:1 every preset is the frame itself),
/// each by its own name. None when nothing, and always in a release build.
pub fn enhancement_label(framegen: Option<Mode>, upscaling: Upscaling, scaling: Scaling, scaled: bool) -> Option<String> {
    if !cfg!(feature = "dev") {
        return None;
    }
    let resize = if upscaling != Upscaling::Off {
        Some(format!("upscaling {}", upscaling.as_str()))
    } else {
        scaled.then(|| format!("scaling {}", scaling.as_str()))
    };
    let parts: Vec<String> = [framegen.map(|m| format!("framegen {}", m.as_str())), resize].into_iter().flatten().collect();
    (!parts.is_empty()).then(|| parts.join(" · "))
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::level::Level;

    #[test]
    fn the_label_names_frame_generation_upscaling_and_the_resize_filter_in_dev_builds_only() {
        let dev = cfg!(feature = "dev");
        for scaling in Scaling::ALL {
            let preset = format!("scaling {}", scaling.as_str());
            // 1:1, nothing on: no label.
            assert_eq!(enhancement_label(None, Upscaling::Off, scaling, false), None, "{scaling:?}");
            // Scaled by the resize filter: its preset.
            assert_eq!(enhancement_label(None, Upscaling::Off, scaling, true), dev.then(|| preset.clone()), "{scaling:?}");
            // Upscaling runs: it, not the resize filter.
            for up in [Upscaling::Fsr, Upscaling::Nis] {
                let want = format!("upscaling {}", up.as_str());
                assert_eq!(enhancement_label(None, up, scaling, true), dev.then(|| want.clone()), "{up:?}");
            }
            for mode in Mode::ALL {
                let fg = format!("framegen {}", mode.as_str());
                assert_eq!(enhancement_label(Some(mode), Upscaling::Off, scaling, false), dev.then(|| fg.clone()), "{mode:?}");
                assert_eq!(
                    enhancement_label(Some(mode), Upscaling::Fsr, scaling, true),
                    dev.then(|| format!("{fg} · upscaling fsr")),
                    "{mode:?}"
                );
            }
        }
    }
    #[test]
    fn upscalings_hint_only_suggests_and_says_when_it_does_nothing() {
        let fit = |m| crate::scale::fit((1920, 1080), m);
        let (k4, hd, small, wide) = (fit((3840, 2160)), fit((1920, 1080)), fit((1280, 720)), fit((5120, 2160)));
        // Off, enlarged: suggested.
        assert_eq!(
            upscaling_hint(Upscaling::Off, Upscaling::Off, &k4).as_deref(),
            Some("suggested: on, the picture is enlarged exactly 2x on this 3840x2160 monitor")
        );
        // Off, not enlarged: nothing to say.
        assert_eq!(upscaling_hint(Upscaling::Off, Upscaling::Off, &hd), None);
        assert_eq!(upscaling_hint(Upscaling::Off, Upscaling::Off, &small), None);
        // On and running: the fit.
        assert_eq!(
            upscaling_hint(Upscaling::Fsr, Upscaling::Fsr, &k4).as_deref(),
            Some("the picture is enlarged exactly 2x on this 3840x2160 monitor")
        );
        // On, 1:1 or a downscale: it does nothing.
        assert_eq!(
            upscaling_hint(Upscaling::Fsr, Upscaling::Off, &hd).as_deref(),
            Some("does nothing here: the picture is shown pixel for pixel on this 1920x1080 monitor")
        );
        assert!(upscaling_hint(Upscaling::Nis, Upscaling::Off, &small).unwrap().starts_with("does nothing here"));
        // NIS past 2x a side: the resize filter enlarges it.
        assert!(
            upscaling_hint(Upscaling::Nis, Upscaling::Off, &fit((4480, 2520))).unwrap().starts_with("NVIDIA Image Scaling works up to 2x")
        );
        assert_eq!(
            upscaling_hint(Upscaling::Nis, Upscaling::Nis, &wide).as_deref(),
            Some("the picture is enlarged exactly 2x on this 5120x2160 monitor, black bars at the sides")
        );
    }

    fn panel(audio: Audio, changes: &[String]) -> Vec<String> {
        panel_lines(&Panel {
            display: "Fullscreen",
            levels: "auto",
            scaling: "balanced",
            upscaling: "off",
            upscaling_hint: None,
            framegen: None,
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
                "GPU enhancements",
                "c     Upscaling    off",
                "",
                "f     Display      Fullscreen",
                "l     Levels       auto",
                "      Resizing     balanced",
                "1-8   Colour       contrast 1 · brightness -2 · gamma 0 · saturation 3",
                "m 9 0 Audio        muted · volume 80%",
                "^-/^+ Audio sync   50 ms   (alt -/+: 50 ms steps)",
                "r     Reset colour",
                "t     Setup tips   PlayStation 5",
                "w     Save         LEVELS=full DELAY=0.060",
                "u     Setup wizard (the feed stops)",
                "",
                "s screenshot   d dashboard   Tab/Esc close   q quit",
            ]
        );
        let other = panel(Audio::Other("none".into()), &[]);
        assert_eq!(other[9], "      Audio        none");
        assert_eq!(other[12], "w     Save         nothing changed");
        assert_eq!(panel(Audio::Failed("no card source".into()), &[])[9], "m 9 0 Audio        not running, see the log: no card source");
        assert_eq!(panel(Audio::Running { level: None, delay_ms: 0 }, &[])[9], "m 9 0 Audio        …");
    }

    #[test]
    fn the_gpu_enhancements_lead_the_page() {
        // Upscaling with its hint, then a dev build's frame generation (#83;
        // a release build passes none), then the picture's details (#85).
        let with = |fg| {
            panel_lines(&Panel {
                display: "Fullscreen",
                levels: "auto",
                scaling: "balanced",
                upscaling: "AMD FSR 1",
                upscaling_hint: Some("the picture is enlarged exactly 2x on this 3840x2160 monitor"),
                framegen: fg,
                colour: [0; 4],
                audio: Audio::Other("none".into()),
                tips_name: "PlayStation 5".into(),
                changes: &[],
            })
        };
        let on = with(Some("ffx-quality"));
        assert_eq!(
            on[2..8],
            [
                "GPU enhancements",
                "c     Upscaling    AMD FSR 1",
                "                   the picture is enlarged exactly 2x on this 3840x2160 monitor",
                "g     Framegen     ffx-quality",
                "",
                "f     Display      Fullscreen",
            ]
        );
        assert_eq!(with(Some("off"))[5], "g     Framegen     off");
        assert!(!with(None).iter().any(|l| l.contains("Framegen")));
        // The resize filter has no key.
        assert!(on.iter().any(|l| l == "      Resizing     balanced"));
        assert!(!on.iter().any(|l| l.starts_with("c     Scaling")));
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
    fn the_banner_follows_the_frame_policys_state() {
        let t0 = Instant::now();
        let no = |since, reason| Signal::NoSignal { since, reason };
        let mut o = Overlay::new("ps5", t0);
        assert_eq!(o.no_signal(), None);
        o.signal = no(Since::Never, Reason::Nothing);
        assert_eq!(o.no_signal().as_deref(), Some("Is the source on and connected?"));
        o.signal = no(Since::Stopped, Reason::Nothing);
        assert_eq!(o.no_signal().as_deref(), Some("The source stopped sending.  Press the PS button on the controller to wake it."));
        o.signal = no(Since::Never, Reason::Undecodable("not a JPEG".into()));
        assert_eq!(o.no_signal().as_deref(), Some("The card's MJPEG frames do not decode: not a JPEG. Pick a yuyv422 mode."));
        o.signal = no(Since::Stopped, Reason::Unusable("flagged as bad by the driver".into()));
        assert_eq!(o.no_signal().as_deref(), Some("The card sends frames the viewer cannot use (flagged as bad by the driver)."));
        o.toggle();
        assert_eq!(o.no_signal(), None, "hidden under the panel");
        o.toggle();
        o.signal = Signal::Picture;
        assert_eq!(o.no_signal(), None);
        let generic = Overlay::new("", t0);
        assert_eq!(generic.tips_id, "generic");
        let mut g = generic;
        g.signal = no(Since::Stopped, Reason::Nothing);
        assert_eq!(g.no_signal().as_deref(), Some("The source stopped sending.  Is it asleep or switched off?"));
    }

    #[test]
    fn pages_messages_and_deadlines() {
        let t0 = Instant::now();
        let mut o = Overlay::new("ps5", t0);
        assert!(o.key_bar_showing(t0) && !o.key_bar_showing(t0 + KEY_BAR_FOR));
        assert_eq!(o.next_change(t0), Some(t0 + KEY_BAR_FOR));
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
