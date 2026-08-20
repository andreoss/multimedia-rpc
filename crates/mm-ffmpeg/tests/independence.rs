use std::fs;
use std::path::{Path, PathBuf};

const FORBIDDEN_CRATES: [&str; 7] = [
    "mm_domain",
    "mm_proto",
    "mm_storage",
    "mm_jobstore",
    "mm_server",
    "mm_worker",
    "mmrpcd",
];

const SPAWN_TOKENS: [&str; 3] = ["process::Command", "Command::new", ".spawn("];

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn rust_sources(dir: &Path) -> Vec<(PathBuf, String)> {
    let mut sources = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else if path.ends_with("tests/independence.rs") {
                continue;
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                let text = fs::read_to_string(&path).unwrap();
                sources.push((path, text));
            }
        }
    }
    assert!(!sources.is_empty());
    sources
}

#[test]
fn no_workspace_crate_is_imported() {
    for (path, text) in rust_sources(&crate_dir()) {
        for forbidden in FORBIDDEN_CRATES {
            assert!(
                !text.contains(forbidden),
                "{} references {forbidden}",
                path.display()
            );
        }
    }
}

#[test]
fn the_manifest_declares_no_workspace_crate_dependency() {
    let manifest = fs::read_to_string(crate_dir().join("Cargo.toml")).unwrap();
    for line in manifest.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("name") {
            continue;
        }
        assert!(
            !trimmed.starts_with("mm-") && !trimmed.contains("mm-domain"),
            "manifest line references a workspace crate: {line}"
        );
    }
}

#[test]
fn process_spawning_is_confined_to_the_probe_and_the_runner() {
    let mut spawning_files = Vec::new();
    for (path, text) in rust_sources(&crate_dir().join("src")) {
        if SPAWN_TOKENS.iter().any(|token| text.contains(token)) {
            spawning_files.push(path);
        }
    }
    spawning_files.sort();
    assert_eq!(
        spawning_files.len(),
        2,
        "process spawning must live in exactly two files, found {spawning_files:?}"
    );
    assert!(
        spawning_files[0].ends_with("src/probe.rs"),
        "spawning lives in {spawning_files:?}"
    );
    assert!(
        spawning_files[1].ends_with("src/runner.rs"),
        "spawning lives in {spawning_files:?}"
    );
}
