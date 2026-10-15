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

/// How long one round trip to PipeWire may take: a session manager that
/// hangs must not freeze the audio keys or the start for good.
const SYNC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

fn sync(core: &pw::core::CoreRc, mainloop: &pw::main_loop::MainLoopRc) -> Result<(), String> {
    let timed_out = Rc::new(Cell::new(false));
    let (t, l) = (timed_out.clone(), mainloop.clone());
    let timer = mainloop.loop_().add_timer(move |_| {
        t.set(true);
        l.quit();
    });
    timer
        .update_timer(Some(SYNC_TIMEOUT), None)
        .into_sync_result()
        .map_err(|e| format!("PipeWire timer: {e}"))?;
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
        if timed_out.get() {
            return Err(format!("PipeWire did not answer within {} s", SYNC_TIMEOUT.as_secs()));
        }
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

    /// Every audio input's node name, sorted: what `pactl list short
    /// sources` lists less the sinks' monitors, which are no Audio/Source
    /// nodes in PipeWire (the wizard's audio stage).
    pub fn audio_sources(&self) -> Vec<String> {
        let mut names: Vec<String> =
            self.0.nodes.values().filter(|n| n.class.starts_with("Audio/Source")).map(|n| n.name.clone()).collect();
        names.sort();
        names.dedup();
        names
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

/// The volume and mute of this process's audio output stream, as mixers
/// and `pactl` show them: volume in percent on PulseAudio's cubic scale
/// (linear = (percent / 100)^3), as view.sh's lib/audio.sh reads them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Level {
    pub volume: u32,
    pub mute: bool,
}

/// What the overlay's m, 9 and 0 ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LevelChange {
    Mute,
    /// Percent points, the result clamped to 0..100 as lib/audio.sh does
    /// (past 100 % the loopback only clips).
    Volume(i32),
}

/// Percent on the cubic scale from a linear channel volume, and back.
pub fn percent(linear: f32) -> u32 {
    (f64::from(linear.max(0.0)).cbrt() * 100.0).round() as u32
}

pub fn linear(percent: u32) -> f32 {
    (f64::from(percent) / 100.0).powi(3) as f32
}


impl Level {
    pub fn changed(self, change: LevelChange) -> Self {
        match change {
            LevelChange::Mute => Self { mute: !self.mute, ..self },
            LevelChange::Volume(by) => Self { volume: (self.volume as i32 + by).clamp(0, 100) as u32, ..self },
        }
    }
}

/// Reads the output stream's level and, with a change, sets it on the
/// stream (Props: channelVolumes and mute), where WirePlumber remembers it
/// for the next session. Blocks for a few PipeWire round trips.
pub fn level(change: Option<LevelChange>) -> Result<Level, String> {
    use pw::spa::param::ParamType;
    use pw::spa::pod::deserialize::PodDeserializer;
    use pw::spa::pod::serialize::PodSerializer;
    use pw::spa::pod::{Object, Pod, Property, PropertyFlags, Value, ValueArray};
    use pw::spa::sys::{SPA_PARAM_Props, SPA_PROP_channelVolumes, SPA_PROP_mute, SPA_TYPE_OBJECT_Props};

    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(|e| format!("PipeWire main loop: {e}"))?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(|e| format!("PipeWire context: {e}"))?;
    let core = context.connect_rc(None).map_err(|e| format!("PipeWire connect: {e}"))?;
    let registry = core.get_registry_rc().map_err(|e| format!("PipeWire registry: {e}"))?;

    // Every output stream is bound (its client may be announced after it);
    // ours is picked once all are known.
    let ours: Rc<RefCell<HashSet<u32>>> = Rc::default();
    // (client, node) of every output stream.
    type Streams = Vec<(Option<u32>, pw::node::Node)>;
    let streams: Rc<RefCell<Streams>> = Rc::default();
    let pid = std::process::id().to_string();
    let (o, st, reg) = (ours.clone(), streams.clone(), registry.clone());
    let _registry_listener = registry
        .add_listener_local()
        .global(move |g| {
            let Some(props) = g.props else { return };
            match g.type_ {
                ObjectType::Client => {
                    let p = props.get("pipewire.sec.pid").or_else(|| props.get("application.process.id"));
                    if p == Some(pid.as_str()) {
                        o.borrow_mut().insert(g.id);
                    }
                }
                ObjectType::Node if props.get("media.class") == Some("Stream/Output/Audio") => {
                    let client = props.get("client.id").and_then(|v| v.parse().ok());
                    if let Ok(node) = reg.bind::<pw::node::Node, _>(g) {
                        st.borrow_mut().push((client, node));
                    }
                }
                _ => {}
            }
        })
        .register();
    sync(&core, &mainloop)?;

    let node = {
        let ours = ours.borrow();
        let mut streams = streams.borrow_mut();
        let mut mine = streams.drain(..).filter(|(c, _)| c.is_some_and(|c| ours.contains(&c)));
        let (_, node) = mine.next().ok_or("this viewer has no audio output stream")?;
        if mine.next().is_some() {
            return Err("this viewer has more than one audio output stream".into());
        }
        node
    };

    // The first channel volume and the mute, as the stream reports them.
    type Read = Option<(Option<f32>, Option<bool>)>;
    let read: Rc<RefCell<Read>> = Rc::default();
    let r = read.clone();
    let _node_listener = node
        .add_listener_local()
        .param(move |_, _, _, _, pod| {
            let Some(pod) = pod else { return };
            let Ok((_, Value::Object(obj))) = PodDeserializer::deserialize_any_from(pod.as_bytes()) else { return };
            let mut volume = None;
            let mut mute = None;
            for p in obj.properties {
                match p.value {
                    Value::ValueArray(ValueArray::Float(v)) if p.key == SPA_PROP_channelVolumes => volume = v.first().copied(),
                    Value::Bool(m) if p.key == SPA_PROP_mute => mute = Some(m),
                    _ => {}
                }
            }
            // A stream can list more than one Props object; the first
            // with the channel volumes is the stream's own.
            if volume.is_some() && r.borrow().is_none() {
                *r.borrow_mut() = Some((volume, mute));
            }
        })
        .register();
    // The stream's Props as it reports them now.
    let read_level = || -> Result<Level, String> {
        *read.borrow_mut() = None;
        node.enum_params(0, Some(ParamType::Props), 0, u32::MAX);
        sync(&core, &mainloop)?;
        let (Some(volume), mute) = read.borrow_mut().take().ok_or("the audio output stream reported no volume")? else {
            return Err("the audio output stream reported no volume".into());
        };
        Ok(Level { volume: percent(volume), mute: mute.unwrap_or(false) })
    };
    let now = read_level()?;
    let Some(change) = change else { return Ok(now) };

    let want = now.changed(change);
    let value = Value::Object(Object {
        type_: SPA_TYPE_OBJECT_Props,
        id: SPA_PARAM_Props,
        properties: vec![
            Property { key: SPA_PROP_mute, flags: PropertyFlags::empty(), value: Value::Bool(want.mute) },
            Property {
                key: SPA_PROP_channelVolumes,
                flags: PropertyFlags::empty(),
                // Every channel the same, as pactl sets a plain percentage.
                value: Value::ValueArray(ValueArray::Float(vec![linear(want.volume); crate::audio::CHANNELS as usize])),
            },
        ],
    });
    let (bytes, _) = PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &value)
        .map_err(|e| format!("volume parameter: {e:?}"))?;
    let bytes = bytes.into_inner();
    let pod = Pod::from_bytes(&bytes).ok_or("volume parameter: not a pod")?;
    node.set_param(ParamType::Props, 0, pod);
    sync(&core, &mainloop)?;
    // What the stream took, not what was asked (as lib/audio.sh reports
    // pactl's reading back).
    read_level()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_on_pulseaudios_cubic_scale() {
        assert_eq!(linear(100), 1.0);
        assert!((linear(50) - 0.125).abs() < 1e-7);
        for p in 0..=100 {
            assert_eq!(percent(linear(p)), p);
        }
        let l = Level { volume: 97, mute: false };
        assert_eq!(l.changed(LevelChange::Volume(5)).volume, 100);
        assert_eq!(Level { volume: 3, ..l }.changed(LevelChange::Volume(-5)).volume, 0);
        assert!(l.changed(LevelChange::Mute).mute);
        assert_eq!(l.changed(LevelChange::Mute).volume, 97);
    }
}
