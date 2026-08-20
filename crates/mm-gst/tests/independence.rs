use std::fs;
use std::path::{Path, PathBuf};

const FORBIDDEN_CRATES: [&str; 9] = [
    "mm_domain",
    "mm_proto",
    "mm_storage",
    "mm_jobstore",
    "mm_server",
    "mm_worker",
    "mm_ffmpeg",
    "mm_magick",
    "mmrpcd",
];

const TOOLKIT_TOKENS: [&str; 2] = ["gstreamer::", "use gstreamer"];

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
fn the_toolkit_is_confined_to_the_pipeline_module() {
    let mut touched = Vec::new();
    for (path, text) in rust_sources(&crate_dir().join("src")) {
        if TOOLKIT_TOKENS.iter().any(|token| text.contains(token)) {
            touched.push(path);
        }
    }
    touched.sort();
    assert_eq!(
        touched.len(),
        1,
        "the toolkit must live in exactly one file, found {touched:?}"
    );
    assert!(
        touched[0].ends_with("src/pipeline.rs"),
        "the toolkit is used in {touched:?}"
    );
}

#[test]
fn the_toolkit_is_only_reachable_behind_its_feature() {
    let lib = fs::read_to_string(crate_dir().join("src/lib.rs")).unwrap();
    assert!(lib.contains("#[cfg(feature = \"gst\")]"));
    let manifest = fs::read_to_string(crate_dir().join("Cargo.toml")).unwrap();
    assert!(manifest.contains("gst = ["));
    assert!(manifest.contains("optional = true"));
}
