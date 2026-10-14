//! What PipeWire actually linked into this process's capture stream.
//!
//! cpal names a target (`target.object`), but if that node is missing the
//! session manager links the default source instead, usually the
//! microphone, the same trap as `pw-loopback` (view.sh checks it with
//! pw-link). `node.dont-fallback` (set through `PIPEWIRE_PROPS`, see
//! main) stops that; this check is the proof: one snapshot of the
//! registry, then the links whose input is one of our capture nodes.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use pipewire as pw;
use pw::types::ObjectType;

#[derive(Default)]
struct Seen {
    /// Clients of this process (several: cpal's threads and this check).
    ours: HashSet<u32>,
    /// Node id -> (client id, node.name, media.class).
    nodes: HashMap<u32, (Option<u32>, String, String)>,
    /// (output node, input node).
    links: Vec<(u32, u32)>,
}

/// The node names linked into this process's capture streams, now.
pub fn sources_linked_to_us() -> Result<Vec<String>, String> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(|e| format!("PipeWire main loop: {e}"))?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(|e| format!("PipeWire context: {e}"))?;
    let core = context.connect_rc(None).map_err(|e| format!("PipeWire connect: {e}"))?;
    let registry = core.get_registry().map_err(|e| format!("PipeWire registry: {e}"))?;

    let seen = Rc::new(RefCell::new(Seen::default()));
    let pid = std::process::id().to_string();
    let s = seen.clone();
    let _registry_listener = registry
        .add_listener_local()
        .global(move |g| {
            let Some(props) = g.props else { return };
            let mut s = s.borrow_mut();
            match g.type_ {
                ObjectType::Client => {
                    let p = props.get("pipewire.sec.pid").or_else(|| props.get("application.process.id"));
                    if p == Some(pid.as_str()) {
                        s.ours.insert(g.id);
                    }
                }
                ObjectType::Node => {
                    let client = props.get("client.id").and_then(|c| c.parse().ok());
                    let name = props.get("node.name").unwrap_or("").to_string();
                    let class = props.get("media.class").unwrap_or("").to_string();
                    s.nodes.insert(g.id, (client, name, class));
                }
                ObjectType::Link => {
                    let out = props.get("link.output.node").and_then(|v| v.parse().ok());
                    let inp = props.get("link.input.node").and_then(|v| v.parse().ok());
                    if let (Some(o), Some(i)) = (out, inp) {
                        s.links.push((o, i));
                    }
                }
                _ => {}
            }
        })
        .register();

    // Every global is announced before the reply to this sync.
    let done = Rc::new(Cell::new(false));
    let pending = core.sync(0).map_err(|e| format!("PipeWire sync: {e}"))?;
    let (d, l) = (done.clone(), mainloop.clone());
    let _core_listener = core
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

    let s = seen.borrow();
    let capture: HashSet<u32> = s
        .nodes
        .iter()
        .filter(|(_, (client, _, class))| {
            client.is_some_and(|c| s.ours.contains(&c)) && class == "Stream/Input/Audio"
        })
        .map(|(id, _)| *id)
        .collect();
    let mut names: Vec<String> = s
        .links
        .iter()
        .filter(|(_, input)| capture.contains(input))
        .map(|(output, _)| s.nodes.get(output).map_or_else(|| format!("node {output}"), |n| n.1.clone()))
        .collect();
    names.sort();
    names.dedup();
    Ok(names)
}
