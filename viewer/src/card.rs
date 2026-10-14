//! Which capture card to open: the same rules as view.sh's `resolve_card`
//! (lib/card.sh). The first video capture node that is not an ignored
//! device, narrowed by the pins (`CARD_SERIAL`, else `CARD_PORT`); a `DEV`
//! that is gone, not a capture node, or ignored is set aside for the pins.

use std::fs;
use std::path::{Path, PathBuf};

use linuxvideo::{CapabilityFlags, Device};
use regex::Regex;

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
}

pub struct Picked {
    pub node: Node,
    /// A DEV set aside, said once.
    pub warning: Option<String>,
}

/// The node's USB device directory in sysfs, if it is on USB.
pub fn usb_dir(node: &Path) -> Option<PathBuf> {
    let name = node.file_name()?;
    let intf = fs::canonicalize(Path::new("/sys/class/video4linux").join(name).join("device")).ok()?;
    let dev = intf.parent()?.to_path_buf();
    dev.join("idVendor").exists().then_some(dev)
}

fn serial(node: &Path) -> String {
    usb_dir(node)
        .and_then(|d| fs::read_to_string(d.join("serial")).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

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

/// The card type, and whether the node itself captures video (a UVC device
/// also has a metadata node).
fn probe(node: &Path) -> Option<(String, bool)> {
    let caps = Device::open(node).ok()?.capabilities().ok()?;
    Some((caps.card().to_string(), caps.device_capabilities().contains(CapabilityFlags::VIDEO_CAPTURE)))
}

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
pub fn pick(nodes: &[Node], pins: &Pins, dev_node: Option<Option<(String, bool)>>, config: &str) -> Result<Picked, String> {
    let ignore = if pins.ignore.is_empty() {
        None
    } else {
        Some(Regex::new(pins.ignore).map_err(|e| format!("IGNORE_CARDS_RE={} is not a valid pattern: {e}", pins.ignore))?)
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
            return Err(format!(
                "{reason}; the card has no other pin, so it cannot be found again safely: plug it back in, or re-run the setup"
            ));
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
        format!(
            "the device at the card pin, {a}, is an ignored device (IGNORE_CARDS_RE in {config}): if it is the capture card, take it off that list; if not, plug the card back in where it was"
        )
    } else if !pins.serial.is_empty() {
        format!("no capture card with serial {} found; is it plugged in? (CARD_SERIAL in {config})", pins.serial)
    } else if !pins.port.is_empty() {
        format!("no capture card on USB port {}; is it plugged in, and into the same port? Re-run the setup if it moved", pins.port)
    } else {
        "no capture device found; set DEV=/dev/videoN (or run the setup)".to_string()
    })
}

/// DEV as `pick` wants it: None when unset, Some(None) when it does not exist.
pub fn describe_dev(dev: &str) -> Option<Option<(String, bool)>> {
    (!dev.is_empty()).then(|| if Path::new(dev).exists() { probe(Path::new(dev)) } else { None })
}

#[cfg(test)]
mod tests {
    use super::*;

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
        Pins { dev, serial, port, ignore }
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
        let e = pick(&nodes(), &pins("", "CAM1", "", "OBSBOT"), None, "cfg").err().unwrap();
        assert!(e.contains("/dev/video0 (OBSBOT Meet 2), is an ignored device"), "{e}");
    }

    #[test]
    fn a_missing_dev_without_a_pin_stops() {
        let e = pick(&nodes(), &pins("/dev/video9", "", "", ""), Some(None), "cfg").err().unwrap();
        assert!(e.starts_with("DEV=/dev/video9 does not exist (any more); the card has no other pin"), "{e}");
        let p = pick(&nodes(), &pins("/dev/video9", "20000130041415", "", ""), Some(None), "cfg").unwrap();
        assert_eq!(p.node.path, Path::new("/dev/video2"));
        assert!(p.warning.unwrap().ends_with("looking for the capture card instead"));
    }

    #[test]
    fn a_dev_that_is_a_metadata_node_is_set_aside() {
        let e = pick(&nodes(), &pins("/dev/video3", "", "", ""), Some(Some(("UVC Camera".into(), false))), "cfg")
            .err()
            .unwrap();
        assert!(e.contains("is UVC Camera now, not a video capture node"), "{e}");
    }

    #[test]
    fn no_card_with_the_serial() {
        let e = pick(&nodes(), &pins("", "NOPE", "", ""), None, "cfg").err().unwrap();
        assert!(e.starts_with("no capture card with serial NOPE found"), "{e}");
    }
}
