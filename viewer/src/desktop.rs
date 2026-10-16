//! The desktop adapters (decided in #10): what depends on the compositor,
//! as lib/desktop.sh has it. Hyprland now, through `hyprctl` and its Lua
//! dispatchers (Omarchy configures Hyprland in Lua, which rejects
//! `hyprctl keyword` and the bare dispatch forms); any other desktop gets
//! the generic adapter, which leaves the window to the toolkit and the
//! display alone.
//!
//! Refresh matching keeps lib/desktop.sh's record (`refresh.restore`,
//! `monitor|rate set|original rate|restore command`, docs/run-dir.md), so
//! view.sh and this viewer each recover a switch the other was killed in.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

use serde_json::Value;

/// DESKTOP, else Hyprland when its session is running and hyprctl is there.
pub fn detect() -> Result<Desktop, String> {
    match std::env::var("DESKTOP").ok().filter(|v| !v.is_empty()).as_deref() {
        Some("hyprland") => Ok(Desktop::Hyprland(Hyprland::new("hyprctl"))),
        Some("generic") => Ok(Desktop::Generic),
        Some(other) => Err(format!("unknown DESKTOP={other} (hyprland | generic)")),
        None => {
            let session = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some_and(|v| !v.is_empty());
            let on_path = std::env::var_os("PATH")
                .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join("hyprctl").is_file()));
            Ok(if session && on_path { Desktop::Hyprland(Hyprland::new("hyprctl")) } else { Desktop::Generic })
        }
    }
}

#[derive(Clone)]
pub enum Desktop {
    Hyprland(Hyprland),
    Generic,
}

/// What a step did, for the log: Ok to tell, Err to warn about.
pub type Report = Result<String, String>;

impl Desktop {
    /// Raises the window of the process `pid`; false if this desktop cannot.
    pub fn focus_pid(&self, pid: u32) -> bool {
        match self {
            Self::Hyprland(h) => h.dispatch(&format!("hl.dsp.focus({{ window = \"pid:{pid}\" }})")),
            Self::Generic => false,
        }
    }

    /// Shapes the window of `pid` for `mode` (fullscreen, borderless,
    /// windowed at `size`) where the toolkit's own requests are ignored.
    /// Blocks: up to 3 s for the window to map, then a few dispatches.
    pub fn place_window(&self, pid: u32, mode: &str, size: (u32, u32)) -> Report {
        match self {
            Self::Hyprland(h) => h.place_window(pid, mode, size),
            Self::Generic => Ok("window: left to the toolkit's own requests".into()),
        }
    }

    /// REFRESH=match: the focused monitor to its mode nearest `fps`, the
    /// way back recorded in `record` first. Some(restore command) when a
    /// switch was made.
    pub fn refresh_match(&self, fps: f64, record: &Path) -> (Option<String>, Report) {
        match self {
            Self::Hyprland(h) => h.refresh_match(fps, record),
            Self::Generic => (None, Err("REFRESH=match is only implemented for Hyprland; leaving the display alone".into())),
        }
    }

    /// Undoes a switch; the record goes only once the undo worked.
    pub fn refresh_restore(&self, restore: &str, record: &Path) -> Report {
        match self {
            Self::Hyprland(h) => h.refresh_restore(restore, record),
            Self::Generic => Ok(String::new()),
        }
    }

    /// A record a killed session left: restored if the monitor is still at
    /// the rate it set, dropped if changed since, kept when unsure.
    pub fn refresh_recover(&self, record: &Path) -> Report {
        match self {
            Self::Hyprland(h) => h.refresh_recover(record),
            Self::Generic => {
                Err(format!("a killed session's refresh record is left for the hyprland adapter: {}", record.display()))
            }
        }
    }
}

#[derive(Clone)]
pub struct Hyprland {
    /// The program and any leading arguments (a test runs a stand-in
    /// through `sh`).
    hyprctl: Vec<PathBuf>,
}

impl Hyprland {
    pub fn new(hyprctl: impl Into<PathBuf>) -> Self {
        Self { hyprctl: vec![hyprctl.into()] }
    }

    /// hyprctl's output, or why it failed (its own words when it gave any).
    fn run(&self, args: &[&str]) -> Result<String, String> {
        let out = Command::new(&self.hyprctl[0])
            .args(&self.hyprctl[1..])
            .args(args)
            .output()
            .map_err(|e| format!("cannot run {}: {e}", self.hyprctl[0].display()))?;
        if out.status.success() {
            return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
        }
        let said = String::from_utf8_lossy(if out.stderr.is_empty() { &out.stdout } else { &out.stderr }).trim().to_string();
        Err(if said.is_empty() { format!("hyprctl {} exited with {}", args[0], out.status) } else { said })
    }

    fn dispatch(&self, lua: &str) -> bool {
        self.run(&["dispatch", lua]).is_ok()
    }

    fn eval(&self, lua: &str) -> Result<(), String> {
        self.run(&["eval", lua]).map(|_| ())
    }

    fn json(&self, what: &str) -> Option<Value> {
        serde_json::from_str(&self.run(&[what, "-j"]).ok()?).ok()
    }

    /// desktop_place_window: each mode first undoes what the others set,
    /// so it also serves switching modes on a live window (f).
    fn place_window(&self, pid: u32, mode: &str, (w, h): (u32, u32)) -> Report {
        let mapped = |c: &Value| c.as_array().is_some_and(|a| a.iter().any(|c| c["pid"].as_u64() == Some(u64::from(pid))));
        let mut tries = 0;
        while !self.json("clients").is_some_and(|c| mapped(&c)) {
            tries += 1;
            if tries == 30 {
                return Err(format!("window: pid {pid} never mapped a window"));
            }
            thread::sleep(Duration::from_millis(100));
        }
        let sel = format!("pid:{pid}");
        let prop = |p: &str, v: &str| format!("hl.dsp.window.set_prop({{ prop = \"{p}\", value = \"{v}\", window = \"{sel}\" }})");
        let full = |m: &str, a: &str| format!("hl.dsp.window.fullscreen({{ mode = \"{m}\", action = \"{a}\", window = \"{sel}\" }})");
        let (steps, said) = match mode {
            "fullscreen" => (
                vec![prop("border_size", "unset"), prop("rounding", "unset"), full("fullscreen", "set")],
                "window: fullscreen".to_string(),
            ),
            "windowed" => (
                vec![
                    full("fullscreen", "unset"),
                    full("maximized", "unset"),
                    prop("border_size", "unset"),
                    prop("rounding", "unset"),
                    format!("hl.dsp.window.float({{ action = \"enable\", window = \"{sel}\" }})"),
                    format!("hl.dsp.window.resize({{ x = {w}, y = {h}, window = \"{sel}\" }})"),
                    format!("hl.dsp.window.center({{ window = \"{sel}\" }})"),
                ],
                format!("window: floating {w}x{h}, centred"),
            ),
            "borderless" => (
                vec![full("fullscreen", "unset"), prop("border_size", "0"), prop("rounding", "0"), full("maximized", "set")],
                "window: maximized, no border".to_string(),
            ),
            other => return Err(format!("window: no mode {other}")),
        };
        // As lib/desktop.sh: dispatch results are not checked (hyprctl
        // reports a refused dispatch as success).
        for s in &steps {
            self.dispatch(s);
        }
        Ok(said)
    }

    fn refresh_match(&self, fps: f64, record: &Path) -> (Option<String>, Report) {
        let plan = match self.json("monitors").ok_or("could not read hyprctl monitors".to_string()).and_then(|m| plan(&m, fps)) {
            Ok(p) => p,
            Err(why) => {
                return (None, Err(format!("could not work out a refresh plan ({why}); leaving the display alone")));
            }
        };
        if plan.current == plan.chosen {
            return (None, Ok(format!("display: {} already at {}Hz, nothing to change", plan.name, plan.current)));
        }
        // Recorded before the switch: a kill between the two must not lose
        // the way back (recovery sorts out which it was).
        let line = format!("{}|{}|{}|{}\n", plan.name, plan.chosen, plan.current, plan.restore);
        if let Err(e) = fs::write(record, line) {
            return (None, Err(format!("cannot write {}: {e}; leaving the display alone", record.display())));
        }
        if let Err(why) = self.eval(&plan.target) {
            return (None, Err(format!("refresh switch failed ({why}); leaving the display alone{}", removed(record))));
        }
        (Some(plan.restore), Ok(format!("display: {} {}Hz -> {}Hz (restored on exit)", plan.name, plan.current, plan.chosen)))
    }

    fn refresh_restore(&self, restore: &str, record: &Path) -> Report {
        match self.eval(restore) {
            Ok(()) => match removed(record) {
                gone if gone.is_empty() => Ok("display: refresh rate restored".into()),
                kept => Err(format!("display: refresh rate restored{kept}")),
            },
            Err(why) => Err(format!(
                "could not restore the display's refresh rate ({why}; the next run retries); now: hyprctl eval '{restore}'"
            )),
        }
    }

    fn refresh_recover(&self, record: &Path) -> Report {
        let text = match fs::read_to_string(record) {
            Ok(t) => t,
            // Kept: an unreadable file is not proof the record is bad.
            Err(e) => return Err(format!("cannot read the refresh record {}: {e}; kept", record.display())),
        };
        let fields: Vec<&str> = text.lines().next().unwrap_or("").splitn(4, '|').collect();
        let [name, set, was, restore] = fields[..] else {
            return Err(format!("unreadable refresh record {}; dropped{}", record.display(), removed(record)));
        };
        if restore.is_empty() {
            return Err(format!("unreadable refresh record {}; dropped{}", record.display(), removed(record)));
        }
        let keep = format!("(or: hyprctl eval '{restore}'; rm {})", record.display());
        let now = self
            .json("monitors")
            .and_then(|m| m.as_array()?.iter().find(|m| m["name"].as_str() == Some(name))?["refreshRate"].as_f64());
        let Some(now) = now else {
            return Err(format!(
                "a killed session left {name} at {set}Hz, but its rate cannot be read now; kept for the next run {keep}"
            ));
        };
        let set_rate: f64 = set.parse().unwrap_or(f64::NAN);
        // As the awk test: within 0.01 Hz.
        if (now - set_rate).powi(2) >= 0.0001 || set_rate.is_nan() {
            let said = format!("display: {name} is at {}Hz, not the {set}Hz a killed session set; left as it is", fmt_g(now));
            return settled(said, record);
        }
        if let Err(why) = self.eval(restore) {
            return Err(format!(
                "could not restore {name} to {was}Hz after a killed session ({why}); kept for the next run {keep}"
            ));
        }
        settled(format!("display: {name} {set}Hz -> {was}Hz, restored after a killed session"), record)
    }
}

/// Removes a settled record: "" when gone, else why it stays (appended to
/// a message, as it then blocks the next REFRESH=match).
fn removed(record: &Path) -> String {
    match fs::remove_file(record) {
        Ok(()) => String::new(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => format!("; but {} cannot be removed ({e}) and blocks REFRESH=match until it is", record.display()),
    }
}

/// `said`, and the record removed now that it is settled.
fn settled(said: String, record: &Path) -> Report {
    match removed(record) {
        gone if gone.is_empty() => Ok(said),
        kept => Err(format!("{said}{kept}")),
    }
}

/// `notify-send`, where there is one: a launcher has no terminal to show
/// a message in (lib/desktop.sh desktop_notify). No notify-send is no
/// notification, as there; any other failure is said on stderr.
pub fn notify(urgency: &str, summary: &str, body: &str) {
    if let Err(e) = send("notify-send", urgency, summary, body) {
        eprintln!("warning: desktop notification \"{summary}\": {e}");
    }
}

fn send(program: &str, urgency: &str, summary: &str, body: &str) -> Result<(), String> {
    // --: a message starting with "-" is text, not an option.
    match Command::new(program).args(["-a", "protoHDMIcapture", "-u", urgency, "--", summary, body]).status() {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(format!("{program} {s}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("{program}: {e}")),
    }
}

/// What the desktop is told when there is no terminal to read stderr in
/// (started from a menu): view.sh's `die` and `audio_failed`.
#[derive(Debug, PartialEq)]
pub enum Notice {
    /// The viewer could not start or went down: why.
    Failed(String),
    /// The feed plays without sound: why.
    NoSound(String),
}

impl Notice {
    /// Urgency, summary and body; none on a terminal, where stderr has it.
    pub fn for_desktop(&self, terminal: bool) -> Option<(&'static str, &'static str, &str)> {
        if terminal {
            return None;
        }
        Some(match self {
            Notice::Failed(why) => ("critical", "protoHDMIcapture failed", why),
            Notice::NoSound(why) => ("normal", "protoHDMIcapture: no sound", why),
        })
    }

    /// Sent unless stderr is a terminal.
    pub fn send(&self) {
        if let Some((urgency, summary, body)) = self.for_desktop(std::io::IsTerminal::is_terminal(&std::io::stderr())) {
            notify(urgency, summary, body);
        }
    }
}

/// pick-refresh-mode.py's answer.
#[derive(Debug, PartialEq)]
pub struct Plan {
    pub name: String,
    /// Lua for `hyprctl eval`: the chosen mode, and the way back.
    pub target: String,
    pub restore: String,
    /// The rates as Python's `:g` prints them.
    pub current: String,
    pub chosen: String,
}

impl Plan {
    /// The script's output line.
    #[cfg(test)]
    fn line(&self) -> String {
        format!("{}|{}|{}|{}|{}", self.name, self.target, self.restore, self.current, self.chosen)
    }
}

/// lib/pick-refresh-mode.py: on the focused monitor, among its modes at the
/// same resolution (never a resolution change: it would rearrange the
/// layout), the rate nearest `fps`; position, scale and transform kept.
pub fn plan(monitors: &Value, fps: f64) -> Result<Plan, String> {
    let m = monitors
        .as_array()
        .and_then(|a| a.iter().find(|m| m["focused"].as_bool() == Some(true)))
        .ok_or("no focused monitor reported")?;
    let (width, height) = (m["width"].as_i64().ok_or("no width")?, m["height"].as_i64().ok_or("no height")?);
    let current = m["refreshRate"].as_f64().ok_or("no refresh rate")?;
    let mut best: Option<f64> = None;
    for mode in m["availableModes"].as_array().into_iter().flatten().filter_map(Value::as_str) {
        // ^(\d+)x(\d+)@([\d.]+)Hz$
        let Some((size, rate)) = mode.strip_suffix("Hz").and_then(|s| s.split_once('@')) else { continue };
        let Some((w, h)) = size.split_once('x') else { continue };
        let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        if !digits(w) || !digits(h) || rate.is_empty() || !rate.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
            continue;
        }
        let (Ok(w), Ok(h), Ok(rate)) = (w.parse::<i64>(), h.parse::<i64>(), rate.parse::<f64>()) else { continue };
        if (w, h) != (width, height) {
            continue;
        }
        if best.is_none_or(|b| (rate - fps).abs() < (b - fps).abs()) {
            best = Some(rate);
        }
    }
    let best = best.ok_or("no mode at the current resolution")?;
    let name = m["name"].as_str().ok_or("no name")?.to_string();
    // Python's str() of the JSON numbers: ints as ints, floats as repr.
    let num = |v: &Value| v.as_number().map(ToString::to_string).unwrap_or_default();
    let position = format!("{}x{}", num(&m["x"]), num(&m["y"]));
    let scale = if m["scale"].is_null() { "1".to_string() } else { num(&m["scale"]) };
    let transform = &m["transform"];
    let extra = if transform.as_f64().is_some_and(|t| t != 0.0) { format!(", transform = {}", num(transform)) } else { String::new() };
    let lua = |rate: f64| {
        format!(
            "hl.monitor({{ output = \"{name}\", mode = \"{width}x{height}@{}\", position = \"{position}\", scale = {scale}{extra} }})",
            fmt_g(rate)
        )
    };
    Ok(Plan { target: lua(best), restore: lua(current), current: fmt_g(current), chosen: fmt_g(best), name })
}

/// Python's `format(x, "g")`: six significant digits, trailing zeros
/// dropped, an exponent outside 1e-4..1e6.
pub fn fmt_g(x: f64) -> String {
    if x == 0.0 {
        return "0".into();
    }
    if !x.is_finite() {
        return if x.is_nan() { "nan".into() } else if x > 0.0 { "inf".into() } else { "-inf".into() };
    }
    // The exponent after rounding to six digits (9.999995 -> 10).
    let sci = format!("{:.5e}", x);
    let (mantissa, exp) = sci.split_once('e').expect("{:e} has an exponent");
    let exp: i32 = exp.parse().expect("an integer exponent");
    let trim = |s: String| if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s };
    if (-4..6).contains(&exp) {
        trim(format!("{:.*}", (5 - exp) as usize, x))
    } else {
        format!("{}e{}{:02}", trim(mantissa.to_string()), if exp < 0 { '-' } else { '+' }, exp.abs())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn g_formats_as_python() {
        for (x, want) in [
            (60.0, "60"),
            (59.95, "59.95"),
            (59.950001, "59.95"),
            (143.998, "143.998"),
            (59.940059, "59.9401"),
            (100.0, "100"),
            (1234567.0, "1.23457e+06"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (9.9999996, "10"),
        ] {
            assert_eq!(fmt_g(x), want, "{x}");
        }
    }

    fn monitors() -> Value {
        serde_json::json!([
            {"name": "DP-1", "width": 3440, "height": 1440, "refreshRate": 100.0, "focused": false,
             "x": 0, "y": 0, "scale": 1.0, "transform": 0,
             "availableModes": ["3440x1440@100.00Hz", "3440x1440@59.97Hz"]},
            {"name": "DP-2", "width": 1920, "height": 1200, "refreshRate": 59.950001, "focused": true,
             "x": 3440, "y": 120, "scale": 1.25, "transform": 1,
             "availableModes": ["1920x1200@59.95Hz", "1920x1080@60.00Hz", "1920x1200@60.00Hz", "1920x1200@50.00Hz", "bogus"]}
        ])
    }

    /// The same answer as lib/pick-refresh-mode.py, byte for byte.
    #[test]
    fn the_plan_matches_pick_refresh_mode_py() {
        let script = concat!(env!("CARGO_MANIFEST_DIR"), "/../lib/pick-refresh-mode.py");
        let mut cases = vec![(monitors(), 60.0), (monitors(), 50.0), (monitors(), 59.94)];
        let mut plain = monitors();
        plain[1]["scale"] = serde_json::json!(1);
        plain[1]["transform"] = serde_json::json!(0);
        cases.push((plain, 60.0));
        for (m, fps) in cases {
            let mut child = Command::new("python3")
                .args(["-I", script, &fps.to_string()])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            use std::io::Write as _;
            child.stdin.take().unwrap().write_all(m.to_string().as_bytes()).unwrap();
            let out = child.wait_with_output().unwrap();
            assert!(out.status.success());
            let py = String::from_utf8(out.stdout).unwrap();
            assert_eq!(plan(&m, fps).unwrap().line(), py.trim_end(), "fps {fps}");
        }
        let mut none = monitors();
        none[1]["focused"] = serde_json::json!(false);
        assert_eq!(plan(&none, 60.0).unwrap_err(), "no focused monitor reported");
    }

    /// A stand-in hyprctl, as tools/harness/refresh-record.sh has: the
    /// monitors it reports, whether eval works, and a log of what it ran.
    fn fake(dir: &Path, rate: Option<f64>, eval_ok: bool) -> Hyprland {
        fs::create_dir_all(dir).unwrap();
        let monitors = match rate {
            Some(r) => {
                let mut m = monitors();
                m[1]["refreshRate"] = serde_json::json!(r);
                m.to_string()
            }
            None => "[]".into(),
        };
        let script = format!(
            "#!/bin/sh\necho \"$*\" >>'{log}'\ncase \"$1\" in\nmonitors) cat <<'EOF'\n{monitors}\nEOF\n;;\neval) [ {rc} = 0 ] || echo 'config error: bad monitor' >&2; exit {rc} ;;\nesac\n",
            log = dir.join("log").display(),
            rc = if eval_ok { 0 } else { 1 }
        );
        let path = dir.join("hyprctl");
        fs::write(&path, script).unwrap();
        // Run through sh, never exec'd: a script just written can fail to
        // exec (ETXTBSY) while another test thread forks.
        Hyprland { hyprctl: vec!["sh".into(), path] }
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("phc-desktop-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn recovery_as_lib_desktop_sh_does_it() {
        // recover RATE EVAL_OK -> (ok, record kept), the harness's cases.
        let recover = |name: &str, rate: Option<f64>, eval_ok: bool| {
            let dir = tmp(name);
            let h = fake(&dir, rate, eval_ok);
            let rec = dir.join("refresh.restore");
            fs::write(&rec, "DP-2|59.97|60|hl.monitor({})\n").unwrap();
            let r = Desktop::Hyprland(h).refresh_recover(&rec);
            let out = (r.is_ok(), rec.exists());
            fs::remove_dir_all(&dir).unwrap();
            out
        };
        assert_eq!(recover("unreadable", None, true), (false, true), "monitor unreadable: kept");
        assert_eq!(recover("failed", Some(59.973), false), (false, true), "restore failed: kept");
        assert_eq!(recover("restored", Some(59.973), true), (true, false), "restored: removed");
        assert_eq!(recover("by-hand", Some(84.964), true), (true, false), "changed by hand: removed");

        let dir = tmp("garbage");
        let rec = dir.join("refresh.restore");
        let h = fake(&dir, Some(60.0), true);
        fs::write(&rec, "nonsense\n").unwrap();
        assert!(Desktop::Hyprland(h).refresh_recover(&rec).is_err());
        assert!(!rec.exists(), "an unreadable record is dropped");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn match_records_first_and_restores() {
        let dir = tmp("match");
        let h = Desktop::Hyprland(fake(&dir, Some(59.950001), true));
        let rec = dir.join("refresh.restore");
        let (restore, report) = h.refresh_match(60.0, &rec);
        assert_eq!(report.unwrap(), "display: DP-2 59.95Hz -> 60Hz (restored on exit)");
        let restore = restore.unwrap();
        let line = fs::read_to_string(&rec).unwrap();
        assert_eq!(line, format!("DP-2|60|59.95|{restore}\n"), "view.sh's record format");
        assert!(restore.contains("mode = \"1920x1200@59.95\""));
        let log = fs::read_to_string(dir.join("log")).unwrap();
        assert!(log.contains("eval hl.monitor({ output = \"DP-2\", mode = \"1920x1200@60\""), "{log}");
        assert!(h.refresh_restore(&restore, &rec).is_ok());
        assert!(!rec.exists());

        // Already there: nothing switched, nothing recorded.
        let (none, report) = h.refresh_match(59.95, &rec);
        assert_eq!((none, report.unwrap()), (None, "display: DP-2 already at 59.95Hz, nothing to change".to_string()));
        assert!(!rec.exists());
        fs::remove_dir_all(&dir).unwrap();

        // A failed switch leaves no record; a failed restore keeps it.
        let dir = tmp("match-fail");
        let h = Desktop::Hyprland(fake(&dir, Some(59.950001), false));
        let (none, report) = h.refresh_match(60.0, &rec_in(&dir));
        assert!(none.is_none() && !rec_in(&dir).exists());
        assert!(report.unwrap_err().contains("(config error: bad monitor)"), "hyprctl's reason is shown");
        fs::write(rec_in(&dir), "x").unwrap();
        assert!(h.refresh_restore("hl.monitor({})", &rec_in(&dir)).is_err());
        assert!(rec_in(&dir).exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    fn rec_in(dir: &Path) -> PathBuf {
        dir.join("refresh.restore")
    }

    #[test]
    fn the_desktop_is_told_only_without_a_terminal() {
        let failed = Notice::Failed("no capture card found".into());
        let no_sound = Notice::NoSound("the card's audio input is not there".into());
        assert_eq!(failed.for_desktop(true), None, "stderr has it");
        assert_eq!(no_sound.for_desktop(true), None);
        assert_eq!(failed.for_desktop(false), Some(("critical", "protoHDMIcapture failed", "no capture card found")));
        assert_eq!(
            no_sound.for_desktop(false),
            Some(("normal", "protoHDMIcapture: no sound", "the card's audio input is not there"))
        );
    }

    #[test]
    #[cfg(unix)]
    fn no_notify_send_is_no_notification_and_a_failing_one_says_so() {
        assert_eq!(send("/nonexistent/notify-send", "normal", "s", "b"), Ok(()));
        assert_eq!(send("true", "normal", "s", "b"), Ok(()));
        let e = send("false", "normal", "s", "b").unwrap_err();
        assert!(e.starts_with("false exit status: 1"), "{e}");
    }
}
