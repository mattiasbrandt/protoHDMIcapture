//! GameMode (#77): while a GPU enhancement is on (`gpustat::enhanced`),
//! protoHDMIcapture asks Feral's GameMode daemon for performance, so the
//! GPU is not clocked down to what a paced 60 Hz load needs (#75: the ML
//! upscaler 7.2 ms back to back, 12.4 ms paced with amdgpu's `auto` power
//! level).
//!
//! - **Only while an enhancement is on:** registered when one turns on,
//!   unregistered when it turns off and when the window closes (`Drop`). A
//!   plain run never registers and starts no thread.
//! - **Over D-Bus, as libgamemode does:** `RegisterGame(pid)`,
//!   `UnregisterGame(pid)` and `QueryStatus(pid)` on the session bus's
//!   `com.feralinteractive.GameMode`. The calls run on their own thread:
//!   the daemon answers a registration only after its GPU optimisation
//!   (`gpuclockctl` through pkexec) ran.
//! - **What GameMode changes is the user's:** its CPU settings come from
//!   any `gamemode.ini`; its GPU part only from `/etc/gamemode.ini` (the
//!   daemon ignores `[gpu]` in the user's own file). protoHDMIcapture
//!   writes neither (docs/testing.md).
//! - **No GameMode** (no daemon, no session bus, not Linux): nothing
//!   happens; said once on stderr with `--stats`, not as a warning.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

/// What the dashboard's GPU section says about GameMode.
#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    /// Not asked: no enhancement on (the section shows no row).
    Off,
    /// Asked; the daemon has not answered yet.
    Asking,
    /// Registered, and the daemon says GameMode is active for this process.
    Active,
    /// Registered, but the daemon's status for this process is this
    /// (`QueryStatus`: 0 inactive, 1 active for others only).
    Registered(i32),
    /// No GameMode on this system: why.
    Unavailable(String),
    /// GameMode is there and said no, or a call failed: why.
    Failed(String),
}

impl Status {
    /// The dashboard's JSON: null while off.
    pub fn json(&self) -> serde_json::Value {
        use serde_json::json;
        match self {
            Status::Off => serde_json::Value::Null,
            Status::Asking => json!({ "state": "asking" }),
            Status::Active => json!({ "state": "active" }),
            Status::Registered(q) => json!({ "state": "registered", "query": q }),
            Status::Unavailable(why) => json!({ "state": "unavailable", "why": why }),
            Status::Failed(why) => json!({ "state": "failed", "why": why }),
        }
    }
}

/// Why a call did not get an answer.
#[derive(Debug, Clone, PartialEq)]
pub enum CallError {
    /// No GameMode: no daemon on the session bus, no session bus, not Linux.
    Absent(String),
    /// Anything else (a timeout, an unexpected reply). Only the D-Bus
    /// client fails a call this way.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Failed(String),
}

/// The daemon's three methods, each with the pid; their integer answer.
pub trait Client {
    fn register(&mut self, pid: i32) -> Result<i32, CallError>;
    fn unregister(&mut self, pid: i32) -> Result<i32, CallError>;
    fn query(&mut self, pid: i32) -> Result<i32, CallError>;
}

impl Client for Box<dyn Client> {
    fn register(&mut self, pid: i32) -> Result<i32, CallError> {
        (**self).register(pid)
    }
    fn unregister(&mut self, pid: i32) -> Result<i32, CallError> {
        (**self).unregister(pid)
    }
    fn query(&mut self, pid: i32) -> Result<i32, CallError> {
        (**self).query(pid)
    }
}

/// The decision, without a thread: what to call for what is wanted.
pub struct Requests<C: Client> {
    client: C,
    pid: i32,
    registered: bool,
    /// The status the last call left.
    last: Status,
}

impl<C: Client> Requests<C> {
    pub fn new(client: C, pid: i32) -> Self {
        Self { client, pid, registered: false, last: Status::Off }
    }

    /// Registers for `on`, unregisters for off; the status after it, None
    /// where nothing was called.
    pub fn apply(&mut self, on: bool) -> Option<Status> {
        let s = self.call(on)?;
        self.last = s.clone();
        Some(s)
    }

    /// The status the last call left (Off before any).
    pub fn last(&self) -> &Status {
        &self.last
    }

    fn call(&mut self, on: bool) -> Option<Status> {
        match (on, self.registered) {
            (true, false) => Some(self.register()),
            (false, true) => {
                self.registered = false;
                Some(match self.client.unregister(self.pid) {
                    Ok(0) => Status::Off,
                    Ok(r) => Status::Failed(format!("UnregisterGame answered {r}")),
                    Err(CallError::Absent(why) | CallError::Failed(why)) => Status::Failed(format!("UnregisterGame: {why}")),
                })
            }
            _ => None,
        }
    }

    fn register(&mut self) -> Status {
        match self.client.register(self.pid) {
            // libgamemode's answers: 0 accepted, -2 rejected (the
            // daemon's whitelist or blacklist), anything else failed.
            Ok(0) => {
                self.registered = true;
                match self.client.query(self.pid) {
                    Ok(2) => Status::Active,
                    Ok(q) => Status::Registered(q),
                    Err(CallError::Absent(why) | CallError::Failed(why)) => Status::Failed(format!("QueryStatus: {why}")),
                }
            }
            Ok(-2) => Status::Failed("GameMode rejected protoHDMIcapture (its gamemode.ini's whitelist or blacklist)".into()),
            Ok(r) => Status::Failed(format!("RegisterGame answered {r}")),
            Err(CallError::Absent(why)) => Status::Unavailable(why),
            Err(CallError::Failed(why)) => Status::Failed(format!("RegisterGame: {why}")),
        }
    }
}

/// The "not available" note was said (once a process, not once a window).
// process-global: the note is once a process, across its windows
static SAID: AtomicBool = AtomicBool::new(false);

/// What stderr says of a new status: with `--stats` the outcome, a failure
/// always.
fn say(status: &Status, stats: bool) {
    match status {
        Status::Active if stats => eprintln!("gamemode: active for protoHDMIcapture"),
        Status::Registered(q) if stats => eprintln!("gamemode: registered, but its status for this process is {q} (2 is active)"),
        Status::Off if stats => eprintln!("gamemode: unregistered"),
        Status::Unavailable(why) if stats && !SAID.swap(true, Ordering::Relaxed) => {
            eprintln!("note: GameMode not available: {why}")
        }
        Status::Failed(why) => eprintln!("warning: GameMode: {why}"),
        _ => {}
    }
}

/// The client a worker thread makes for itself.
pub type MakeClient = fn() -> Box<dyn Client>;

/// GameMode for one window: a worker thread from the first `set(true)`,
/// unregistering when dropped.
pub struct GameMode {
    make: MakeClient,
    stats: bool,
    wanted: bool,
    status: Arc<Mutex<Status>>,
    worker: Option<(mpsc::Sender<bool>, thread::JoinHandle<()>)>,
}

impl GameMode {
    /// GameMode through the system's D-Bus (none off Linux).
    pub fn new(stats: bool) -> Self {
        Self::with(system, stats)
    }

    pub fn with(make: MakeClient, stats: bool) -> Self {
        Self { make, stats, wanted: false, status: Arc::new(Mutex::new(Status::Off)), worker: None }
    }

    /// GameMode is wanted (an enhancement is on).
    #[cfg(test)]
    pub fn wanted(&self) -> bool {
        self.wanted
    }

    /// What the dashboard reads.
    pub fn status(&self) -> Arc<Mutex<Status>> {
        self.status.clone()
    }

    /// Wants GameMode for `on` (an enhancement is on).
    pub fn set(&mut self, on: bool) {
        if on == self.wanted {
            return;
        }
        self.wanted = on;
        if self.worker.is_none() {
            if !on {
                return;
            }
            match self.start() {
                Ok(w) => self.worker = Some(w),
                Err(e) => {
                    let s = Status::Failed(e);
                    say(&s, self.stats);
                    *self.status.lock().expect("the GameMode status's lock is never poisoned") = s;
                    return;
                }
            }
        }
        if let Some((tx, _)) = &self.worker {
            *self.status.lock().expect("the GameMode status's lock is never poisoned") = if on { Status::Asking } else { Status::Off };
            // A send fails only once the worker is gone (it panicked): the
            // join in Drop says so.
            let _ = tx.send(on);
        }
    }

    fn start(&self) -> Result<(mpsc::Sender<bool>, thread::JoinHandle<()>), String> {
        let (tx, rx) = mpsc::channel::<bool>();
        let (make, stats, status) = (self.make, self.stats, self.status.clone());
        let thread = thread::Builder::new()
            .name("gamemode".into())
            .spawn(move || {
                let mut requests = Requests::new(make(), std::process::id() as i32);
                let mut publish = |on: bool| {
                    if let Some(s) = requests.apply(on) {
                        say(&s, stats);
                    }
                    // What the latest wish left, also where it called
                    // nothing (off and on again before the thread woke: still
                    // registered). A wish made since is not overwritten: it
                    // shows "asking" or off until its own turn.
                    let mut shown = status.lock().expect("the GameMode status's lock is never poisoned");
                    if (*shown == Status::Asking) == on {
                        *shown = requests.last().clone();
                    }
                };
                // Each wish in turn, the latest last; the window closing
                // (the sender dropped) unregisters.
                while let Ok(mut on) = rx.recv() {
                    while let Ok(later) = rx.try_recv() {
                        on = later;
                    }
                    publish(on);
                }
                publish(false);
            })
            .map_err(|e| format!("the GameMode thread: {e}"))?;
        Ok((tx, thread))
    }
}

impl Drop for GameMode {
    fn drop(&mut self) {
        if let Some((tx, thread)) = self.worker.take() {
            drop(tx);
            if thread.join().is_err() {
                eprintln!("warning: the GameMode thread panicked");
            }
        }
    }
}

/// A system without GameMode, for tests that must not reach the real one.
#[cfg(test)]
pub fn absent() -> Box<dyn Client> {
    struct Absent;
    impl Client for Absent {
        fn register(&mut self, _: i32) -> Result<i32, CallError> {
            Err(CallError::Absent("a test".into()))
        }
        fn unregister(&mut self, _: i32) -> Result<i32, CallError> {
            Err(CallError::Absent("a test".into()))
        }
        fn query(&mut self, _: i32) -> Result<i32, CallError> {
            Err(CallError::Absent("a test".into()))
        }
    }
    Box::new(Absent)
}

#[cfg(target_os = "linux")]
fn system() -> Box<dyn Client> {
    Box::new(dbus_client::DBus)
}

#[cfg(not(target_os = "linux"))]
fn system() -> Box<dyn Client> {
    struct NotLinux;
    impl Client for NotLinux {
        fn register(&mut self, _: i32) -> Result<i32, CallError> {
            Err(CallError::Absent("GameMode is Linux only".into()))
        }
        fn unregister(&mut self, _: i32) -> Result<i32, CallError> {
            Err(CallError::Absent("GameMode is Linux only".into()))
        }
        fn query(&mut self, _: i32) -> Result<i32, CallError> {
            Err(CallError::Absent("GameMode is Linux only".into()))
        }
    }
    Box::new(NotLinux)
}

#[cfg(target_os = "linux")]
mod dbus_client {
    use super::{CallError, Client};
    use dbus::{BusType, Connection, Message};

    const NAME: &str = "com.feralinteractive.GameMode";
    const PATH: &str = "/com/feralinteractive/GameMode";
    /// The daemon answers a registration after its GPU optimisation ran.
    const TIMEOUT_MS: i32 = 10_000;

    /// The session bus's GameMode daemon, a connection a call.
    pub struct DBus;

    impl DBus {
        fn call(method: &str, pid: i32) -> Result<i32, CallError> {
            let conn =
                Connection::get_private(BusType::Session).map_err(|e| CallError::Absent(format!("no session bus ({})", text(&e))))?;
            let msg = Message::new_method_call(NAME, PATH, NAME, method).map_err(CallError::Failed)?.append1(pid);
            let reply = conn.send_with_reply_and_block(msg, TIMEOUT_MS).map_err(|e| match e.name() {
                Some("org.freedesktop.DBus.Error.ServiceUnknown" | "org.freedesktop.DBus.Error.NameHasNoOwner") => {
                    CallError::Absent(format!("no {NAME} on the session bus (gamemoded is not installed or not running)"))
                }
                _ => CallError::Failed(text(&e)),
            })?;
            reply.read1::<i32>().map_err(|e| CallError::Failed(format!("{method}'s reply: {e}")))
        }
    }

    fn text(e: &dbus::Error) -> String {
        match (e.name(), e.message()) {
            (Some(n), Some(m)) => format!("{n}: {m}"),
            (Some(n), None) => n.to_string(),
            (None, Some(m)) => m.to_string(),
            (None, None) => "unknown D-Bus error".to_string(),
        }
    }

    impl Client for DBus {
        fn register(&mut self, pid: i32) -> Result<i32, CallError> {
            Self::call("RegisterGame", pid)
        }
        fn unregister(&mut self, pid: i32) -> Result<i32, CallError> {
            Self::call("UnregisterGame", pid)
        }
        fn query(&mut self, pid: i32) -> Result<i32, CallError> {
            Self::call("QueryStatus", pid)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake daemon: the calls it got, and fixed answers.
    #[derive(Clone)]
    struct Fake {
        calls: Arc<Mutex<Vec<String>>>,
        register: Result<i32, CallError>,
        query: i32,
    }

    impl Fake {
        fn new(register: Result<i32, CallError>, query: i32) -> Self {
            Self { calls: Arc::default(), register, query }
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Client for Fake {
        fn register(&mut self, pid: i32) -> Result<i32, CallError> {
            self.calls.lock().unwrap().push(format!("RegisterGame({pid})"));
            self.register.clone()
        }
        fn unregister(&mut self, pid: i32) -> Result<i32, CallError> {
            self.calls.lock().unwrap().push(format!("UnregisterGame({pid})"));
            Ok(0)
        }
        fn query(&mut self, pid: i32) -> Result<i32, CallError> {
            self.calls.lock().unwrap().push(format!("QueryStatus({pid})"));
            Ok(self.query)
        }
    }

    #[test]
    fn an_enhancement_turning_on_and_off_registers_and_unregisters() {
        let fake = Fake::new(Ok(0), 2);
        let mut r = Requests::new(fake.clone(), 42);
        // A plain run: off, nothing called.
        assert_eq!(r.apply(false), None);
        assert!(fake.calls().is_empty());
        // On: registered, and the daemon says active for us.
        assert_eq!(r.apply(true), Some(Status::Active));
        // Still on (a second update): nothing more.
        assert_eq!(r.apply(true), None);
        // Off: unregistered.
        assert_eq!(r.apply(false), Some(Status::Off));
        assert_eq!(r.apply(false), None);
        // Toggled on again.
        assert_eq!(r.apply(true), Some(Status::Active));
        assert_eq!(fake.calls(), ["RegisterGame(42)", "QueryStatus(42)", "UnregisterGame(42)", "RegisterGame(42)", "QueryStatus(42)"]);
    }

    #[test]
    fn no_gamemode_or_a_refusal_registers_nothing_to_undo() {
        // No daemon: unavailable, and turning off calls nothing.
        let fake = Fake::new(Err(CallError::Absent("no daemon".into())), 0);
        let mut r = Requests::new(fake.clone(), 7);
        assert_eq!(r.apply(true), Some(Status::Unavailable("no daemon".into())));
        assert_eq!(r.apply(false), None);
        assert_eq!(fake.calls(), ["RegisterGame(7)"]);
        // Rejected (-2) and failed (-1): said, nothing to unregister.
        for answer in [-2, -1] {
            let fake = Fake::new(Ok(answer), 0);
            let mut r = Requests::new(fake.clone(), 7);
            assert!(matches!(r.apply(true), Some(Status::Failed(_))), "{answer}");
            assert_eq!(r.apply(false), None);
            assert_eq!(fake.calls(), ["RegisterGame(7)"]);
        }
        // A call that failed (a timeout): a warning, nothing registered.
        let fake = Fake::new(Err(CallError::Failed("timed out".into())), 0);
        let mut r = Requests::new(fake.clone(), 7);
        assert_eq!(r.apply(true), Some(Status::Failed("RegisterGame: timed out".into())));
        assert_eq!(r.apply(false), None);
        // Registered, but not active for this process: shown as such, and
        // still unregistered.
        let fake = Fake::new(Ok(0), 1);
        let mut r = Requests::new(fake.clone(), 7);
        assert_eq!(r.apply(true), Some(Status::Registered(1)));
        assert_eq!(r.apply(false), Some(Status::Off));
    }

    // process-global: the fake a worker thread's `MakeClient` (a plain fn) reads
    static WORKER_FAKE: Mutex<Option<Fake>> = Mutex::new(None);

    fn worker_fake() -> Box<dyn Client> {
        Box::new(WORKER_FAKE.lock().unwrap().clone().expect("the test set its fake"))
    }

    #[test]
    fn the_window_registers_on_its_thread_and_unregisters_when_it_closes() {
        let fake = Fake::new(Ok(0), 2);
        *WORKER_FAKE.lock().unwrap() = Some(fake.clone());
        let pid = std::process::id() as i32;
        // A plain run: no thread, nothing called, no row.
        let mut g = GameMode::with(worker_fake, false);
        g.set(false);
        assert!(g.worker.is_none());
        assert_eq!(*g.status().lock().unwrap(), Status::Off);
        // On: the worker registers; the status ends active.
        g.set(true);
        let status = g.status();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while *status.lock().unwrap() != Status::Active {
            assert!(std::time::Instant::now() < deadline, "still {:?}", status.lock().unwrap());
            thread::sleep(std::time::Duration::from_millis(5));
        }
        // The window closes with it on: unregistered before the drop returns.
        drop(g);
        assert_eq!(fake.calls(), [format!("RegisterGame({pid})"), format!("QueryStatus({pid})"), format!("UnregisterGame({pid})")]);
    }

    /// Against the running gamemoded (`cargo test -- --ignored gamemode`):
    /// registers this process, checks the daemon counts it, unregisters.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "needs gamemoded on the session bus"]
    fn registers_with_the_running_gamemoded() {
        let pid = std::process::id() as i32;
        let mut r = Requests::new(dbus_client::DBus, pid);
        assert_eq!(r.apply(true), Some(Status::Active));
        let count = std::process::Command::new("gamemoded").arg("-s").output().expect("gamemoded -s");
        let said = String::from_utf8_lossy(&count.stdout);
        assert!(said.contains("is active"), "gamemoded -s: {said}");
        assert_eq!(r.apply(false), Some(Status::Off));
        // 0, or 1 where another program holds GameMode: never 2 again.
        let mut d = dbus_client::DBus;
        assert!(matches!(d.query(pid), Ok(0 | 1)), "not registered once unregistered");
    }
}
