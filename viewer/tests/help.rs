//! `-h` and `--help` (#26): the usage on stdout, exit 0, before anything
//! is opened.

use std::process::Command;

#[test]
fn help_prints_the_usage_and_exits_0() {
    for flag in ["-h", "--help"] {
        let out = Command::new(env!("CARGO_BIN_EXE_protohdmicapture")).arg(flag).output().unwrap();
        assert!(out.status.success(), "{flag}: {:?}", out.status);
        let text = String::from_utf8(out.stdout).unwrap();
        assert!(text.starts_with("usage: protohdmicapture "), "{flag}: {text}");
        for f in ["--setup", "--debug", "--menu", "--test-pattern"] {
            assert!(text.contains(f), "{flag}: no {f} in\n{text}");
        }
    }
}
