#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

pub fn scratch_dir() -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scratch/s07");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

pub fn fixture(name: &str, seconds: u32, fps: u32, width: u32, height: u32) -> PathBuf {
    static GENERATION: Mutex<()> = Mutex::new(());
    let _guard = GENERATION.lock().unwrap();
    let dir = scratch_dir().join("fixtures");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    if path.is_file() && path.metadata().unwrap().len() > 0 {
        return path;
    }
    let output = Command::new("ffmpeg")
        .arg("-y")
        .arg("-f")
        .arg("lavfi")
        .arg("-i")
        .arg(format!(
            "testsrc2=duration={seconds}:size={width}x{height}:rate={fps}"
        ))
        .arg("-f")
        .arg("lavfi")
        .arg("-i")
        .arg(format!(
            "sine=frequency=440:sample_rate=44100:duration={seconds}"
        ))
        .arg("-c:v")
        .arg("libx264")
        .arg("-preset")
        .arg("veryfast")
        .arg("-pix_fmt")
        .arg("yuv420p")
        .arg("-c:a")
        .arg("aac")
        .arg("-b:a")
        .arg("128k")
        .arg("-movflags")
        .arg("+faststart")
        .arg("-f")
        .arg("mp4")
        .arg(&path)
        .output()
        .expect("the media tool must be runnable to generate fixtures");
    assert!(output.status.success(), "fixture generation failed: {name}");
    path
}

pub fn moov_precedes_mdat(path: &Path) -> bool {
    let bytes = std::fs::read(path).unwrap();
    let mut offset = 0usize;
    let mut moov_at = None;
    let mut mdat_at = None;
    while offset + 8 <= bytes.len() {
        let declared = u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap()) as u64;
        let kind = &bytes[offset + 4..offset + 8];
        let size = match declared {
            0 => bytes.len() as u64 - offset as u64,
            1 => {
                if offset + 16 > bytes.len() {
                    break;
                }
                u64::from_be_bytes(bytes[offset + 8..offset + 16].try_into().unwrap())
            }
            other => other,
        };
        if kind == b"moov" && moov_at.is_none() {
            moov_at = Some(offset);
        }
        if kind == b"mdat" && mdat_at.is_none() {
            mdat_at = Some(offset);
        }
        if size < 8 {
            break;
        }
        offset += size as usize;
    }
    matches!((moov_at, mdat_at), (Some(moov), Some(mdat)) if moov < mdat)
}
