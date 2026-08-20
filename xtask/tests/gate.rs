use std::fs;
use std::path::{Path, PathBuf};

use xtask::{check_comments, check_layering};

fn temp_workspace(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("mm-xtask-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(path.join("crates")).unwrap();
    fs::write(
        path.join("Cargo.toml"),
        "[workspace]\nmembers = [\n    \"crates/mm-domain\",\n    \"crates/mm-ffmpeg\",\n    \"crates/mm-storage\",\n    \"crates/mm-jobstore\",\n    \"crates/mm-server\",\n    \"crates/mm-worker\",\n]\n",
    )
    .unwrap();
    path
}

fn manifest(root: &Path, name: &str, dependency: Option<&str>) {
    let directory = root.join("crates").join(name);
    fs::create_dir_all(&directory).unwrap();
    let dependency = dependency
        .map(|dependency| format!("{dependency} = {{ path = \"../{dependency}\" }}\n"))
        .unwrap_or_default();
    fs::write(
        directory.join("Cargo.toml"),
        format!("[package]\nname = \"{name}\"\n\n[dependencies]\n{dependency}"),
    )
    .unwrap();
}

#[test]
fn the_workspace_layers_hold() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    check_layering(root).expect("the workspace is layered");
}

#[test]
fn a_tool_crate_depending_on_the_workspace_is_refused_by_name() {
    let root = temp_workspace("tool");
    manifest(&root, "mm-domain", None);
    manifest(&root, "mm-ffmpeg", Some("mm-domain"));
    manifest(&root, "mm-storage", None);
    manifest(&root, "mm-jobstore", None);
    manifest(&root, "mm-server", None);
    manifest(&root, "mm-worker", None);
    let violations = check_layering(&root).expect_err("the tool dependency is refused");
    assert!(violations.iter().any(|violation| {
        violation.crate_name == "mm-ffmpeg"
            && violation.dependency == "mm-domain"
            && violation.rule.contains("tool crate")
    }));
}

#[test]
fn an_adapter_depending_on_an_adapter_is_refused_by_name() {
    let root = temp_workspace("adapter");
    manifest(&root, "mm-domain", None);
    manifest(&root, "mm-ffmpeg", None);
    manifest(&root, "mm-storage", Some("mm-jobstore"));
    manifest(&root, "mm-jobstore", None);
    manifest(&root, "mm-server", None);
    manifest(&root, "mm-worker", None);
    let violations = check_layering(&root).expect_err("the adapter dependency is refused");
    assert!(violations.iter().any(|violation| {
        violation.crate_name == "mm-storage" && violation.rule.contains("another adapter")
    }));
}

#[test]
fn a_server_depending_on_the_worker_is_refused_by_name() {
    let root = temp_workspace("server");
    manifest(&root, "mm-domain", None);
    manifest(&root, "mm-ffmpeg", None);
    manifest(&root, "mm-storage", None);
    manifest(&root, "mm-jobstore", None);
    manifest(&root, "mm-server", Some("mm-worker"));
    manifest(&root, "mm-worker", None);
    let violations = check_layering(&root).expect_err("the worker dependency is refused");
    assert!(violations.iter().any(|violation| {
        violation.crate_name == "mm-server" && violation.rule.contains("server")
    }));
}

#[test]
fn a_comment_in_source_is_refused_by_name() {
    let root = temp_workspace("comments");
    manifest(&root, "mm-domain", None);
    manifest(&root, "mm-ffmpeg", None);
    manifest(&root, "mm-storage", None);
    manifest(&root, "mm-jobstore", None);
    manifest(&root, "mm-server", None);
    manifest(&root, "mm-worker", None);
    let source = root.join("crates/mm-domain/src");
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("lib.rs"), "pub fn ok() {}\n// a comment\n").unwrap();
    let violations = check_comments(&root).expect_err("the comment is refused");
    assert!(violations[0].contains("a comment"));
    fs::write(source.join("lib.rs"), "pub fn ok() {}\n").unwrap();
    check_comments(&root).expect("a comment-free source passes");
}

#[test]
fn a_domain_crate_depending_on_the_workspace_is_refused_by_name() {
    let root = temp_workspace("domain");
    manifest(&root, "mm-domain", Some("mm-ffmpeg"));
    manifest(&root, "mm-ffmpeg", None);
    manifest(&root, "mm-storage", None);
    manifest(&root, "mm-jobstore", None);
    manifest(&root, "mm-server", None);
    manifest(&root, "mm-worker", None);
    let violations = check_layering(&root).expect_err("the domain dependency is refused");
    assert!(violations.iter().any(|violation| {
        violation.crate_name == "mm-domain" && violation.rule.contains("domain crate")
    }));
}
