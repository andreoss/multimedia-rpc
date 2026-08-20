use mm_ffmpeg::{ScopeKind, build_frame_sample_args, build_scope_args};

const INPUT: &str = "https://store.invalid/input.mp4";
const OUTPUT: &str = "scope.jpg";

#[test]
fn every_scope_kind_renders_its_filter_and_the_requested_size() {
    for (kind, needle) in [
        (ScopeKind::Frame, "pad=w=160:h=120"),
        (
            ScopeKind::Waveform,
            "waveform=mode=column:components=1:display=overlay",
        ),
        (
            ScopeKind::Parade,
            "waveform=mode=column:components=7:display=parade",
        ),
        (ScopeKind::Histogram, "histogram=display_mode=overlay"),
        (ScopeKind::Vectorscope, "vectorscope=mode=color3"),
    ] {
        let args = build_scope_args(INPUT, kind, 160, 120, OUTPUT);
        let filter = args
            .iter()
            .position(|arg| arg == "-vf")
            .and_then(|index| args.get(index + 1))
            .expect("the scope filter is present");
        assert!(filter.contains(needle), "{kind:?}: {filter}");
        if kind != ScopeKind::Frame {
            assert!(filter.contains("scale=160:120"), "{kind:?}: {filter}");
        }
        assert_eq!(args.last().map(String::as_str), Some(OUTPUT));
        assert!(args.windows(2).any(|pair| pair == ["-frames:v", "1"]));
    }
}

#[test]
fn a_frame_sample_reads_one_frame_of_raw_rgb() {
    let args = build_frame_sample_args(INPUT, OUTPUT);
    assert_eq!(
        args,
        vec![
            "-y".to_string(),
            "-nostdin".to_string(),
            "-hide_banner".to_string(),
            "-i".to_string(),
            INPUT.to_string(),
            "-frames:v".to_string(),
            "1".to_string(),
            "-pix_fmt".to_string(),
            "rgb24".to_string(),
            "-f".to_string(),
            "rawvideo".to_string(),
            OUTPUT.to_string(),
        ]
    );
}
