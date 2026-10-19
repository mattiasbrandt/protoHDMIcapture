//! The live changes (#45): the settings the viewer changes while the feed
//! runs, from their start values to the overlay's w. Each one is an entry
//! of `ENTRIES`, which says how it is read from the settings and how it is
//! spelled back, as the settings file spells it; the entries are the rows
//! settings.rows flags `save`, in its order (a test holds them to it).
//!
//! DELAY is the loopback's, not held here: what it is now comes in as the
//! loopback's delay in ms while the loopback runs, and None means it is
//! neither a change nor saved.

use std::collections::BTreeMap;

use crate::app::WindowMode;
use crate::picture::{Colour, Levels, Picture};
use crate::scale::Scaling;
use crate::settings::Settings;

/// The live values as w would write them, by setting.
pub type Values = BTreeMap<&'static str, String>;

/// One live setting: read from its value into the live values, and spelled
/// back from them (None: no value to save, DELAY without the loopback).
struct Entry {
    name: &'static str,
    /// Part of the picture (SCALING too): what the setup wizard's feed
    /// reads (`read_picture_lenient`).
    picture: bool,
    read: fn(&mut LiveChanges, &str) -> Result<(), String>,
    spell: fn(&LiveChanges, Option<u32>) -> Option<String>,
}

fn colour(name: &str, v: &str) -> Result<i32, String> {
    v.parse().map_err(|_| format!("{name}={v} is not a whole number"))
}

/// settings.rows' `save` rows, in its order.
const ENTRIES: [Entry; 8] = [
    Entry {
        name: "DELAY",
        picture: false,
        read: |_, v| delay_ms(v).map(drop),
        spell: |_, delay_ms| delay_ms.map(seconds),
    },
    Entry {
        name: "WINDOW",
        picture: false,
        // WINDOW_SIZE is read first: a windowed WINDOW takes its size.
        read: |l, v| {
            l.window = WindowMode::from_settings(v, &format!("{}x{}", l.window_size.0, l.window_size.1))?;
            Ok(())
        },
        spell: |l, _| Some(l.window.names().0.into()),
    },
    Entry {
        name: "SCALING",
        picture: true,
        read: |l, v| {
            l.scaling = Scaling::from_settings(v)?;
            Ok(())
        },
        spell: |l, _| Some(l.scaling.as_str().into()),
    },
    Entry {
        name: "LEVELS",
        picture: true,
        read: |l, v| {
            l.picture.levels = Levels::from_settings(v)?;
            Ok(())
        },
        spell: |l, _| Some(l.picture.levels.as_str().into()),
    },
    Entry {
        name: "BRIGHTNESS",
        picture: true,
        read: |l, v| {
            l.picture.brightness = colour("BRIGHTNESS", v)?;
            Ok(())
        },
        spell: |l, _| Some(l.picture.brightness.to_string()),
    },
    Entry {
        name: "CONTRAST",
        picture: true,
        read: |l, v| {
            l.picture.contrast = colour("CONTRAST", v)?;
            Ok(())
        },
        spell: |l, _| Some(l.picture.contrast.to_string()),
    },
    Entry {
        name: "SATURATION",
        picture: true,
        read: |l, v| {
            l.picture.saturation = colour("SATURATION", v)?;
            Ok(())
        },
        spell: |l, _| Some(l.picture.saturation.to_string()),
    },
    Entry {
        name: "GAMMA",
        picture: true,
        read: |l, v| {
            l.picture.gamma = colour("GAMMA", v)?;
            Ok(())
        },
        spell: |l, _| Some(l.picture.gamma.to_string()),
    },
];

/// DELAY's default, for a start that cannot read it.
pub const DEFAULT_DELAY_MS: u32 = 50;

/// DELAY in seconds, as the settings spell it, in ms.
pub fn delay_ms(seconds: &str) -> Result<u32, String> {
    let s: f64 = seconds.parse().map_err(|_| format!("DELAY={seconds} is not a number of seconds"))?;
    if !(0.0..=1.0).contains(&s) {
        return Err(format!("DELAY={seconds} is not between 0 and 1 s"));
    }
    Ok((s * 1000.0).round() as u32)
}

/// A delay in ms as the settings spell DELAY: seconds, as the mpv script
/// writes it.
pub fn seconds(ms: u32) -> String {
    format!("{:.3}", f64::from(ms) / 1000.0)
}

/// The live values and what w compares them with.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveChanges {
    window: WindowMode,
    /// WINDOW_SIZE, for f's windowed mode.
    window_size: (u32, u32),
    picture: Picture,
    scaling: Scaling,
    /// The values at the start or at the last save (`mark_baseline`).
    baseline: Values,
}

impl LiveChanges {
    /// Every live setting at settings.rows' default.
    pub const DEFAULT: Self = Self {
        window: WindowMode::Fullscreen,
        window_size: (1280, 720),
        picture: Picture::DEFAULT,
        scaling: Scaling::Balanced,
        baseline: BTreeMap::new(),
    };

    /// The start values (the viewer's): a value that cannot be read is the
    /// error.
    pub fn read(s: &Settings) -> Result<Self, String> {
        let mut l = Self::DEFAULT;
        l.window_size = window_size(s.get("WINDOW_SIZE"))?;
        for e in &ENTRIES {
            (e.read)(&mut l, s.get(e.name))?;
        }
        Ok(l)
    }

    /// The picture settings and SCALING at start, for the setup wizard's
    /// feed, where they get fixed: a value that cannot be read stays at its
    /// default, and the warnings say so. Every other live value stays at its
    /// default, unread and unwarned: the wizard's window is its own, and its
    /// sync stage says an invalid DELAY itself.
    pub fn read_picture_lenient(s: &Settings) -> (Self, Vec<String>) {
        let mut l = Self::DEFAULT;
        let mut warnings = Vec::new();
        for e in ENTRIES.iter().filter(|e| e.picture) {
            if let Err(why) = (e.read)(&mut l, s.get(e.name)) {
                let default = (e.spell)(&Self::DEFAULT, Some(DEFAULT_DELAY_MS)).expect("every default spells");
                warnings.push(format!("{why}; {}={default} instead", e.name));
            }
        }
        (l, warnings)
    }

    /// These values with the display mode and size given instead (the
    /// wizard's window).
    pub fn with_window(self, window: WindowMode, size: (u32, u32)) -> Self {
        Self { window, window_size: size, ..self }
    }

    pub fn window(&self) -> WindowMode {
        self.window
    }

    pub fn window_size(&self) -> (u32, u32) {
        self.window_size
    }

    pub fn picture(&self) -> Picture {
        self.picture
    }

    pub fn scaling(&self) -> Scaling {
        self.scaling
    }

    /// f: the next display mode (WINDOW's order).
    pub fn cycle_window(&mut self) -> WindowMode {
        self.window = self.window.next(self.window_size);
        self.window
    }

    /// c.
    pub fn cycle_scaling(&mut self) {
        self.scaling = self.scaling.next();
    }

    /// l.
    pub fn cycle_levels(&mut self) {
        self.picture.cycle_levels();
    }

    /// 1..8: the colour value moved, clamped; the new value.
    pub fn adjust_colour(&mut self, colour: Colour, by: i32) -> i32 {
        self.picture.adjust(colour, by);
        let p = &self.picture;
        match colour {
            Colour::Contrast => p.contrast,
            Colour::Brightness => p.brightness,
            Colour::Gamma => p.gamma,
            Colour::Saturation => p.saturation,
        }
    }

    /// r: the four colour values to 0.
    pub fn reset_colour(&mut self) {
        self.picture.reset_colour();
    }

    /// Each live value as w would write it; DELAY only with `delay_ms`, the
    /// running loopback's.
    pub fn values(&self, delay_ms: Option<u32>) -> Values {
        ENTRIES.iter().filter_map(|e| Some((e.name, (e.spell)(self, delay_ms)?))).collect()
    }

    /// What w would save: each value that differs from the baseline, as
    /// `NAME=value`, in settings.rows' order.
    pub fn changes(&self, delay_ms: Option<u32>) -> Vec<String> {
        let now = self.values(delay_ms);
        ENTRIES
            .iter()
            .filter_map(|e| {
                let v = now.get(e.name)?;
                (self.baseline.get(e.name) != Some(v)).then(|| format!("{}={v}", e.name))
            })
            .collect()
    }

    /// The values now are what w compares with from here (the start).
    pub fn mark_baseline(&mut self, delay_ms: Option<u32>) {
        self.baseline = self.values(delay_ms);
    }

    /// w: the changes written to the settings file, as legacy's `--save`
    /// writes them, and the new baseline. Ok with what was saved, empty
    /// when nothing changed (nothing is written then).
    pub fn save(&mut self, s: &Settings, delay_ms: Option<u32>) -> Result<Vec<String>, String> {
        let changed = self.changes(delay_ms);
        if changed.is_empty() {
            return Ok(changed);
        }
        let pairs: Vec<(&str, &str)> = changed.iter().filter_map(|kv| kv.split_once('=')).collect();
        s.save(&pairs)?;
        self.mark_baseline(delay_ms);
        Ok(changed)
    }
}

/// WINDOW_SIZE: the windowed mode's width and height.
fn window_size(v: &str) -> Result<(u32, u32), String> {
    match WindowMode::from_settings("windowed", v)? {
        WindowMode::Windowed { width, height } => Ok((width, height)),
        _ => unreachable!("windowed is Windowed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Settings from a temp config file, and the file.
    fn settings(config: &str) -> (Settings, std::path::PathBuf) {
        // as_ptr: a &str's {:p} carries its length after a ':' (no file name on Windows).
        let dir = std::env::temp_dir().join(format!("phc-live-{}-{:p}", std::process::id(), config.as_ptr()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("config");
        std::fs::write(&file, config).unwrap();
        let path = file.display().to_string();
        (Settings::resolve_with(&move |k| (k == "CONFIG_FILE").then(|| path.clone())), file)
    }

    #[test]
    fn the_entries_are_lib_settings_shs_save_rows_in_its_order() {
        let flagged: Vec<String> = crate::settings::with_flag("save");
        let entries: Vec<&str> = ENTRIES.iter().map(|e| e.name).collect();
        assert_eq!(entries, flagged);
    }

    #[test]
    fn read_change_save_and_read_it_back() {
        let (s, file) = settings("# mine\nLEVELS=auto\nCONTRAST=2\nBRIGHTNESS=-4\nSATURATION=7\nWINDOW_SIZE=1600x900\n");
        let mut l = LiveChanges::read(&s).unwrap();
        let p = l.picture();
        assert_eq!((p.contrast, p.brightness, p.saturation, p.gamma), (2, -4, 7, 0));
        assert_eq!(l.window_size(), (1600, 900));
        l.mark_baseline(Some(50));
        assert!(l.changes(Some(50)).is_empty());

        l.cycle_levels();
        l.cycle_scaling();
        assert_eq!(l.adjust_colour(Colour::Gamma, -3), -3);
        assert_eq!(l.adjust_colour(Colour::Saturation, 1), 8);
        assert_eq!(l.cycle_window(), WindowMode::Borderless);
        assert_eq!(l.changes(Some(50)), ["WINDOW=borderless", "SCALING=fast", "LEVELS=limited", "SATURATION=8", "GAMMA=-3"], "settings.rows' order");
        assert_eq!(l.save(&s, Some(50)).unwrap().len(), 5);
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "# mine\nLEVELS=limited\nCONTRAST=2\nBRIGHTNESS=-4\nSATURATION=8\nWINDOW_SIZE=1600x900\nWINDOW=borderless\nSCALING=fast\nGAMMA=-3\n"
        );
        assert!(l.changes(Some(50)).is_empty(), "the save is the new baseline");
        assert_eq!(l.save(&s, Some(50)), Ok(vec![]), "nothing changed: nothing written");

        // f from borderless: windowed at WINDOW_SIZE.
        assert_eq!(l.cycle_window(), WindowMode::Windowed { width: 1600, height: 900 });
        // A fresh read of what w wrote gives the same values.
        let again = LiveChanges::read(&settings(&std::fs::read_to_string(&file).unwrap()).0).unwrap();
        assert_eq!((again.picture(), again.scaling()), (l.picture(), l.scaling()));
        // The file w wrote reads as saved and valid, as `--settings` says it.
        let (back, copy) = settings(&std::fs::read_to_string(&file).unwrap());
        assert!(back.errors().is_empty(), "{:?}", back.errors());
        for (k, v) in [("LEVELS", "limited"), ("SCALING", "fast"), ("GAMMA", "-3"), ("BRIGHTNESS", "-4"), ("SATURATION", "8")] {
            assert_eq!((back.get(k), back.from(k)), (v, crate::settings::From::Saved), "{k}");
        }
        std::fs::remove_dir_all(copy.parent().unwrap()).unwrap();
        std::fs::remove_dir_all(file.parent().unwrap()).unwrap();
    }

    #[test]
    fn delay_only_while_the_loopback_runs() {
        let (s, file) = settings("DELAY=0.08\n");
        let mut l = LiveChanges::read(&s).unwrap();
        l.mark_baseline(Some(80));
        assert_eq!(l.values(Some(80))["DELAY"], "0.080", "in seconds, as the mpv script writes it");
        assert!(!l.values(None).contains_key("DELAY"), "no loopback: no live DELAY");
        assert_eq!(l.changes(Some(95)), ["DELAY=0.095"]);
        assert!(l.changes(None).is_empty(), "DELAY is no change without the loopback");
        assert_eq!(l.save(&s, None), Ok(vec![]));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "DELAY=0.08\n");
        assert_eq!(l.save(&s, Some(95)), Ok(vec!["DELAY=0.095".to_string()]));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "DELAY=0.095\n");
        std::fs::remove_dir_all(file.parent().unwrap()).unwrap();
    }

    #[test]
    fn delay_reads_in_seconds_and_spells_back_the_same() {
        assert_eq!(delay_ms("0.05"), Ok(50));
        assert_eq!(delay_ms("1"), Ok(1000));
        assert!(delay_ms("1.5").is_err() && delay_ms("soon").is_err());
        assert_eq!(seconds(50), "0.050");
        assert_eq!(delay_ms(&seconds(123)), Ok(123));
    }

    #[test]
    fn a_strict_read_refuses_and_a_lenient_one_falls_back() {
        // Unvalidated values, as the wizard can find them.
        let (s, file) = settings("LEVELS=tv\nSCALING=sharp\nCONTRAST=5\nBRIGHTNESS=-9\nSATURATION=lots\nWINDOW=tiled\nWINDOW_SIZE=big\nDELAY=soon\n");
        assert_eq!(LiveChanges::read(&s), Err("WINDOW_SIZE=big is not WIDTHxHEIGHT".into()));
        let (l, warnings) = LiveChanges::read_picture_lenient(&s);
        assert_eq!(l.picture().levels, Levels::Auto);
        assert_eq!((l.picture().contrast, l.picture().brightness), (5, -9), "what can be read is kept");
        assert_eq!(l.picture().saturation, 0);
        assert_eq!(l.scaling(), Scaling::Balanced);
        // Only the picture and SCALING: the wizard's window is its own, and
        // its sync stage says an invalid DELAY itself.
        assert_eq!(
            warnings,
            [
                "SCALING=sharp is not balanced, fast, high-quality or config; SCALING=balanced instead",
                "LEVELS=tv is not auto, limited or full; LEVELS=auto instead",
                "SATURATION=lots is not a whole number; SATURATION=0 instead",
            ]
        );
        std::fs::remove_dir_all(file.parent().unwrap()).unwrap();
    }
}
