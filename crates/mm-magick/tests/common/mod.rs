#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

pub fn scratch_dir() -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scratch/s14");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

pub fn solid(name: &str, width: u32, height: u32, colour: &str) -> PathBuf {
    static GENERATION: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = GENERATION.lock().unwrap();
    let dir = scratch_dir().join("fixtures");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    if path.is_file() && path.metadata().map(|meta| meta.len()).unwrap_or(0) > 0 {
        return path;
    }
    let output = Command::new("magick")
        .arg("-size")
        .arg(format!("{width}x{height}"))
        .arg(format!("xc:{colour}"))
        .arg(&path)
        .output()
        .expect("the image tool must be runnable to generate fixtures");
    assert!(output.status.success(), "fixture generation failed: {name}");
    path
}
