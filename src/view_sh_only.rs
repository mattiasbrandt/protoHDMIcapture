//! Settings only view.sh's players act on (#26). The settings file is
//! shared, so the Rust viewer reads them, and says what it does with them
//! instead of ignoring them: BACKEND other than mpv and AUDIO=mpv are
//! refused with the way out; IPC and GST_IO_MODE away from their defaults
//! get a note. They retire with view.sh (#10).

use crate::settings::Settings;

/// Refused, with why and what to do instead.
const REFUSED: [(&str, &str, &str); 3] = [
    ("BACKEND", "gst", "a diagnostic player of legacy/view.sh; protoHDMIcapture plays with its own pipeline. Use legacy/view.sh, or set BACKEND=mpv"),
    ("BACKEND", "ffplay", "a diagnostic player of legacy/view.sh; protoHDMIcapture plays with its own pipeline. Use legacy/view.sh, or set BACKEND=mpv"),
    ("AUDIO", "mpv", "mpv's own audio, in legacy/view.sh only; protoHDMIcapture plays the sound with its loopback. Use legacy/view.sh, or set AUDIO=loopback"),
];

/// Read by view.sh's players only: a note when set away from the default.
const NOTED: [(&str, &str); 2] = [("IPC", "mpv's IPC socket"), ("GST_IO_MODE", "the gst player's capture mode")];

/// The notes to print, or the refusal.
pub fn check(s: &Settings) -> Result<Vec<String>, String> {
    for (name, value, why) in REFUSED {
        if s.get(name) == value {
            return Err(format!("{name}={value}: {why}"));
        }
    }
    Ok(NOTED
        .into_iter()
        .filter_map(|(name, what)| {
            let default = s.rows.iter().find(|r| r.name == name).map_or("", |r| r.default.as_str());
            let v = s.get(name);
            (v != default).then(|| format!("{name}={v} is {what}, in legacy/view.sh only; protoHDMIcapture leaves it unused"))
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Settings from the defaults and these environment variables.
    fn with(vars: &[(&str, &str)]) -> Settings {
        let vars: Vec<(String, String)> = vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        Settings::resolve_with(&|k| {
            if k == "CONFIG_FILE" {
                return Some("/nonexistent/protohdmicapture.conf".into());
            }
            vars.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())
        })
    }

    #[test]
    fn view_sh_only_players_are_refused_with_the_way_out() {
        assert_eq!(
            check(&with(&[("BACKEND", "gst")])).unwrap_err(),
            "BACKEND=gst: a diagnostic player of legacy/view.sh; protoHDMIcapture plays with its own pipeline. Use legacy/view.sh, or set BACKEND=mpv"
        );
        assert!(check(&with(&[("BACKEND", "ffplay")])).unwrap_err().starts_with("BACKEND=ffplay: a diagnostic player of legacy/view.sh"));
        assert_eq!(
            check(&with(&[("AUDIO", "mpv")])).unwrap_err(),
            "AUDIO=mpv: mpv's own audio, in legacy/view.sh only; protoHDMIcapture plays the sound with its loopback. Use legacy/view.sh, or set AUDIO=loopback"
        );
    }

    #[test]
    fn mpv_and_gst_settings_get_a_note() {
        assert_eq!(check(&with(&[])), Ok(vec![]), "the defaults say nothing");
        assert_eq!(
            check(&with(&[("IPC", ""), ("GST_IO_MODE", "dmabuf")])),
            Ok(vec![
                "IPC= is mpv's IPC socket, in legacy/view.sh only; protoHDMIcapture leaves it unused".to_string(),
                "GST_IO_MODE=dmabuf is the gst player's capture mode, in legacy/view.sh only; protoHDMIcapture leaves it unused".to_string(),
            ])
        );
    }
}
