//! The audio output's level, as the overlay's m, 9 and 0 change it: the
//! platform-neutral half (pw.rs reads and sets it on PipeWire).

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
    /// This level: a restarted loopback given the last one's (#29), as
    /// view.sh's `audio_state_apply`.
    Set(Level),
}

impl Level {
    pub fn changed(self, change: LevelChange) -> Self {
        match change {
            LevelChange::Mute => Self { mute: !self.mute, ..self },
            LevelChange::Volume(by) => Self { volume: (self.volume as i32 + by).clamp(0, 100) as u32, ..self },
            LevelChange::Set(level) => level,
        }
    }
}
