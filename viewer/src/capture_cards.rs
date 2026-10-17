//! Capture card profiles (capture-cards/<chip>.md, the shape in
//! docs/capture-cards.md): which chip a USB id means, the link it needs and
//! what is known about it. Read as lib/capture-cards.sh reads them, for the
//! setup wizard's card and USB stages.

mod embedded {
    include!(concat!(env!("OUT_DIR"), "/capture_cards.rs"));
}

/// What `capture_card_lookup` sets (CC_*).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Lookup {
    /// The profile's id (its file name), when the USB id is profiled.
    pub file: Option<String>,
    pub chip: String,
    pub hint: String,
    /// The fastest link the chip uses, in Mbit/s ("480", "5000").
    pub link: String,
    /// "level date note" for the id; empty without a Status line.
    pub status: String,
    /// The chip's quirks plus those tagged with this id, tags removed.
    pub quirks: Vec<String>,
}

/// How much is known about an id: `capture_card_lookup`'s 0, 1, 2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Known {
    Profiled,
    Vendor,
    Unknown,
}

/// `cc_field`: the value of the first `KEY: value` line, without a
/// carriage return.
fn field(text: &str, key: &str) -> String {
    let prefix = format!("{key}: ");
    text.lines().find_map(|l| l.strip_prefix(&prefix)).unwrap_or("").replace('\r', "")
}

/// The first line without its `# `.
fn title(text: &str) -> &str {
    let first = text.lines().next().unwrap_or("");
    first.strip_prefix("# ").unwrap_or("")
}

fn listed(field: &str, id: &str) -> bool {
    format!(" {field} ").contains(&format!(" {id} "))
}

/// `cc_statuses`' (id, level, date, note) for the first Status line of `id`,
/// as "level date note".
fn status(text: &str, id: &str) -> String {
    for line in text.lines().filter(|l| l.starts_with("Status: ")) {
        let mut f = line.split_whitespace().skip(1);
        let (sid, level, date) = (f.next().unwrap_or(""), f.next().unwrap_or(""), f.next().unwrap_or(""));
        if sid != id {
            continue;
        }
        // `sub(/^Status: +[^ ]+ +[^ ]+ +[^ ]+ */, "", note)`: the whole
        // line when there are fewer than three fields after "Status:".
        let note = strip_fields(line, 4).unwrap_or(line);
        return format!("{level} {date}{}", if note.is_empty() { String::new() } else { format!(" {note}") });
    }
    String::new()
}

/// `line` after `n` space-separated fields and the spaces after them, if
/// it has that many (awk's `+[^ ]+ +` runs on spaces only).
fn strip_fields(line: &str, n: usize) -> Option<&str> {
    let mut rest = line;
    for i in 0..n {
        if i > 0 {
            let t = rest.trim_start_matches(' ');
            if t.len() == rest.len() {
                return None;
            }
            rest = t;
        }
        let end = rest.find(' ').unwrap_or(rest.len());
        if end == 0 {
            return None;
        }
        rest = &rest[end..];
    }
    Some(rest.trim_start_matches(' '))
}

fn quirks(text: &str, id: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_q = false;
    for line in text.lines() {
        if line.starts_with("## ") {
            in_q = line == "## Quirks";
            continue;
        }
        let Some(q) = line.strip_prefix("- ").filter(|_| in_q) else { continue };
        // `[vid:pid] ` at the start ties a quirk to that id.
        if let Some(tagged) = q.strip_prefix('[')
            && let Some((tag, rest)) = tagged.split_once("] ")
            && !tag.contains(']')
        {
            if tag == id {
                out.push(rest.to_string());
            }
            continue;
        }
        out.push(q.to_string());
    }
    out
}

/// `capture_card_lookup` for a USB id (vid:pid).
pub fn lookup(id: &str) -> (Known, Lookup) {
    for (file, text) in embedded::FILES {
        if listed(&field(text, "Ids"), id) {
            return (
                Known::Profiled,
                Lookup {
                    file: Some(file.to_string()),
                    chip: title(text).to_string(),
                    hint: field(text, "Hint"),
                    link: field(text, "Link"),
                    status: status(text, id),
                    quirks: quirks(text, id),
                },
            );
        }
    }
    let vendor = id.split(':').next().unwrap_or("");
    for (_, text) in embedded::FILES {
        if listed(&field(text, "Vendors"), vendor) {
            let maker = title(text).split(' ').next().unwrap_or("");
            let chip = format!("{maker} family, exact chip unknown");
            return (Known::Vendor, Lookup { chip, hint: "go by the format list".into(), ..Default::default() });
        }
    }
    (Known::Unknown, Lookup { chip: "unknown chip".into(), hint: "go by the format list".into(), ..Default::default() })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// lib/capture-cards.sh's answer for `id`: the return code, then each
    /// CC_* on its own line, the quirks joined by "|".
    fn bash(id: &str) -> String {
        let out = std::process::Command::new("bash")
            .arg("-c")
            .arg(
                r#". "$1/lib/capture-cards.sh"; capture_card_lookup "$2"; rc=$?
                   f=""; [ -n "$CC_FILE" ] && f=$(basename "$CC_FILE" .md)
                   q=$(IFS='|'; echo "${CC_QUIRKS[*]}")
                   printf '%s\n' "$rc" "$f" "$CC_CHIP" "$CC_HINT" "$CC_LINK" "$CC_STATUS" "$q""#,
            )
            .arg("bash")
            .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/.."))
            .arg(id)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    fn rust(id: &str) -> String {
        let (known, l) = lookup(id);
        let rc = match known {
            Known::Profiled => 0,
            Known::Vendor => 1,
            Known::Unknown => 2,
        };
        format!("{rc}\n{}\n{}\n{}\n{}\n{}\n{}\n", l.file.unwrap_or_default(), l.chip, l.hint, l.link, l.status, l.quirks.join("|"))
    }

    #[test]
    #[cfg(target_os = "linux")] // runs bash, python or hyprctl
    fn lookups_as_lib_capture_cards_sh() {
        for id in ["1e4e:7016", "345f:2130", "534d:2109", "534d:9999", "dead:beef", ""] {
            assert_eq!(rust(id), bash(id), "{id}");
        }
        assert_eq!(lookup("534d:9999").1.chip, "MacroSilicon family, exact chip unknown");
    }

    #[test]
    fn status_notes_and_tags() {
        let t = "# X Y\nStatus: a:b tested 2026-01-02 PS5:  ok\nStatus: c:d reported\n## Quirks\n- all\n- [a:b] mine\n- [c:d] theirs\n## References\n- [a:b] no\n";
        assert_eq!(status(t, "a:b"), "tested 2026-01-02 PS5:  ok");
        assert_eq!(status(t, "c:d"), "reported  Status: c:d reported", "awk keeps the whole line without a date");
        assert_eq!(quirks(t, "a:b"), ["all", "mine"]);
    }
}
