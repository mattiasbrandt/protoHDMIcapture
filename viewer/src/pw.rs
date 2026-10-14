//! One look at PipeWire's graph, for the two questions audio asks:
//!
//! - **Which source is the card's audio input?** The one whose ALSA device
//!   lies under the card's USB device in sysfs, as view.sh's
//!   `resolve_audio_source` finds it (works with or without a USB serial;
//!   "the first USB source" would grab a headset mic).
//! - **What did PipeWire actually link into our capture stream?** cpal names
//!   a target, but a missing one becomes the default source (the
//!   microphone) unless `node.dont-fallback` is set (main); this is the
//!   proof (#5, the Codex review).

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use pipewire as pw;
use pw::types::ObjectType;

struct NodeInfo {
    client: Option<u32>,
    device: Option<u32>,
    name: String,
    class: String,
}

#[derive(Default)]
struct Seen {
    /// Clients of this process (cpal's threads and this look).
    ours: HashSet<u32>,
    nodes: HashMap<u32, NodeInfo>,
    /// (output node, input node).
    links: Vec<(u32, u32)>,
    /// Audio device id -> device.sysfs.path (from the device's own info).
    device_sysfs: HashMap<u32, String>,
}

pub struct Snapshot(Seen);

fn sync(core: &pw::core::CoreRc, mainloop: &pw::main_loop::MainLoopRc) -> Result<(), String> {
    let done = Rc::new(Cell::new(false));
    let pending = core.sync(0).map_err(|e| format!("PipeWire sync: {e}"))?;
    let (d, l) = (done.clone(), mainloop.clone());
    let _listener = core
        .add_listener_local()
        .done(move |id, seq| {
            if id == pw::core::PW_ID_CORE && seq == pending {
                d.set(true);
                l.quit();
            }
        })
        .register();
    while !done.get() {
        mainloop.run();
    }
    Ok(())
}

/// The graph as it is now.
pub fn snapshot() -> Result<Snapshot, String> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(|e| format!("PipeWire main loop: {e}"))?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(|e| format!("PipeWire context: {e}"))?;
    let core = context.connect_rc(None).map_err(|e| format!("PipeWire connect: {e}"))?;
    let registry = core.get_registry_rc().map_err(|e| format!("PipeWire registry: {e}"))?;

    let seen = Rc::new(RefCell::new(Seen::default()));
    // Bound devices and their info listeners live until the second sync.
    let bound: Rc<RefCell<Vec<(pw::device::Device, pw::device::DeviceListener)>>> = Rc::default();
    let pid = std::process::id().to_string();
    let (s, b, reg) = (seen.clone(), bound.clone(), registry.clone());
    let _registry_listener = registry
        .add_listener_local()
        .global(move |g| {
            let Some(props) = g.props else { return };
            match g.type_ {
                ObjectType::Client => {
                    let p = props.get("pipewire.sec.pid").or_else(|| props.get("application.process.id"));
                    if p == Some(pid.as_str()) {
                        s.borrow_mut().ours.insert(g.id);
                    }
                }
                ObjectType::Node => {
                    let num = |k| props.get(k).and_then(|v: &str| v.parse().ok());
                    let info = NodeInfo {
                        client: num("client.id"),
                        device: num("device.id"),
                        name: props.get("node.name").unwrap_or("").to_string(),
                        class: props.get("media.class").unwrap_or("").to_string(),
                    };
                    s.borrow_mut().nodes.insert(g.id, info);
                }
                ObjectType::Link => {
                    let num = |k| props.get(k).and_then(|v: &str| v.parse().ok());
                    if let (Some(o), Some(i)) = (num("link.output.node"), num("link.input.node")) {
                        s.borrow_mut().links.push((o, i));
                    }
                }
                // The sysfs path is only in the device's own info.
                ObjectType::Device if props.get("media.class") == Some("Audio/Device") => {
                    let Ok(device) = reg.bind::<pw::device::Device, _>(g) else { return };
                    let (s2, id) = (s.clone(), g.id);
                    let listener = device
                        .add_listener_local()
                        .info(move |info| {
                            if let Some(path) = info.props().and_then(|p| p.get("device.sysfs.path")) {
                                s2.borrow_mut().device_sysfs.insert(id, path.to_string());
                            }
                        })
                        .register();
                    b.borrow_mut().push((device, listener));
                }
                _ => {}
            }
        })
        .register();

    // Every global comes before the first reply; the bound devices' info
    // before the second.
    sync(&core, &mainloop)?;
    sync(&core, &mainloop)?;
    drop(bound);
    drop(_registry_listener);
    let seen = Rc::try_unwrap(seen).map_err(|_| "PipeWire snapshot still borrowed".to_string())?.into_inner();
    Ok(Snapshot(seen))
}

impl Snapshot {
    /// The card's PipeWire source: the Audio/Source node of the audio
    /// device under `usb_sysfs` (a sysfs path without the leading "/sys").
    pub fn card_source(&self, usb_sysfs: &str) -> Option<String> {
        let s = &self.0;
        let under = format!("{usb_sysfs}/");
        let devices: HashSet<u32> =
            s.device_sysfs.iter().filter(|(_, p)| p.starts_with(&under)).map(|(id, _)| *id).collect();
        let mut names: Vec<&str> = s
            .nodes
            .values()
            .filter(|n| n.class == "Audio/Source" && n.device.is_some_and(|d| devices.contains(&d)))
            .map(|n| n.name.as_str())
            .collect();
        names.sort();
        names.first().map(|n| n.to_string())
    }

    /// The node names linked into this process's capture streams.
    pub fn sources_linked_to_us(&self) -> Vec<String> {
        let s = &self.0;
        let capture: HashSet<u32> = s
            .nodes
            .iter()
            .filter(|(_, n)| n.client.is_some_and(|c| s.ours.contains(&c)) && n.class == "Stream/Input/Audio")
            .map(|(id, _)| *id)
            .collect();
        let mut names: Vec<String> = s
            .links
            .iter()
            .filter(|(_, input)| capture.contains(input))
            .map(|(output, _)| s.nodes.get(output).map_or_else(|| format!("node {output}"), |n| n.name.clone()))
            .collect();
        names.sort();
        names.dedup();
        names
    }
}
