//! Source profiles (sources/<id>.md, the shape in legacy/lib/sources.sh): what the
//! overlay shows as setup tips, and the wake hint of the no-signal banner.
//! Read as the mpv script's `read_source` reads them.

mod embedded {
    include!(concat!(env!("OUT_DIR"), "/sources.rs"));
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub id: String,
    pub name: String,
    /// "tested 2026-09-06", "reported -", or empty.
    pub status: String,
    /// What wakes the source, if the profile says.
    pub wake: Option<String>,
    pub tips: Vec<String>,
    /// The `Suggest:` line's KEY=value words (the wizard's mode stage).
    pub suggest: Vec<String>,
}

/// The first line is the name; the rest until the first `## ` heading.
fn parse(id: &str, text: &str) -> Profile {
    let mut p = Profile { id: id.into(), name: id.into(), status: String::new(), wake: None, tips: Vec::new(), suggest: Vec::new() };
    let mut suggested = false;
    for (i, line) in text.lines().enumerate() {
        if i == 0 {
            p.name = line.strip_prefix("# ").unwrap_or(line).to_string();
        } else if line.starts_with("## ") {
            break;
        } else if let Some(s) = line.strip_prefix("Status: ") {
            p.status = s.to_string();
        } else if let Some(v) = line.strip_prefix("Suggest:").filter(|_| !suggested) {
            // `source_suggest`: the first Suggest line, split into words.
            suggested = true;
            p.suggest = v.split_whitespace().map(str::to_string).collect();
        } else if let Some(w) = line.strip_prefix("Wake: ") {
            p.wake = Some(w.to_string());
        } else if let Some(t) = line.strip_prefix("- ") {
            p.tips.push(t.to_string());
        }
    }
    p
}

/// The profile with this id, if there is one.
pub fn get(id: &str) -> Option<Profile> {
    embedded::FILES.iter().find(|(i, _)| *i == id).map(|(i, t)| parse(i, t))
}

/// Every profile id, sorted, generic last (the tips page's browse order).
pub fn ids() -> Vec<&'static str> {
    let mut ids: Vec<&str> = embedded::FILES.iter().map(|(i, _)| *i).filter(|i| *i != "generic").collect();
    ids.push("generic");
    ids
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_read_as_the_overlay_reads_them() {
        let ps5 = get("ps5").expect("sources/ps5.md");
        assert_eq!(ps5.name, "PlayStation 5");
        assert!(ps5.status.starts_with("tested"));
        assert_eq!(ps5.wake.as_deref(), Some("press the PS button on the controller"));
        assert!(ps5.tips.iter().any(|t| t.starts_with("HDCP: Off")));
        assert!(!ps5.tips.iter().any(|t| t.contains("playstation.com")), "the references are not tips");
        assert_eq!(get("nope"), None);
        assert_eq!(ps5.suggest, ["RES=1920x1080", "FPS=60"]);
        assert!(get("generic").unwrap().suggest.is_empty());
    }

    #[test]
    fn every_profile_is_embedded_generic_last() {
        let on_disk = std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/sources"))
            .unwrap()
            .filter(|e| e.as_ref().unwrap().path().extension().is_some_and(|x| x == "md"))
            .count();
        let ids = ids();
        assert_eq!(ids.len(), on_disk);
        assert_eq!(ids.last(), Some(&"generic"));
        let mut sorted = ids[..ids.len() - 1].to_vec();
        sorted.sort();
        assert_eq!(sorted, ids[..ids.len() - 1]);
    }

    #[test]
    fn parse_stops_at_the_first_subheading() {
        let p = parse("x", "# X\nStatus: reported -\n- one\n## References\n- not a tip\n");
        assert_eq!(p.tips, ["one"]);
        assert_eq!(p.wake, None);
    }
}
