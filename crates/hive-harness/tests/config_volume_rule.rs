//! D35's rule, made grep-able: the daemon never opens a file under a person's
//! config volume. This crate is where the volume's host path is known, and the
//! only things allowed to know it are the spec that carries it and the
//! launcher that mounts it. Any other mention is a reader, and a reader is a
//! bug.
//!
//! This is a rule and not a boundary ... the daemon and the harness share a
//! uid under `--userns keep-id`, so the daemon *could* read the directory.
//! D35 says so. Until the daemon runs as a different user, this test is the
//! enforcement, and it fails the moment a third file learns the path.

use std::fs;
use std::path::Path;

#[test]
fn only_the_spec_and_the_launcher_know_the_config_dir() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let allowed = ["spec.rs", "podman.rs"];
    let mut offenders = Vec::new();
    let mut seen_in_allowed = 0;
    for entry in fs::read_dir(&src).expect("src dir") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let name = path.file_name().unwrap().to_str().unwrap().to_string();
        let text = fs::read_to_string(&path).expect("read source");
        let hits = text.lines().filter(|l| l.contains("config_dir")).count();
        if hits == 0 {
            continue;
        }
        if allowed.contains(&name.as_str()) {
            seen_in_allowed += hits;
        } else {
            offenders.push((name, hits));
        }
    }
    // The rule cannot pass vacuously: if the field were renamed, this would
    // otherwise go green while checking nothing.
    assert!(
        seen_in_allowed > 0,
        "config_dir is not mentioned in spec.rs or podman.rs; the field was renamed and this test is checking nothing"
    );
    assert!(
        offenders.is_empty(),
        "config_dir is read outside the spec and the launcher: {offenders:?}. D35: the daemon mounts the person's config volume and never opens it"
    );
}
