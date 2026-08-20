use mm_ffmpeg::{AudioError, AudioTarget, build_audio_args};

const INPUT_A: &str = "http://127.0.0.1:9/a.mp4";
const INPUT_B: &str = "http://127.0.0.1:9/b.mp4";
const OUTPUT: &str = "scratch/out.wav";

fn target() -> AudioTarget {
    AudioTarget {
        gain_db: 6.0,
        fade_in_micros: 1_500_000,
        fade_out_micros: 500_000,
        duration_micros: 6_000_000,
        high_pass_hz: Some(80),
        low_pass_hz: Some(12_000),
        limiter_true_peak_db: Some(-1.0),
    }
}

fn argument_after<'a>(args: &'a [String], flag: &str) -> &'a str {
    let position = args.iter().position(|arg| arg == flag).unwrap();
    &args[position + 1]
}

#[test]
fn an_audio_graph_applies_gain_fades_filters_and_a_limiter() {
    let args =
        build_audio_args(&[INPUT_A.to_string()], &target(), OUTPUT).expect("the argv builds");
    let graph = argument_after(&args, "-filter_complex");
    assert!(graph.contains("volume=6dB"), "{graph}");
    assert!(graph.contains("highpass=f=80"), "{graph}");
    assert!(graph.contains("lowpass=f=12000"), "{graph}");
    assert!(graph.contains("afade=t=in:st=0:d=1.500000"), "{graph}");
    assert!(
        graph.contains("afade=t=out:st=5.500000:d=0.500000"),
        "{graph}"
    );
    assert!(
        graph.contains("alimiter=limit=0.891251:level=false"),
        "{graph}"
    );
    assert_eq!(argument_after(&args, "-map"), "[aout]");
    assert_eq!(args.last().map(String::as_str), Some(OUTPUT));
}

#[test]
fn two_audio_inputs_are_mixed_for_the_longer_duration() {
    let args = build_audio_args(
        &[INPUT_A.to_string(), INPUT_B.to_string()],
        &target(),
        OUTPUT,
    )
    .expect("the argv builds");
    let graph = argument_after(&args, "-filter_complex");
    assert!(graph.contains("amix=inputs=2:duration=longest"), "{graph}");
    assert_eq!(args.iter().filter(|arg| *arg == "-i").count(), 2);
}

#[test]
fn a_fade_pair_longer_than_the_duration_is_refused() {
    let target = AudioTarget {
        fade_in_micros: 4_000_000,
        fade_out_micros: 3_000_000,
        duration_micros: 6_000_000,
        ..target()
    };
    assert_eq!(
        build_audio_args(&[INPUT_A.to_string()], &target, OUTPUT),
        Err(AudioError::FadeExceedsDuration {
            fade_in_micros: 4_000_000,
            fade_out_micros: 3_000_000,
            duration_micros: 6_000_000,
        })
    );
}
