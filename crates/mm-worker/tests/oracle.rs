use std::path::{Path, PathBuf};
use std::process::Command;

use mm_gst::{render_playlist, split_fragments, top_level_boxes};
use mm_testkit::assert_golden;

fn scratch() -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scratch/s57-worker");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn fragmented() -> (PathBuf, Vec<u8>) {
    let path = scratch().join("fragmented.mp4");
    let output = Command::new("ffmpeg")
        .args(["-y", "-f", "lavfi", "-i"])
        .arg("testsrc2=duration=4:size=320x180:rate=24")
        .args([
            "-c:v",
            "libx264",
            "-preset",
            "veryfast",
            "-g",
            "24",
            "-keyint_min",
            "24",
            "-sc_threshold",
            "0",
            "-movflags",
            "frag_keyframe+empty_moov+default_base_moof",
            "-f",
            "mp4",
        ])
        .arg(&path)
        .output()
        .expect("the media tool must be runnable");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let bytes = std::fs::read(&path).expect("the fragmented media reads");
    (path, bytes)
}

fn root_boxes(path: &Path) -> Vec<String> {
    let output = Command::new("ffprobe")
        .args(["-v", "trace"])
        .arg(path)
        .output()
        .expect("the media tool must be runnable");
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .filter(|line| line.contains("parent:'root'"))
        .filter_map(|line| {
            let start = line.find("type:'")? + 6;
            let rest = &line[start..];
            let end = rest.find('\'')?;
            Some(rest[..end].to_string())
        })
        .collect()
}

#[test]
fn the_box_walk_lists_the_root_boxes_the_tool_lists() {
    let (path, bytes) = fragmented();
    let walked: Vec<String> = top_level_boxes(&bytes)
        .expect("the fragmented stream walks")
        .into_iter()
        .map(|fragment| {
            String::from_utf8_lossy(&bytes[fragment.start + 4..fragment.start + 8]).to_string()
        })
        .collect();
    assert_eq!(walked, root_boxes(&path));
}

#[test]
fn the_rendered_playlist_duration_is_the_sum_the_renderer_wrote() {
    let (_, bytes) = fragmented();
    let segmented = split_fragments(&bytes).expect("the stream segments");
    let work = scratch().join("oracle-hls");
    let _ = std::fs::remove_dir_all(&work);
    let written = segmented.write_to(&work).expect("the set writes");
    let names: Vec<String> = written
        .segments
        .iter()
        .map(|path| path.file_name().unwrap().to_string_lossy().to_string())
        .collect();
    let playlist = render_playlist(Some("init.mp4"), 1.0, &names);
    let playlist_path = work.join("playlist.m3u8");
    std::fs::write(&playlist_path, &playlist).expect("the playlist writes");
    let golden = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/playlist.m3u8");
    assert_golden(&golden, &playlist);

    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=nw=1:nk=1",
        ])
        .arg(&playlist_path)
        .output()
        .expect("the media tool must be runnable");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let measured: f64 = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .expect("the tool reports a duration");
    let written_sum = names.len() as f64;
    assert!(
        (measured - written_sum).abs() < 0.001,
        "tool duration {measured} is not the written sum {written_sum}"
    );
}
