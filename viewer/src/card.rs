//! Which capture card to open: the same rules as view.sh's `resolve_card`
//! (lib/card.sh). The first video capture node that is not an ignored
//! device, narrowed by the pins (`CARD_SERIAL`, else `CARD_PORT`); a `DEV`
//! that is gone, not a capture node, or ignored is set aside for the pins.
//!
//! It is also the capture card's platform seam (#43): every question about
//! a card (its nodes, its USB link, its modes, its own audio input, who
//! holds it, opening it) is one function here, split by platform inside:
//! V4L2, sysfs and PipeWire on Linux, Media Foundation and WASAPI on
//! Windows (capture/mf.rs, #18). Callers ask this module, never the
//! platform's.

#[cfg(target_os = "linux")]
use std::fs;
#[cfg(target_os = "linux")]
use std::path::Path;
use std::path::PathBuf;

use crate::capture::{Format, FrameSource};

#[cfg(target_os = "linux")]
use linuxvideo::{CapabilityFlags, Device};
use regex::Regex;

#[cfg(windows)]
pub use crate::capture::mf::{describe_dev, list};

/// One capture node, as `card_list` describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub path: PathBuf,
    /// The USB serial (sysfs), empty when the device has none.
    pub serial: String,
    /// udev's ID_PATH: the USB port, stable across replugs into it.
    pub port: String,
    /// The V4L2 card name ("Card type").
    pub card_type: String,
}

/// What the settings say about the card.
pub struct Pins<'a> {
    pub dev: &'a str,
    pub serial: &'a str,
    pub port: &'a str,
    pub ignore: &'a str,
    /// Where the pin in effect (CARD_SERIAL, else CARD_PORT) came from, as
    /// the missing card's message says it: "in <config file>", or "from
    /// the environment".
    pub source: &'a str,
}

pub struct Picked {
    pub node: Node,
    /// A DEV set aside, said once.
    pub warning: Option<String>,
}

/// Why no card was picked.
#[derive(Debug)]
pub enum PickError {
    /// The card the pins name is not plugged in (#39): the setup can
    /// choose a card again.
    Missing(Missing),
    Other(String),
}

/// The pinned card not found (#39): what was found instead, what to
/// check, and how to choose a card again.
#[derive(Debug, Clone, PartialEq)]
pub struct Missing {
    pub headline: String,
    /// "found instead: …", and the other hints.
    pub details: Vec<String>,
    /// "to choose a card again: …".
    pub setup: String,
}

impl std::fmt::Display for Missing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.headline)?;
        for d in self.details.iter().chain([&self.setup]) {
            write!(f, "\n  {d}")?;
        }
        Ok(())
    }
}

impl Missing {
    /// A desktop notification's body (#28): the first line and how to
    /// choose again; the details are on stderr and in the log.
    pub fn notice(&self) -> String {
        format!("{}\n{}", self.headline, self.setup)
    }
}

impl std::fmt::Display for PickError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PickError::Missing(m) => m.fmt(f),
            PickError::Other(e) => f.write_str(e),
        }
    }
}

#[cfg(target_os = "linux")]
/// The node's USB device directory in sysfs, if it is on USB.
fn usb_dir(node: &Path) -> Option<PathBuf> {
    let name = node.file_name()?;
    let intf = fs::canonicalize(Path::new("/sys/class/video4linux").join(name).join("device")).ok()?;
    let dev = intf.parent()?.to_path_buf();
    dev.join("idVendor").exists().then_some(dev)
}

#[cfg(target_os = "linux")]
fn serial(node: &Path) -> String {
    usb_dir(node)
        .and_then(|d| fs::read_to_string(d.join("serial")).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

#[cfg(target_os = "linux")]
/// udev's ID_PATH, from its database: /run/udev/data/c<major>:<minor>.
fn port(node: &Path) -> String {
    let Some(name) = node.file_name() else { return String::new() };
    let devnum = fs::read_to_string(Path::new("/sys/class/video4linux").join(name).join("dev")).unwrap_or_default();
    fs::read_to_string(format!("/run/udev/data/c{}", devnum.trim()))
        .unwrap_or_default()
        .lines()
        .find_map(|l| l.strip_prefix("E:ID_PATH="))
        .unwrap_or_default()
        .to_string()
}

#[cfg(target_os = "linux")]
/// The card type, and whether the node itself captures video (a UVC device
/// also has a metadata node).
fn probe(node: &Path) -> Option<(String, bool)> {
    let caps = Device::open(node).ok()?.capabilities().ok()?;
    Some((caps.card().to_string(), caps.device_capabilities().contains(CapabilityFlags::VIDEO_CAPTURE)))
}

#[cfg(target_os = "linux")]
/// `card_list`: every video capture node, in /dev order.
pub fn list() -> Vec<Node> {
    let mut paths: Vec<PathBuf> = fs::read_dir("/dev")
        .map(|d| d.filter_map(|e| e.ok().map(|e| e.path())).collect())
        .unwrap_or_default();
    paths.retain(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("video")));
    // view.sh walks /dev/video* in glob order (video0, video1, video10...).
    paths.sort();
    paths
        .into_iter()
        .filter_map(|p| {
            let (card_type, capture) = probe(&p)?;
            capture.then(|| Node { serial: serial(&p), port: port(&p), card_type, path: p })
        })
        .collect()
}

/// `card_ignored`: an empty pattern ignores nothing.
fn ignored(ignore: &Option<Regex>, card_type: &str) -> bool {
    ignore.as_ref().is_some_and(|re| re.is_match(card_type))
}

/// `resolve_card` over the given nodes; `dev_node` describes DEV when set
/// (None: it does not exist).
pub fn pick(nodes: &[Node], pins: &Pins, dev_node: Option<Option<(String, bool)>>, config: &str) -> Result<Picked, PickError> {
    let ignore = if pins.ignore.is_empty() {
        None
    } else {
        Some(Regex::new(pins.ignore).map_err(|e| PickError::Other(format!("IGNORE_CARDS_RE={} is not a valid pattern: {e}", pins.ignore)))?)
    };
    let mut warning = None;
    if !pins.dev.is_empty() {
        let reason = match &dev_node {
            Some(None) | None => Some(format!("DEV={} does not exist (any more)", pins.dev)),
            Some(Some((t, capture))) if !capture => {
                Some(format!("DEV={} is {} now, not a video capture node", pins.dev, if t.is_empty() { "a device" } else { t }))
            }
            Some(Some((t, _))) if ignored(&ignore, t) => {
                Some(format!("DEV={} is {} now, an ignored device", pins.dev, if t.is_empty() { "a device" } else { t }))
            }
            Some(Some((t, _))) => {
                let node = Node { path: pins.dev.into(), serial: String::new(), port: String::new(), card_type: t.clone() };
                return Ok(Picked { node, warning: None });
            }
        };
        let reason = reason.expect("every other case returned");
        // With no pin, a scan would open the first capture node left: a
        // webcam not on the ignore list, and its mic on the speakers.
        if pins.serial.is_empty() && pins.port.is_empty() {
            return Err(PickError::Other(format!(
                "{reason}; the card has no other pin, so it cannot be found again safely: plug it back in, or re-run the setup"
            )));
        }
        warning = Some(format!("{reason}; looking for the capture card instead"));
    }

    let mut at_pin = None;
    for n in nodes {
        // Ignored devices first: never opened, not even at the pin.
        if ignored(&ignore, &n.card_type) {
            if (!pins.serial.is_empty() && n.serial == pins.serial)
                || (pins.serial.is_empty() && !pins.port.is_empty() && n.port == pins.port)
            {
                at_pin = Some(format!("{} ({})", n.path.display(), n.card_type));
            }
            continue;
        }
        if !pins.serial.is_empty() {
            if n.serial != pins.serial {
                continue;
            }
        } else if !pins.port.is_empty() && n.port != pins.port {
            continue;
        }
        return Ok(Picked { node: n.clone(), warning });
    }
    Err(if let Some(a) = at_pin {
        PickError::Other(format!(
            "the device at the card pin, {a}, is an ignored device (IGNORE_CARDS_RE in {config}): if it is the capture card, take it off that list; if not, plug the card back in where it was"
        ))
    } else if !pins.serial.is_empty() || !pins.port.is_empty() {
        PickError::Missing(missing(nodes, pins, &ignore))
    } else {
        PickError::Other("no capture device found; set DEV=/dev/videoN (or run the setup)".to_string())
    })
}

/// The pinned card not among `nodes` (#39), said with what is there.
fn missing(nodes: &[Node], pins: &Pins, ignore: &Option<Regex>) -> Missing {
    let headline = if !pins.serial.is_empty() {
        format!("the capture card with serial {} is not plugged in (CARD_SERIAL {})", pins.serial, pins.source)
    } else {
        format!("the capture card on USB port {} is not plugged in (CARD_PORT {})", pins.port, pins.source)
    };
    let found: Vec<String> = nodes
        .iter()
        .map(|n| match (ignored(ignore, &n.card_type), n.serial.as_str(), n.port.as_str()) {
            (true, _, _) => format!("{} (ignored)", n.card_type),
            (false, "", "") => n.card_type.clone(),
            (false, "", port) => format!("{} (port {port})", n.card_type),
            (false, serial, _) => format!("{} (serial {serial})", n.card_type),
        })
        .collect();
    let mut details = vec![format!("found instead: {}", if found.is_empty() { "none".to_string() } else { found.join(", ") })];
    // A card pinned by its port has no serial to know it by elsewhere: a
    // capture device on another port may be it, moved.
    if pins.serial.is_empty()
        && let Some(n) = nodes.iter().find(|n| !ignored(ignore, &n.card_type) && !n.port.is_empty())
    {
        details.push(format!("moved? {} is on USB port {}: if it is the card, the setup pins it there", n.card_type, n.port));
    }
    details.push("check: the card's USB cable, and that it is on a USB 3 port".into());
    // view.sh's own wizard beside it where view.sh runs (wizard.rs
    // `has_view_sh`: Linux).
    let setup = if cfg!(target_os = "linux") {
        "to choose a card again: protohdmicapture --setup (or ./view.sh --setup)"
    } else {
        "to choose a card again: protohdmicapture --setup"
    };
    Missing { headline, details, setup: setup.into() }
}

#[cfg(target_os = "linux")]
/// DEV as `pick` wants it: None when unset, Some(None) when it does not exist.
pub fn describe_dev(dev: &str) -> Option<Option<(String, bool)>> {
    (!dev.is_empty()).then(|| if Path::new(dev).exists() { probe(Path::new(dev)) } else { None })
}

/// An error from opening the card, with the hint lib/setup.sh gives when
/// the card is busy: who holds it, where that can be read (`holders`).
fn open_error(e: String, holders: &[String]) -> String {
    if !e.contains("busy") {
        return e;
    }
    match holders {
        [] => format!("the card is busy: a feed is probably running. Close it first. ({e})"),
        h => format!("the card is busy: {} has it open. Close it first. ({e})", h.join(", ")),
    }
}

/// The processes that have `node` open, as "name (pid N)", from /proc.
/// Another user's processes cannot be read and are not listed.
#[cfg(target_os = "linux")]
fn holders(node: &Path) -> Vec<String> {
    let Ok(procs) = fs::read_dir("/proc") else { return Vec::new() };
    let mut out = Vec::new();
    for p in procs.flatten() {
        let Some(pid) = p.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
        let Ok(fds) = fs::read_dir(p.path().join("fd")) else { continue };
        if fds.flatten().any(|fd| fs::read_link(fd.path()).is_ok_and(|t| t == node)) {
            let name = fs::read_to_string(p.path().join("comm")).map_or_else(|_| "?".to_string(), |c| c.trim().to_string());
            out.push(format!("{name} (pid {pid})"));
        }
    }
    out
}

/// The capture node opened at `format`: V4L2 on Linux, Media Foundation
/// on Windows (#18), a busy card's error saying who holds it where that
/// can be read (Linux).
pub fn open(node: &Node, format: Format) -> Result<Box<dyn FrameSource>, String> {
    #[cfg(target_os = "linux")]
    let source = crate::capture::V4l2::open(&node.path, format).map_err(|e| open_error(e, &holders(&node.path)))?;
    #[cfg(windows)]
    let source = crate::capture::MediaFoundation::open(&node.path, format).map_err(|e| open_error(e, &[]))?;
    Ok(Box::new(source))
}

/// How a node is named on the wizard's pages: its path, or on Windows
/// "USB 1e4e:7016" for Media Foundation's device link, which is a line
/// long (the link itself where it names no USB device).
pub fn label(node: &Node) -> String {
    #[cfg(windows)]
    {
        let link = node.path.to_string_lossy();
        crate::capture::mf::link_usb_id(&link).map_or_else(|| link.into_owned(), |id| format!("USB {id}"))
    }
    #[cfg(not(windows))]
    {
        node.path.display().to_string()
    }
}

/// The card's USB device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Usb {
    /// vid:pid.
    pub id: String,
    /// The port: on Linux the device directory's name ("3-2").
    pub port: String,
    /// bcdUSB as sysfs gives it (" 3.20").
    pub version: String,
    /// The link speed in Mbit/s ("5000"); empty when unknown.
    pub speed: String,
    /// Why the speed is unknown, where the platform says (Windows: the step
    /// of asking the hub that failed); empty otherwise.
    pub unknown: String,
}

/// The node's USB device, None when it is not on USB: sysfs on Linux, the
/// device tree and the hub on Windows.
pub fn usb(node: &Node) -> Option<Usb> {
    #[cfg(target_os = "linux")]
    {
        let dir = usb_dir(&node.path)?;
        let read = |f: &str| fs::read_to_string(dir.join(f)).map(|s| s.trim_end_matches('\n').to_string()).unwrap_or_default();
        Some(Usb {
            id: format!("{}:{}", read("idVendor"), read("idProduct")),
            port: dir.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
            version: read("version"),
            speed: read("speed"),
            unknown: String::new(),
        })
    }
    #[cfg(windows)]
    {
        let d = crate::capture::mf::usb(&node.path)?;
        let ((version, speed), unknown) = match d.link {
            Ok(link) => (link, String::new()),
            Err(why) => (Default::default(), why),
        };
        Some(Usb { id: d.id, port: d.port, version, speed, unknown })
    }
}

/// A mode the card lists: FMT, RES and FPS as the settings spell them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mode {
    pub fmt: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

/// The modes the node lists, in its own order: the formats this viewer
/// captures, each size and rate; none (said) when they cannot be read.
pub fn modes(node: &Node) -> Vec<Mode> {
    #[cfg(target_os = "linux")]
    let modes = crate::capture::V4l2::modes(&node.path);
    #[cfg(windows)]
    let modes = crate::capture::mf::modes(&node.path);
    match modes {
        Ok(modes) => modes
            .into_iter()
            .map(|(p, width, height, fps)| Mode { fmt: p.setting().into(), width, height, fps: rounded_fps(fps) })
            .collect(),
        Err(e) => {
            eprintln!("warning: the card's modes: {e}");
            Vec::new()
        }
    }
}

/// A rate as FPS spells it: rounded as lib/setup.sh rounds v4l2-ctl's
/// "(%.3f fps)".
fn rounded_fps(fps: f64) -> u32 {
    (fps + 0.5).floor() as u32
}

/// An audio input: what SRC may say to name it (`id`) and how it is shown
/// (`name`). On Linux both are its PipeWire node name; on Windows the
/// endpoint id and the friendly name, which can say nothing of the card
/// ("USB Audio Device", #18).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioInput {
    pub id: String,
    pub name: String,
}

/// The audio inputs, and which one is the card's.
pub struct AudioInputs {
    /// The card's own input, or why it is not known.
    pub card: Result<AudioInput, String>,
    /// Every input's name (no monitors), as SRC may say it.
    pub all: Vec<String>,
}

/// The audio inputs, with the card's own: on Linux the PipeWire source on
/// the card's USB device, on Windows the one input in the card's container
/// (one physical device). `card`: None with `--test-pattern`. Err: the
/// inputs cannot be listed at all.
pub fn audio_inputs(card: Option<&Node>) -> Result<AudioInputs, String> {
    #[cfg(target_os = "linux")]
    {
        let snap = crate::pw::snapshot()?;
        let found = card.ok_or("no card to find the audio input of (--test-pattern)").and_then(|card| {
            let usb = usb_dir(&card.path).ok_or("the card is not on USB, so its audio input cannot be found")?;
            let usb = usb.to_string_lossy();
            snap.card_source(usb.strip_prefix("/sys").unwrap_or(&usb)).ok_or("the card has no PipeWire source")
        });
        let card = found.map(|n| AudioInput { id: n.clone(), name: n }).map_err(str::to_string);
        Ok(AudioInputs { card, all: snap.audio_sources() })
    }
    #[cfg(windows)]
    {
        let inputs: Vec<(String, String, Option<u128>)> =
            crate::audio::inputs()?.into_iter().map(|i| (i.name, i.id, i.container)).collect();
        let found = match card {
            None => Err("no card to find the audio input of (--test-pattern)".to_string()),
            Some(card) => crate::capture::mf::container(&card.path)
                .map_err(|e| format!("the card's audio input cannot be found: {e}"))
                .and_then(|g| card_input(g.to_u128(), &inputs).map_err(|e| format!("{e} ({})", card.card_type))),
        };
        Ok(AudioInputs { card: found, all: inputs.into_iter().map(|i| i.0).collect() })
    }
}

/// How to name the audio input by hand, after why the card's own is not
/// known.
pub fn set_src(all: &[String]) -> String {
    if cfg!(windows) {
        format!("set SRC to one of: {}", all.join(" | "))
    } else {
        "set SRC=<name from pactl list short sources>".to_string()
    }
}

/// The one input (name, endpoint id, container id) in the card's
/// container.
#[cfg(any(windows, test))]
fn card_input(card: u128, inputs: &[(String, String, Option<u128>)]) -> Result<AudioInput, &'static str> {
    let mut found = inputs.iter().filter(|i| i.2 == Some(card));
    match (found.next(), found.next()) {
        (Some(one), None) => Ok(AudioInput { id: one.1.clone(), name: one.0.clone() }),
        (None, _) => Err("no audio input belongs to the card"),
        (Some(_), Some(_)) => Err("more than one audio input belongs to the card"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn a_busy_card_says_who_holds_it() {
        let e = "/dev/video0: Device or resource busy (os error 16)";
        assert_eq!(
            open_error(e.into(), &["mpv (pid 4242)".into()]),
            "the card is busy: mpv (pid 4242) has it open. Close it first. (/dev/video0: Device or resource busy (os error 16))"
        );
        assert_eq!(
            open_error(e.into(), &[]),
            "the card is busy: a feed is probably running. Close it first. (/dev/video0: Device or resource busy (os error 16))"
        );
        // Any other error is left as it is.
        assert_eq!(open_error("/dev/video0: No such device".into(), &["mpv (pid 1)".into()]), "/dev/video0: No such device");
    }

    fn input(name: &str, id: &str, container: Option<u128>) -> (String, String, Option<u128>) {
        (name.into(), id.into(), container)
    }

    #[test]
    fn the_cards_input_is_the_one_in_its_container() {
        // Names that say nothing of the card, as on Windows (#18).
        let inputs = [
            input("Remote Audio", "{0.0.1.00000000}.{a}", None),
            input("Microphone (USB Audio Device)", "{0.0.1.00000000}.{b}", Some(7)),
            input("Microphone (Yeti Nano)", "{0.0.1.00000000}.{c}", Some(9)),
        ];
        let card = AudioInput { id: "{0.0.1.00000000}.{b}".into(), name: "Microphone (USB Audio Device)".into() };
        assert_eq!(card_input(7, &inputs), Ok(card));
        assert_eq!(card_input(8, &inputs), Err("no audio input belongs to the card"));
        let two = [input("a", "1", Some(7)), input("b", "2", Some(7))];
        assert_eq!(card_input(7, &two), Err("more than one audio input belongs to the card"));
    }

    #[test]
    fn a_rate_is_rounded_as_lib_setup_sh_rounds_it() {
        // 59.94 (60000/1001) and 29.97 as FPS says them; .5 rounds up.
        assert_eq!(rounded_fps(60000.0 / 1001.0), 60);
        assert_eq!(rounded_fps(30000.0 / 1001.0), 30);
        assert_eq!(rounded_fps(25.0), 25);
        assert_eq!(rounded_fps(24.5), 25);
        assert_eq!(rounded_fps(24.49), 24);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn holders_of_a_file_this_process_has_open() {
        let dir = std::env::temp_dir().join(format!("protohdmi-holders-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("node");
        let _open = fs::File::create(&path).unwrap();
        let me = format!("(pid {})", std::process::id());
        assert!(holders(&path).iter().any(|h| h.ends_with(&me)), "{:?}", holders(&path));
        assert!(holders(&dir.join("other")).is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }

    fn node(path: &str, serial: &str, port: &str, t: &str) -> Node {
        Node { path: path.into(), serial: serial.into(), port: port.into(), card_type: t.into() }
    }

    fn nodes() -> Vec<Node> {
        vec![
            node("/dev/video0", "CAM1", "pci-a", "OBSBOT Meet 2"),
            node("/dev/video2", "20000130041415", "pci-b", "UVC Camera (1e4e:7016)"),
            node("/dev/video4", "", "pci-c", "USB Video"),
        ]
    }

    fn pins<'a>(dev: &'a str, serial: &'a str, port: &'a str, ignore: &'a str) -> Pins<'a> {
        Pins { dev, serial, port, ignore, source: "in cfg" }
    }

    /// The error's text, whichever kind.
    fn err(r: Result<Picked, PickError>) -> String {
        r.err().expect("an error").to_string()
    }

    fn missing_of(r: Result<Picked, PickError>) -> Missing {
        match r {
            Err(PickError::Missing(m)) => m,
            Err(PickError::Other(e)) => panic!("not the missing card: {e}"),
            Ok(p) => panic!("picked {}", p.node.path.display()),
        }
    }

    #[test]
    fn the_first_capture_node_not_ignored() {
        let p = pick(&nodes(), &pins("", "", "", "OBSBOT|Meet 2"), None, "cfg").unwrap();
        assert_eq!(p.node.path, Path::new("/dev/video2"));
        let p = pick(&nodes(), &pins("", "", "", ""), None, "cfg").unwrap();
        assert_eq!(p.node.path, Path::new("/dev/video0"));
    }

    #[test]
    fn serial_wins_over_port() {
        let p = pick(&nodes(), &pins("", "20000130041415", "pci-c", ""), None, "cfg").unwrap();
        assert_eq!(p.node.path, Path::new("/dev/video2"));
        let p = pick(&nodes(), &pins("", "", "pci-c", ""), None, "cfg").unwrap();
        assert_eq!(p.node.path, Path::new("/dev/video4"));
    }

    #[test]
    fn an_ignored_device_at_the_pin_is_never_opened() {
        let r = pick(&nodes(), &pins("", "CAM1", "", "OBSBOT"), None, "cfg");
        // Not the missing card: the setup's offer is not for this one.
        assert!(matches!(r, Err(PickError::Other(_))));
        let e = err(r);
        assert!(e.contains("/dev/video0 (OBSBOT Meet 2), is an ignored device"), "{e}");
    }

    #[test]
    fn a_missing_dev_without_a_pin_stops() {
        let e = err(pick(&nodes(), &pins("/dev/video9", "", "", ""), Some(None), "cfg"));
        assert!(e.starts_with("DEV=/dev/video9 does not exist (any more); the card has no other pin"), "{e}");
        let p = pick(&nodes(), &pins("/dev/video9", "20000130041415", "", ""), Some(None), "cfg").unwrap();
        assert_eq!(p.node.path, Path::new("/dev/video2"));
        assert!(p.warning.unwrap().ends_with("looking for the capture card instead"));
    }

    #[test]
    fn a_dev_that_is_a_metadata_node_is_set_aside() {
        let e = err(pick(&nodes(), &pins("/dev/video3", "", "", ""), Some(Some(("UVC Camera".into(), false))), "cfg"));
        assert!(e.contains("is UVC Camera now, not a video capture node"), "{e}");
    }

    /// The setup line as this platform writes it.
    fn setup_line() -> &'static str {
        if cfg!(target_os = "linux") {
            "to choose a card again: protohdmicapture --setup (or ./view.sh --setup)"
        } else {
            "to choose a card again: protohdmicapture --setup"
        }
    }

    #[test]
    fn the_missing_card_by_serial_says_what_is_there_and_how_to_choose_again() {
        // The laptop (#39): the card unplugged, its serial from the file.
        let laptop = [node(r"\\?\usb#vid_5986", "", r"PCIROOT(0)#USB(5)", "Integrated Camera")];
        let p = Pins { source: r"in C:\Users\m\AppData\Roaming\protohdmicapture\config", ..pins("", "20000130041415", "", "Integrated Camera") };
        let m = missing_of(pick(&laptop, &p, None, "cfg"));
        assert_eq!(
            m.to_string(),
            format!(
                "the capture card with serial 20000130041415 is not plugged in (CARD_SERIAL in C:\\Users\\m\\AppData\\Roaming\\protohdmicapture\\config)\n  found instead: Integrated Camera (ignored)\n  check: the card's USB cable, and that it is on a USB 3 port\n  {}",
                setup_line()
            )
        );
        // The notification: the first line and how to choose again.
        assert_eq!(m.notice(), format!("{}\n{}", m.headline, setup_line()));
        // From the environment, as `--settings` says CARD_SERIAL=…(env).
        let m = missing_of(pick(&laptop, &Pins { source: "from the environment", ..pins("", "20000130041415", "", "") }, None, "cfg"));
        assert_eq!(m.headline, "the capture card with serial 20000130041415 is not plugged in (CARD_SERIAL from the environment)");
        // Not ignored: each device with its serial, or its port without one.
        assert_eq!(m.details[0], "found instead: Integrated Camera (port PCIROOT(0)#USB(5))");
        let m = missing_of(pick(&nodes(), &pins("", "NOPE", "", "OBSBOT"), None, "cfg"));
        assert_eq!(m.details[0], "found instead: OBSBOT Meet 2 (ignored), UVC Camera (1e4e:7016) (serial 20000130041415), USB Video (port pci-c)");
        // A serial pin: no "moved?" guess, the serial would have found it.
        assert!(!m.details.iter().any(|d| d.starts_with("moved?")), "{:?}", m.details);
        // Nothing plugged in at all.
        let m = missing_of(pick(&[], &pins("", "NOPE", "", ""), None, "cfg"));
        assert_eq!(m.details[0], "found instead: none");
    }

    #[test]
    fn the_missing_card_by_port_names_a_capture_device_on_another_port() {
        // No serial (the card is pinned by its port): one not ignored
        // elsewhere may be it, moved.
        let m = missing_of(pick(&nodes(), &pins("", "", "pci-z", "OBSBOT"), None, "cfg"));
        assert_eq!(m.headline, "the capture card on USB port pci-z is not plugged in (CARD_PORT in cfg)");
        assert_eq!(m.details[1], "moved? UVC Camera (1e4e:7016) is on USB port pci-b: if it is the card, the setup pins it there");
        // Only ignored devices: no guess.
        let m = missing_of(pick(&nodes()[..1], &pins("", "", "pci-z", "OBSBOT"), None, "cfg"));
        assert_eq!(m.details, ["found instead: OBSBOT Meet 2 (ignored)", "check: the card's USB cable, and that it is on a USB 3 port"]);
    }

    #[test]
    fn no_pin_and_no_device_is_not_the_missing_card() {
        // Nothing saved to miss: the setup's offer is not for this one.
        assert!(matches!(pick(&[], &pins("", "", "", ""), None, "cfg"), Err(PickError::Other(_))));
    }
}
