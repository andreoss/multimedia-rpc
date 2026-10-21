use std::time::Duration;

use mm_ffmpeg::{
    MediaKind, ParseError, media_kind, parse_keyframe_times, parse_media_info, parse_still_info,
    parse_subtitle_streams,
};

fn mp4_probe_json() -> String {
    r#"{
        "streams": [
            {
                "index": 0,
                "codec_name": "h264",
                "codec_type": "video",
                "codec_tag_string": "avc1",
                "width": 1280,
                "height": 720,
                "sample_aspect_ratio": "1:1",
                "pix_fmt": "yuv420p",
                "field_order": "progressive",
                "r_frame_rate": "30/1",
                "avg_frame_rate": "30/1"
            },
            {
                "index": 1,
                "codec_name": "aac",
                "codec_type": "audio",
                "codec_tag_string": "mp4a",
                "sample_rate": "44100",
                "channels": 2,
                "bit_rate": "128000"
            }
        ],
        "format": {
            "format_name": "mov,mp4,m4a,3gp,3g2,mj2",
            "duration": "20.023000",
            "size": "5242880",
            "bit_rate": "2100000"
        }
    }"#
    .to_string()
}

#[test]
fn full_mp4_report_parses() {
    let info = parse_media_info(&mp4_probe_json()).unwrap();
    assert_eq!(info.container, "mov,mp4,m4a,3gp,3g2,mj2");
    assert!((info.duration.as_secs_f64() - 20.023).abs() < 1e-6);
    assert_eq!(info.size_bytes, Some(5_242_880));
    assert_eq!(info.bit_rate, Some(2_100_000));
    assert!(!info.drm);

    assert_eq!(info.video.len(), 1);
    let video = &info.video[0];
    assert_eq!(video.codec, "h264");
    assert_eq!(video.width, 1280);
    assert_eq!(video.height, 720);
    assert_eq!(video.avg_fps, Some(30.0));
    assert_eq!(video.real_fps, Some(30.0));
    assert_eq!(video.rotation, 0);
    assert_eq!(video.pixel_format.as_deref(), Some("yuv420p"));
    assert_eq!(video.sample_aspect_ratio, Some((1, 1)));
    assert!(!video.interlaced);

    assert_eq!(info.audio.len(), 1);
    let audio = &info.audio[0];
    assert_eq!(audio.codec, "aac");
    assert_eq!(audio.channels, 2);
    assert_eq!(audio.sample_rate, Some(44_100));
    assert_eq!(audio.bit_rate, Some(128_000));
}

#[test]
fn truncated_duration_is_corrected_from_size_and_bit_rate() {
    let json = r#"{
        "streams": [
            {"codec_type": "video", "codec_name": "h264", "width": 640, "height": 360,
             "avg_frame_rate": "30/1", "r_frame_rate": "30/1"}
        ],
        "format": {
            "format_name": "mov,mp4,m4a,3gp,3g2,mj2",
            "duration": "1.500000",
            "size": "2500000",
            "bit_rate": "1000000"
        }
    }"#;
    let info = parse_media_info(json).unwrap();
    assert!((info.duration.as_secs_f64() - 20.0).abs() < 1e-6);
}

#[test]
fn format_duration_wins_when_larger_than_the_estimate() {
    let json = r#"{
        "streams": [],
        "format": {
            "format_name": "matroska,webm",
            "duration": "30.000000",
            "size": "1000",
            "bit_rate": "1000000"
        }
    }"#;
    let info = parse_media_info(json).unwrap();
    assert!((info.duration.as_secs_f64() - 30.0).abs() < 1e-6);
}

#[test]
fn missing_duration_falls_back_to_the_estimate() {
    let json = r#"{
        "streams": [],
        "format": {
            "format_name": "mov,mp4,m4a,3gp,3g2,mj2",
            "size": "1250000",
            "bit_rate": "1000000"
        }
    }"#;
    let info = parse_media_info(json).unwrap();
    assert_eq!(info.duration, Duration::from_secs(10));
}

#[test]
fn absent_duration_sources_are_an_error() {
    let json = r#"{
        "streams": [],
        "format": {"format_name": "mov,mp4,m4a,3gp,3g2,mj2"}
    }"#;
    assert!(matches!(
        parse_media_info(json).unwrap_err(),
        ParseError::MissingDuration
    ));
}

#[test]
fn missing_format_section_is_an_error() {
    assert!(matches!(
        parse_media_info(r#"{"streams": []}"#).unwrap_err(),
        ParseError::MissingFormat
    ));
}

#[test]
fn malformed_json_is_an_error() {
    assert!(matches!(
        parse_media_info("not json").unwrap_err(),
        ParseError::Json { .. }
    ));
}

#[test]
fn vfr_streams_expose_differing_rates() {
    let json = r#"{
        "streams": [
            {"codec_type": "video", "codec_name": "h264", "width": 640, "height": 360,
             "avg_frame_rate": "2997/100", "r_frame_rate": "60/1"}
        ],
        "format": {"format_name": "mov,mp4,m4a,3gp,3g2,mj2", "duration": "5.0"}
    }"#;
    let info = parse_media_info(json).unwrap();
    let video = &info.video[0];
    assert!((video.avg_fps.unwrap() - 29.97).abs() < 1e-6);
    assert_eq!(video.real_fps, Some(60.0));
}

#[test]
fn zero_denominator_rates_are_absent() {
    let json = r#"{
        "streams": [
            {"codec_type": "video", "codec_name": "h264", "width": 640, "height": 360,
             "avg_frame_rate": "0/0", "r_frame_rate": "0/0"}
        ],
        "format": {"format_name": "mov,mp4,m4a,3gp,3g2,mj2", "duration": "5.0"}
    }"#;
    let info = parse_media_info(json).unwrap();
    assert_eq!(info.video[0].avg_fps, None);
    assert_eq!(info.video[0].real_fps, None);
}

#[test]
fn rotation_comes_from_display_matrix_side_data() {
    let json = r#"{
        "streams": [
            {"codec_type": "video", "codec_name": "h264", "width": 640, "height": 360,
             "avg_frame_rate": "30/1", "r_frame_rate": "30/1",
             "side_data_list": [{"side_data_type": "Display Matrix", "rotation": -90}]}
        ],
        "format": {"format_name": "mov,mp4,m4a,3gp,3g2,mj2", "duration": "5.0"}
    }"#;
    let info = parse_media_info(json).unwrap();
    assert_eq!(info.video[0].rotation, -90);
}

#[test]
fn rotation_falls_back_to_the_rotate_tag() {
    let json = r#"{
        "streams": [
            {"codec_type": "video", "codec_name": "h264", "width": 640, "height": 360,
             "avg_frame_rate": "30/1", "r_frame_rate": "30/1",
             "tags": {"rotate": "90"}}
        ],
        "format": {"format_name": "mov,mp4,m4a,3gp,3g2,mj2", "duration": "5.0"}
    }"#;
    let info = parse_media_info(json).unwrap();
    assert_eq!(info.video[0].rotation, 90);
}

#[test]
fn interlaced_field_orders_are_detected() {
    for (order, expected) in [
        ("tt", true),
        ("bb", true),
        ("tb", true),
        ("bt", true),
        ("progressive", false),
    ] {
        let json = format!(
            r#"{{
                "streams": [
                    {{"codec_type": "video", "codec_name": "h264", "width": 640,
                      "height": 360, "avg_frame_rate": "30/1", "r_frame_rate": "30/1",
                      "field_order": "{order}"}}
                ],
                "format": {{"format_name": "mov,mp4,m4a,3gp,3g2,mj2", "duration": "5.0"}}
            }}"#
        );
        let info = parse_media_info(&json).unwrap();
        assert_eq!(info.video[0].interlaced, expected, "field_order {order}");
    }
}

#[test]
fn encrypted_codec_tags_set_the_drm_flag() {
    for tag in ["encv", "enca"] {
        let json = format!(
            r#"{{
                "streams": [
                    {{"codec_type": "video", "codec_name": "h264", "width": 640,
                      "height": 360, "avg_frame_rate": "30/1", "r_frame_rate": "30/1",
                      "codec_tag_string": "{tag}"}}
                ],
                "format": {{"format_name": "mov,mp4,m4a,3gp,3g2,mj2", "duration": "5.0"}}
            }}"#
        );
        let info = parse_media_info(&json).unwrap();
        assert!(info.drm, "codec tag {tag}");
    }
}

#[test]
fn anamorphic_sample_aspect_ratio_parses() {
    let json = r#"{
        "streams": [
            {"codec_type": "video", "codec_name": "h264", "width": 720, "height": 576,
             "avg_frame_rate": "25/1", "r_frame_rate": "25/1",
             "sample_aspect_ratio": "16:11"}
        ],
        "format": {"format_name": "mov,mp4,m4a,3gp,3g2,mj2", "duration": "5.0"}
    }"#;
    let info = parse_media_info(json).unwrap();
    assert_eq!(info.video[0].sample_aspect_ratio, Some((16, 11)));
}

#[test]
fn non_audio_video_streams_are_ignored() {
    let json = r#"{
        "streams": [
            {"codec_type": "subtitle", "codec_name": "mov_text"},
            {"codec_type": "data", "codec_name": "bin_data"},
            {"codec_type": "video", "codec_name": "h264", "width": 640, "height": 360,
             "avg_frame_rate": "30/1", "r_frame_rate": "30/1"}
        ],
        "format": {"format_name": "mov,mp4,m4a,3gp,3g2,mj2", "duration": "5.0"}
    }"#;
    let info = parse_media_info(json).unwrap();
    assert_eq!(info.video.len(), 1);
    assert!(info.audio.is_empty());
}

#[test]
fn keyframe_times_are_read_one_per_line() {
    assert_eq!(
        parse_keyframe_times("0.000000\n1.000000\n2.000000\n"),
        vec![0.0, 1.0, 2.0]
    );
}

#[test]
fn keyframe_lines_that_carry_no_time_are_skipped() {
    assert_eq!(
        parse_keyframe_times("0.000000\nN/A\n\n2.500000\n"),
        vec![0.0, 2.5]
    );
}

#[test]
fn a_keyframe_line_with_trailing_side_data_reads_its_first_field() {
    assert_eq!(
        parse_keyframe_times("0.000000,H.26[45] User Data Unregistered SEI message\n"),
        vec![0.0]
    );
}

#[test]
fn an_empty_keyframe_output_reads_as_no_keyframe() {
    assert!(parse_keyframe_times("").is_empty());
}

fn still_probe_json() -> &'static str {
    r#"{
        "streams": [
            {"index": 0, "codec_name": "mjpeg", "codec_type": "video",
             "width": 320, "height": 180}
        ],
        "format": {"filename": "frame.jpg", "nb_streams": 1,
                   "format_name": "image_pipe", "size": "5432"}
    }"#
}

#[test]
fn a_still_report_reads_its_container_and_frame() {
    let still = parse_still_info(still_probe_json()).unwrap();
    assert_eq!(still.container, "image_pipe");
    assert_eq!(still.codec, "mjpeg");
    assert_eq!(still.width, 320);
    assert_eq!(still.height, 180);
    assert_eq!(still.size_bytes, Some(5432));
}

#[test]
fn a_still_report_without_an_image_stream_is_an_error() {
    let json = r#"{"streams": [{"codec_type": "audio", "codec_name": "aac"}],
                   "format": {"format_name": "wav"}}"#;
    assert!(matches!(
        parse_still_info(json),
        Err(ParseError::NoImageStream)
    ));
}

#[test]
fn a_still_report_without_a_format_section_is_an_error() {
    let json = r#"{"streams": [{"codec_type": "video", "width": 4, "height": 4}]}"#;
    assert!(matches!(
        parse_still_info(json),
        Err(ParseError::MissingFormat)
    ));
}

#[test]
fn a_container_name_carries_the_media_kind() {
    assert_eq!(media_kind("png_pipe"), MediaKind::Image);
    assert_eq!(media_kind("jpeg_pipe"), MediaKind::Image);
    assert_eq!(media_kind("image2"), MediaKind::Image);
    assert_eq!(media_kind("mov,mp4,m4a,3gp,3g2,mj2"), MediaKind::Video);
    assert_eq!(media_kind("matroska,webm"), MediaKind::Video);
    assert_eq!(media_kind(""), MediaKind::Video);
}

#[test]
fn subtitle_streams_are_reported() {
    let json = r#"{
        "streams": [
            {"codec_type": "video", "codec_name": "h264", "width": 640, "height": 360,
             "avg_frame_rate": "30/1", "r_frame_rate": "30/1"},
            {"codec_type": "subtitle", "codec_name": "mov_text", "tags": {"language": "eng"}}
        ],
        "format": {"format_name": "mov,mp4,m4a,3gp,3g2,mj2", "duration": "5.0"}
    }"#;
    let info = parse_media_info(json).unwrap();
    assert_eq!(info.subtitle.len(), 1);
    assert_eq!(info.subtitle[0].codec, "mov_text");
    assert_eq!(info.subtitle[0].language.as_deref(), Some("eng"));
}

#[test]
fn a_subtitle_stream_report_needs_no_duration() {
    let json = r#"{
        "streams": [
            {"codec_type": "subtitle", "codec_name": "eia_608"},
            {"codec_type": "subtitle", "codec_name": "dvb_teletext", "tags": {"language": "swe"}}
        ]
    }"#;
    let streams = parse_subtitle_streams(json).unwrap();
    assert_eq!(streams.len(), 2);
    assert_eq!(streams[0].codec, "eia_608");
    assert_eq!(streams[1].codec, "dvb_teletext");
    assert_eq!(streams[1].language.as_deref(), Some("swe"));
}
