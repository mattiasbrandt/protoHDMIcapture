//! The session's share of view.sh's run directory (docs/run-dir.md): one
//! video window at a time across both viewers, and refresh matching that
//! a view.sh started meanwhile cannot undo under us.
//!
//! - `video.lock`: flock held for the whole run, the same lock view.sh
//!   takes (`take_video_lock`). Held by another session: that window is
//!   focused and this one does not start.
//! - `video.pid`: this process (the Rust viewer is its own player), for a
//!   second launch to focus; removed at the end.
//! - `refresh.restore`: a killed session's switch is recovered first; with
//!   REFRESH=match the focused monitor is switched (recorded first) and
//!   switched back at the end.
//!
//! - A player a killed view.sh left running (it keeps the card busy) is
//!   stopped under the lock, as view.sh's `kill_orphan` does.
//!
//! Not taken over (yet): `audio.lock` and its files (the viewer runs its
//! own loopback; a killed view.sh's orphan loopback is not stopped). The
//! files only view.sh's dashboard reads are not written: the Rust
//! viewer's dashboard reads `dashboard.sock` (dashboard.rs).

#[cfg(target_os = "linux")]
use std::fs::{self, File, OpenOptions};
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
#[cfg(target_os = "linux")]
use std::os::unix::fs::MetadataExt;
#[cfg(target_os = "linux")]
use std::path::Path;
use std::path::PathBuf;
#[cfg(target_os = "linux")]
use std::thread;
#[cfg(target_os = "linux")]
use std::time::Duration;

#[cfg(target_os = "linux")]
use crate::desktop::Desktop;

/// `${XDG_RUNTIME_DIR:-/tmp}/protohdmicapture`, as view.sh names it.
#[cfg(not(windows))]
pub fn run_dir() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()).unwrap_or_else(|| "/tmp".into());
    PathBuf::from(base).join("protohdmicapture")
}

/// `%LOCALAPPDATA%\protohdmicapture` (else the temporary directory's), on
/// Windows (#18).
#[cfg(windows)]
pub fn run_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty()).map_or_else(std::env::temp_dir, PathBuf::from).join("protohdmicapture")
}

#[cfg(target_os = "linux")]
pub struct Session {
    dir: PathBuf,
    /// Holds `video.lock` until dropped.
    _lock: File,
    desktop: Desktop,
    /// The restore command of a refresh switch this session made.
    restore: Option<String>,
    /// video.pid is ours: written by announce(), so ours to remove.
    announced: bool,
}

#[cfg(target_os = "linux")]
/// Another session has the video window.
#[derive(Debug, PartialEq)]
pub struct AlreadyRunning {
    pub pid: Option<u32>,
    pub focused: bool,
}

#[cfg(target_os = "linux")]
fn say(r: crate::desktop::Report) {
    match r {
        Ok(m) if !m.is_empty() => eprintln!("{m}"),
        Ok(_) => {}
        Err(m) => eprintln!("warning: {m}"),
    }
}

#[cfg(target_os = "linux")]
impl Session {
    /// Takes the video lock and recovers a killed session's refresh switch.
    pub fn start(dir: &Path, desktop: Desktop) -> Result<Result<Self, AlreadyRunning>, String> {
        fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        let lock = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(dir.join("video.lock"))
            .map_err(|e| format!("cannot open {}: {e}", dir.join("video.lock").display()))?;
        // Safety: flock on an fd this File owns.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::EWOULDBLOCK) {
                return Err(format!("cannot lock {}: {err}", dir.join("video.lock").display()));
            }
            let pid = fs::read_to_string(dir.join("video.pid")).ok().and_then(|p| p.trim().parse().ok());
            let focused = pid.is_some_and(|p| desktop.focus_pid(p));
            return Ok(Err(AlreadyRunning { pid, focused }));
        }
        // With the lock held, the session that wrote video.pid is gone.
        if let Some(m) = kill_orphan(&dir.join("video.pid"), &PLAYER_RE, "player") {
            eprintln!("warning: {m}");
        }
        let record = dir.join("refresh.restore");
        // With the lock held, a record is not a live session's.
        if record.exists() {
            say(desktop.refresh_recover(&record));
        }
        Ok(Ok(Self { dir: dir.to_path_buf(), _lock: lock, desktop, restore: None, announced: false }))
    }

    /// video.pid, once the viewer is about to show its window (a record
    /// after what it records: a second launch focuses it).
    pub fn announce(&mut self) -> Result<(), String> {
        let path = self.dir.join("video.pid");
        fs::write(&path, format!("{}\n", std::process::id())).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        self.announced = true;
        Ok(())
    }

    /// REFRESH=match, just before the window opens: the focused monitor to
    /// its mode nearest `fps`, switched back when the session ends.
    pub fn match_refresh(&mut self, fps: f64) {
        let record = self.dir.join("refresh.restore");
        if record.exists() {
            // A switch now would record the switched rate as the original.
            eprintln!("warning: REFRESH=match skipped: a killed session's refresh switch is still unresolved ({})", record.display());
            return;
        }
        let (restore, report) = self.desktop.refresh_match(fps, &record);
        say(report);
        self.restore = restore;
    }
}

#[cfg(target_os = "linux")]
impl Drop for Session {
    /// The display back, then the files; the lock goes with the fd. A
    /// failed restore keeps the record for the next run of either viewer.
    fn drop(&mut self) {
        if let Some(restore) = self.restore.take() {
            say(self.desktop.refresh_restore(&restore, &self.dir.join("refresh.restore")));
        }
        // Only our own: a start that failed before announce() leaves a
        // killed session's record for the next one to find its orphan by.
        if !self.announced {
            return;
        }
        let pid = self.dir.join("video.pid");
        match fs::remove_file(&pid) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                eprintln!("warning: cannot remove {}: {e}", pid.display());
            }
            _ => {}
        }
    }
}

#[cfg(target_os = "linux")]
/// view.sh's players, as its kill_orphan matches them.
static PLAYER_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"^(mpv .*--title=protoHDMIcapture|ffplay .*-window_title protoHDMIcapture|gst-launch-1\.0 .*v4l2src device=)")
        .expect("a valid pattern")
});

#[cfg(target_os = "linux")]
/// view.sh's `kill_orphan`: the process in `pidfile`, if it is still there,
/// the user's own, and its command line matches `re` (the PID may be
/// anyone's by now), is stopped: TERM, up to 2 s, then KILL. Says what it
/// did, if anything. Call only with the lock its session held.
fn kill_orphan(pidfile: &Path, re: &regex::Regex, what: &str) -> Option<String> {
    let pid: i32 = fs::read_to_string(pidfile).ok()?.trim().parse().ok().filter(|p| *p > 0)?;
    let proc_dir = PathBuf::from(format!("/proc/{pid}"));
    // Safety: geteuid has no preconditions.
    if fs::metadata(&proc_dir).ok()?.uid() != unsafe { libc::geteuid() } {
        return None;
    }
    let cmd: String = fs::read(proc_dir.join("cmdline")).ok()?.iter().map(|&b| if b == 0 { ' ' } else { b as char }).collect();
    if !re.is_match(&cmd) {
        return None;
    }
    let alive = || unsafe { libc::kill(pid, 0) } == 0;
    // Safety: kill on a pid checked above as our own matching process.
    unsafe { libc::kill(pid, libc::SIGTERM) };
    for _ in 0..20 {
        if !alive() {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    if alive() {
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    Some(format!("stopped the {what} (pid {pid}) that a killed session left running"))
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("phc-session-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn one_video_window_across_both_viewers() {
        let dir = tmp("lock");
        let mut first = Session::start(&dir, Desktop::Generic).unwrap().unwrap();
        assert!(!dir.join("video.pid").exists(), "not before the window is about to show");
        first.announce().unwrap();
        assert_eq!(fs::read_to_string(dir.join("video.pid")).unwrap(), format!("{}\n", std::process::id()));
        // A second session (flock locks are per open file, so this process
        // stands in for another) finds it held and the first's pid.
        let second = Session::start(&dir, Desktop::Generic).unwrap();
        assert_eq!(second.err(), Some(AlreadyRunning { pid: Some(std::process::id()), focused: false }));
        // view.sh's own test of the lock: flock -n on the same file fails.
        let held = std::process::Command::new("flock").args(["-n"]).arg(dir.join("video.lock")).arg("true").status().unwrap();
        assert!(!held.success(), "view.sh would see the lock held");
        drop(first);
        assert!(!dir.join("video.pid").exists());
        let free = std::process::Command::new("flock").args(["-n"]).arg(dir.join("video.lock")).arg("true").status().unwrap();
        assert!(free.success(), "released at the end");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_failed_start_leaves_a_killed_sessions_pid() {
        let dir = tmp("pid");
        fs::create_dir_all(&dir).unwrap();
        // Not a player of ours: left running, and its record left.
        fs::write(dir.join("video.pid"), "1\n").unwrap();
        drop(Session::start(&dir, Desktop::Generic).unwrap().unwrap());
        assert_eq!(fs::read_to_string(dir.join("video.pid")).unwrap(), "1\n", "never announced: not ours to remove");
        fs::remove_dir_all(&dir).unwrap();
    }

    /// A stand-in process with this command line (argv[0] set by exec -a).
    fn stand_in(cmdline: &str) -> std::process::Child {
        let child = std::process::Command::new("bash").args(["-c", &format!("exec -a '{cmdline}' sleep 30")]).spawn().unwrap();
        // Until the exec has happened.
        for _ in 0..50 {
            let cmd = fs::read(format!("/proc/{}/cmdline", child.id())).unwrap_or_default();
            if cmd.starts_with(cmdline.as_bytes()) {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        child
    }

    #[test]
    fn a_killed_view_shs_player_is_stopped_and_nothing_else() {
        let dir = tmp("orphan");
        fs::create_dir_all(&dir).unwrap();
        let mut orphan = stand_in("mpv --profile=low-latency --title=protoHDMIcapture av://v4l2:/dev/video0");
        fs::write(dir.join("video.pid"), format!("{}\n", orphan.id())).unwrap();
        let msg = kill_orphan(&dir.join("video.pid"), &PLAYER_RE, "player");
        assert_eq!(msg, Some(format!("stopped the player (pid {}) that a killed session left running", orphan.id())));
        let status = orphan.wait().unwrap();
        assert!(!status.success(), "it was signalled");

        // Someone else's process at that pid now: left alone.
        let mut other = stand_in("sleep-like-anything --title=protoHDMIcapture");
        fs::write(dir.join("video.pid"), format!("{}\n", other.id())).unwrap();
        assert_eq!(kill_orphan(&dir.join("video.pid"), &PLAYER_RE, "player"), None);
        assert!(other.try_wait().unwrap().is_none(), "still running");
        other.kill().unwrap();
        other.wait().unwrap();

        // A pid that is gone, and junk: nothing.
        fs::write(dir.join("video.pid"), "999999999\n").unwrap();
        assert_eq!(kill_orphan(&dir.join("video.pid"), &PLAYER_RE, "player"), None);
        fs::write(dir.join("video.pid"), "junk\n").unwrap();
        assert_eq!(kill_orphan(&dir.join("video.pid"), &PLAYER_RE, "player"), None);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_standing_record_blocks_a_new_switch() {
        let dir = tmp("record");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("refresh.restore"), "DP-2|60|59.95|hl.monitor({})\n").unwrap();
        // Generic cannot recover it: kept, and REFRESH=match does not switch.
        let mut s = Session::start(&dir, Desktop::Generic).unwrap().unwrap();
        s.match_refresh(60.0);
        assert!(s.restore.is_none());
        drop(s);
        assert!(dir.join("refresh.restore").exists());
        fs::remove_dir_all(&dir).unwrap();
    }
}
