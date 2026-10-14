//! Embeds the source profiles (`../sources/*.md`), as `settings.rs` embeds
//! lib/settings.sh: the overlay's setup tips travel with the binary.

use std::fmt::Write as _;
use std::path::Path;
use std::{env, fs};

fn main() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../sources");
    println!("cargo:rerun-if-changed={}", dir.display());
    let mut files: Vec<_> = fs::read_dir(&dir)
        .expect("the sources directory")
        .map(|e| e.expect("a sources entry").path())
        .filter(|p| p.extension().is_some_and(|x| x == "md"))
        .collect();
    // By id, as the mpv script sorts them ("switch" before "switch-2").
    files.sort_by_key(|p| p.file_stem().map(|s| s.to_os_string()));
    let mut out = String::from("/// (id, file contents) of every sources/*.md, sorted by id.\npub const FILES: &[(&str, &str)] = &[\n");
    for f in files {
        println!("cargo:rerun-if-changed={}", f.display());
        let id = f.file_stem().expect("a file name").to_string_lossy().to_string();
        let path = fs::canonicalize(&f).expect("a readable profile");
        writeln!(out, "    ({id:?}, include_str!({:?})),", path.display().to_string()).unwrap();
    }
    out.push_str("];\n");
    fs::write(Path::new(&env::var("OUT_DIR").unwrap()).join("sources.rs"), out).expect("OUT_DIR is writable");
}
