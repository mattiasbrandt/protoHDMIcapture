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
//! Not taken over (yet): `audio.lock` and its files (the viewer runs its
//! own loopback), the orphan cleanup of a killed view.sh's mpv, and the
//! files only the dashboard (#16) reads.

use std::fs::{self, File, OpenOptions};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use crate::desktop::Desktop;

/// `${XDG_RUNTIME_DIR:-/tmp}/protohdmicapture`, as view.sh names it.
pub fn run_dir() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()).unwrap_or_else(|| "/tmp".into());
    PathBuf::from(base).join("protohdmicapture")
}

pub struct Session {
    dir: PathBuf,
    /// Holds `video.lock` until dropped.
    _lock: File,
    desktop: Desktop,
    /// The restore command of a refresh switch this session made.
    restore: Option<String>,
}

/// Another session has the video window.
#[derive(Debug, PartialEq)]
pub struct AlreadyRunning {
    pub pid: Option<u32>,
    pub focused: bool,
}

fn say(r: crate::desktop::Report) {
    match r {
        Ok(m) if !m.is_empty() => eprintln!("{m}"),
        Ok(_) => {}
        Err(m) => eprintln!("warning: {m}"),
    }
}

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
        let record = dir.join("refresh.restore");
        // With the lock held, a record is not a live session's.
        if record.exists() {
            say(desktop.refresh_recover(&record));
        }
        Ok(Ok(Self { dir: dir.to_path_buf(), _lock: lock, desktop, restore: None }))
    }

    /// video.pid, once the viewer is about to show its window (a record
    /// after what it records: a second launch focuses it).
    pub fn announce(&self) -> Result<(), String> {
        let path = self.dir.join("video.pid");
        fs::write(&path, format!("{}\n", std::process::id())).map_err(|e| format!("cannot write {}: {e}", path.display()))
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

impl Drop for Session {
    /// The display back, then the files; the lock goes with the fd. A
    /// failed restore keeps the record for the next run of either viewer.
    fn drop(&mut self) {
        if let Some(restore) = self.restore.take() {
            say(self.desktop.refresh_restore(&restore, &self.dir.join("refresh.restore")));
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

#[cfg(test)]
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
        let first = Session::start(&dir, Desktop::Generic).unwrap().unwrap();
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
