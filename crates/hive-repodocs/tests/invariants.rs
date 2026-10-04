//! The gate's assertions about CLAUDE.md. Ported from
//! internal/repodocs/invariants_test.go.

use hive_repodocs::{MIN_INVARIANTS, REQUIRED_PHRASES, invariant_numbers};

const PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../CLAUDE.md");

fn what_to_do() -> String {
    format!(
        "If you are ADDING an invariant, raise MIN_INVARIANTS (currently {MIN_INVARIANTS}) in the same commit.\n\
         If you are not, your branch predates one and the merge dropped it: rebase on origin/main\n\
         and take the version of CLAUDE.md with more invariants, not fewer."
    )
}

/// Fails if CLAUDE.md loses an invariant or the list stops being contiguous.
/// A gap means a merge took one out of the middle, which is harder to spot by
/// eye than a missing tail.
#[test]
fn invariants_are_intact() {
    let body = std::fs::read_to_string(PATH).expect("read CLAUDE.md");
    let numbers = invariant_numbers(&body);
    assert!(
        numbers.len() >= MIN_INVARIANTS,
        "CLAUDE.md has {} invariants, expected at least {MIN_INVARIANTS}.\n{}",
        numbers.len(),
        what_to_do()
    );
    for (i, got) in numbers.iter().enumerate() {
        assert_eq!(
            *got,
            i + 1,
            "invariant list is not contiguous at position {}.\n{}",
            i + 1,
            what_to_do()
        );
    }
}

/// Fails when a merge drops guidance the numbered list does not cover.
#[test]
fn required_guidance_survives() {
    let body = std::fs::read_to_string(PATH).expect("read CLAUDE.md");
    for phrase in REQUIRED_PHRASES {
        assert!(
            body.contains(phrase),
            "CLAUDE.md no longer contains {phrase:?}.\n\
             If you deliberately reworded it, update REQUIRED_PHRASES in the same commit.\n\
             If you did not, your branch predates it and the merge dropped it: rebase on\n\
             origin/main and keep the version with more guidance, not less."
        );
    }
}

/// `unsafe_code` is `deny` at the workspace rather than `forbid` so that
/// hive-db can carry the one block the engine needs (sqlite-vec's
/// registration, D41 §3). This is what keeps that from spreading: no other
/// crate contains an `unsafe` block, function, impl or extern, or lifts the
/// lint. Adding one means editing this test and saying why.
#[test]
fn unsafe_is_confined_to_hive_db() {
    let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut offenders = Vec::new();
    for entry in std::fs::read_dir(&crates).expect("crates/") {
        let dir = entry.expect("entry").path();
        let name = dir.file_name().unwrap().to_string_lossy().into_owned();
        // hive-db is the named exception; hive-repodocs is this file, whose
        // string literals spell the very tokens it looks for.
        if !dir.is_dir() || name == "hive-db" || name == "hive-repodocs" {
            continue;
        }
        scan(&dir.join("src"), &mut offenders);
        scan(&dir.join("tests"), &mut offenders);
    }
    assert!(
        offenders.is_empty(),
        "unsafe outside hive-db:\n{}",
        offenders.join("\n")
    );
}

fn scan(dir: &std::path::Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries {
        let path = entry.expect("entry").path();
        if path.is_dir() {
            scan(&path, out);
            continue;
        }
        if path.extension().is_none_or(|e| e != "rs") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("read source");
        for (n, line) in text.lines().enumerate() {
            let code = line.split("//").next().unwrap_or("");
            let is_unsafe = code.contains("allow(unsafe_code)")
                || code
                    .split_whitespace()
                    .zip(code.split_whitespace().skip(1))
                    .any(|(a, b)| {
                        a == "unsafe"
                            && (b.starts_with('{') || b == "fn" || b == "impl" || b == "extern")
                    })
                || code.contains("unsafe{");
            if is_unsafe {
                out.push(format!("{}:{}: {}", path.display(), n + 1, line.trim()));
            }
        }
    }
}

/// Fails when hive-sandbox gains a role that is on by default and the egress
/// image does not switch it off. The image names its roles one by one, so a new
/// default-on role starts inside the proxy too; D42's `--run-models` did, asked
/// for a store the proxy does not have, and the proxy died before listening
/// (#115 is what made that readable).
#[test]
fn the_egress_image_switches_off_every_default_role() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
    let main = std::fs::read_to_string(format!("{root}/crates/hive-sandbox/src/main.rs"))
        .expect("read hive-sandbox main.rs");
    let image = std::fs::read_to_string(format!("{root}/docker/egress/Containerfile"))
        .expect("read docker/egress/Containerfile");
    let mut roles = Vec::new();
    let mut default_on = false;
    for line in main.lines().map(str::trim) {
        if line.starts_with("#[arg(") {
            default_on = line.contains("default_value_t = true");
            continue;
        }
        if line.starts_with("///") || line.is_empty() {
            continue;
        }
        if default_on
            && let Some(name) = line.strip_suffix(": bool,")
            && (name == "serve_api" || name.starts_with("run_"))
        {
            roles.push(name.replace('_', "-"));
        }
        default_on = false;
    }
    assert!(
        roles.len() >= 4,
        "found only {roles:?}; the scan of main.rs has stopped matching"
    );
    for role in &roles {
        assert!(
            image.contains(&format!("\"--{role}=false\"")),
            "docker/egress/Containerfile does not pass --{role}=false; the proxy would start that role too"
        );
    }
}
