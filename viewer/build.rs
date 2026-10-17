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

/// The app icon as the Windows exe's icon resource 1 (#35): Explorer,
/// Start and the taskbar show it, and app.rs gives it to the window. A
/// missing resource compiler (llvm-rc, or rc.exe on Windows) is a warning,
/// so a plain `cargo check` for Windows still runs; a failed compile stops
/// the build.
fn windows_icon() {
    // Not canonicalize: on Windows that gives a \\?\ path, which rc may not take.
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("viewer/ is in the repo");
    let ico = repo.join("packaging/windows/protohdmicapture.ico");
    println!("cargo:rerun-if-changed={}", ico.display());
    fs::metadata(&ico).unwrap_or_else(|e| panic!("{}: {e}", ico.display()));
    // Forward slashes: rc reads a backslash in a quoted name as an escape.
    let rc = Path::new(&env::var("OUT_DIR").unwrap()).join("icon.rc");
    fs::write(&rc, format!("1 ICON \"{}\"\n", ico.display().to_string().replace('\\', "/"))).expect("OUT_DIR is writable");
    match embed_resource::compile(&rc, embed_resource::NONE) {
        embed_resource::CompilationResult::NotAttempted(why) => println!("cargo:warning=no icon in the exe: {why}"),
        r => r.manifest_optional().unwrap_or_else(|e| panic!("the exe's icon: {e}")),
    }
}

fn main() {
    embed("../sources", "sources.rs");
    embed("../capture-cards", "capture_cards.rs");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        windows_icon();
    }
}
