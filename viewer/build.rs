//! Embeds the source profiles (`../sources/*.md`) and the capture card
//! profiles (`../capture-cards/*.md`), as `settings.rs` embeds
//! lib/settings.sh: the overlay's setup tips and the wizard's card facts
//! travel with the binary.

use std::fmt::Write as _;
use std::path::Path;
use std::{env, fs};

/// `<out>`: `pub const FILES: &[(&str, &str)]`, (id, contents) of every
/// `<dir>/*.md`, sorted by id.
fn embed(dir: &str, out: &str) {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(dir);
    println!("cargo:rerun-if-changed={}", dir.display());
    let mut files: Vec<_> = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|e| e.expect("a directory entry").path())
        .filter(|p| p.extension().is_some_and(|x| x == "md"))
        .collect();
    // By id, as the mpv script sorts them ("switch" before "switch-2").
    files.sort_by_key(|p| p.file_stem().map(|s| s.to_os_string()));
    let mut text = format!(
        "/// (id, file contents) of every {}/*.md, sorted by id.\npub const FILES: &[(&str, &str)] = &[\n",
        dir.file_name().expect("a directory name").to_string_lossy()
    );
    for f in files {
        println!("cargo:rerun-if-changed={}", f.display());
        let id = f.file_stem().expect("a file name").to_string_lossy().to_string();
        let path = fs::canonicalize(&f).expect("a readable profile");
        writeln!(text, "    ({id:?}, include_str!({:?})),", path.display().to_string()).unwrap();
    }
    text.push_str("];\n");
    fs::write(Path::new(&env::var("OUT_DIR").unwrap()).join(out), text).expect("OUT_DIR is writable");
}

fn main() {
    embed("../sources", "sources.rs");
    embed("../capture-cards", "capture_cards.rs");
}
