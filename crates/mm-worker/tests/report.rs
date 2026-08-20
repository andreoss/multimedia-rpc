use std::time::Duration;

use mm_domain::{BlockCode, OutputProfile, Verdict, VerdictCode, WarnCode};
use mm_ffmpeg::{AudioStream, MediaInfo, SubtitleStream, VideoStream};
use mm_worker::{blocks, input_report, render_verdicts, target_profile, warnings};

fn media_info() -> MediaInfo {
    MediaInfo {
        container: "mov,mp4,m4a,3gp,3g2,mj2".to_string(),
        duration: Duration::from_secs_f64(12.5),
        size_bytes: Some(4_000_000),
        bit_rate: Some(2_560_000),
        video: vec![VideoStream {
            codec: "h264".to_string(),
            width: 1280,
            height: 720,
            avg_fps: Some(30.0),
            real_fps: Some(30.0),
            rotation: 90,
            pixel_format: Some("yuv420p".to_string()),
            sample_aspect_ratio: Some((1, 1)),
            interlaced: false,
        }],
        audio: vec![AudioStream {
            codec: "aac".to_string(),
            channels: 2,
            sample_rate: Some(48_000),
            bit_rate: Some(128_000),
        }],
        subtitle: Vec::<SubtitleStream>::new(),
        drm: false,
    }
}

#[test]
fn a_media_info_maps_to_an_input_report() {
    let report = input_report(&media_info());
    assert_eq!(report.container, "mov,mp4,m4a,3gp,3g2,mj2");
    assert_eq!(report.duration, Duration::from_secs_f64(12.5));
    assert_eq!(report.size_bytes, Some(4_000_000));
    assert!(!report.drm);
    assert_eq!(report.video.len(), 1);
    assert_eq!(report.video[0].codec, "h264");
    assert_eq!(report.video[0].width, 1280);
    assert_eq!(report.video[0].height, 720);
    assert_eq!(report.video[0].avg_fps, Some(30.0));
    assert_eq!(report.video[0].real_fps, Some(30.0));
    assert_eq!(report.video[0].rotation, 90);
    assert_eq!(report.video[0].sample_aspect_ratio, Some((1, 1)));
    assert!(!report.video[0].interlaced);
    assert_eq!(report.audio.len(), 1);
    assert_eq!(report.audio[0].codec, "aac");
    assert_eq!(report.audio[0].channels, 2);
    assert_eq!(report.audio[0].sample_rate, Some(48_000));
}

#[test]
fn an_absent_output_profile_yields_the_default_target() {
    let target = target_profile(None);
    assert_eq!(target, mm_ffmpeg::TargetProfile::default());
}

#[test]
fn output_profile_fields_override_the_target_defaults() {
    let profile =
        OutputProfile::from_raw(30.0, 4000, 20, 192, 44_100, 640, 360).expect("valid profile");
    let target = target_profile(Some(&profile));
    assert_eq!(target.fps, 30);
    assert_eq!(target.video_max_bitrate_kbps, 4000);
    assert_eq!(target.video_crf, 20);
    assert_eq!(target.audio_bitrate_kbps, 192);
    assert_eq!(target.audio_sample_rate_hz, 44_100);
    assert_eq!(target.resolution, Some((640, 360)));
}

#[test]
fn unset_profile_fields_keep_the_target_defaults() {
    let profile = OutputProfile::from_raw(60.0, 0, 0, 0, 0, 0, 0).expect("valid profile");
    let target = target_profile(Some(&profile));
    let default = mm_ffmpeg::TargetProfile::default();
    assert_eq!(target.fps, 60);
    assert_eq!(target.resolution, default.resolution);
    assert_eq!(
        target.video_max_bitrate_kbps,
        default.video_max_bitrate_kbps
    );
    assert_eq!(target.video_crf, default.video_crf);
    assert_eq!(target.audio_bitrate_kbps, default.audio_bitrate_kbps);
    assert_eq!(target.audio_sample_rate_hz, default.audio_sample_rate_hz);
}

fn mixed_verdicts() -> Vec<Verdict> {
    vec![
        Verdict {
            rung: None,
            input: Some(0),
            code: VerdictCode::Block(BlockCode::UnsupportedContainer),
        },
        Verdict {
            rung: None,
            input: None,
            code: VerdictCode::Warn(WarnCode::FpsDrift),
        },
        Verdict {
            rung: None,
            input: Some(1),
            code: VerdictCode::Block(BlockCode::DrmProtected),
        },
    ]
}

#[test]
fn blocks_and_warnings_split_the_verdict_list() {
    let verdicts = mixed_verdicts();
    let blocked = blocks(&verdicts);
    assert_eq!(blocked.len(), 2);
    assert!(
        blocked
            .iter()
            .all(|verdict| matches!(verdict.code, VerdictCode::Block(_)))
    );
    let warned = warnings(&verdicts);
    assert_eq!(warned.len(), 1);
    assert_eq!(warned[0].code, VerdictCode::Warn(WarnCode::FpsDrift));
}

#[test]
fn rendered_verdicts_name_the_input_and_the_code() {
    let text = render_verdicts(&mixed_verdicts());
    assert!(text.contains("input 0: UnsupportedContainer"), "{text}");
    assert!(text.contains("inputs: FpsDrift"), "{text}");
    assert!(text.contains("input 1: DrmProtected"), "{text}");
}
