use mm_ffmpeg::{HlsError, HlsTarget, TargetProfile, build_hls_args};

#[test]
fn an_hls_pack_writes_a_media_playlist_and_its_segments() {
    let args = build_hls_args(
        "https://host/media.mp4",
        &HlsTarget {
            profile: TargetProfile::default(),
            segment_seconds: 6.0,
            copy_streams: false,
        },
        "scratch/out",
    )
    .unwrap();
    assert_eq!(args[0..4], ["-y", "-nostdin", "-hide_banner", "-i"]);
    assert_eq!(args[4], "https://host/media.mp4");
    assert_eq!(args[position(&args, "-f") + 1], "hls");
    assert_eq!(args[position(&args, "-hls_playlist_type") + 1], "vod");
    assert_eq!(args[position(&args, "-hls_time") + 1], "6.000");
    assert_eq!(
        args[position(&args, "-hls_segment_filename") + 1],
        "scratch/out/seg%05d.m4s"
    );
    assert_eq!(args[position(&args, "-hls_segment_type") + 1], "fmp4");
    assert_eq!(args.last().unwrap(), "scratch/out/playlist.m3u8");
}

#[test]
fn an_hls_pack_writes_the_rung_s_own_encode_targets() {
    let profile = TargetProfile {
        video_max_bitrate_kbps: 2_500,
        video_crf: 26,
        audio_bitrate_kbps: 96,
        ..TargetProfile::default()
    };
    let args = build_hls_args(
        "media.mp4",
        &HlsTarget {
            profile,
            segment_seconds: 1.0,
            copy_streams: false,
        },
        "out",
    )
    .unwrap();
    assert_eq!(args[position(&args, "-crf") + 1], "26");
    assert_eq!(args[position(&args, "-maxrate") + 1], "2500k");
    assert_eq!(args[position(&args, "-bufsize") + 1], "5000k");
    assert_eq!(args[position(&args, "-b:a") + 1], "96k");
    assert_eq!(args[position(&args, "-ar") + 1], "48000");
    assert_eq!(args[position(&args, "-c:v") + 1], "libx264");
    assert_eq!(args[position(&args, "-c:a") + 1], "aac");
    assert!(
        args.windows(2)
            .any(|pair| pair == ["-hls_flags", "independent_segments+temp_file"]),
        "the flags are missing: {args:?}"
    );
}

#[test]
fn an_hls_pack_refuses_a_segment_length_that_is_not_positive_and_finite() {
    for value in [0.0, -1.0] {
        let target = HlsTarget {
            profile: TargetProfile::default(),
            segment_seconds: value,
            copy_streams: false,
        };
        assert_eq!(
            build_hls_args("media.mp4", &target, "out").unwrap_err(),
            HlsError::SegmentLength { value }
        );
    }
}

#[test]
fn an_hls_pack_refuses_an_output_dir_that_is_not_a_path() {
    let target = HlsTarget {
        profile: TargetProfile::default(),
        segment_seconds: 6.0,
        copy_streams: false,
    };
    assert_eq!(
        build_hls_args("media.mp4", &target, "").unwrap_err(),
        HlsError::OutputDir
    );
}

fn position(args: &[String], flag: &str) -> usize {
    args.iter()
        .position(|arg| arg == flag)
        .unwrap_or_else(|| panic!("{flag} is missing from the args"))
}

#[test]
fn an_hls_pack_can_carry_an_encode_it_was_given() {
    let profile = TargetProfile {
        video_max_bitrate_kbps: 800,
        ..TargetProfile::default()
    };
    let args = build_hls_args(
        "media.mp4",
        &HlsTarget {
            profile,
            segment_seconds: 6.0,
            copy_streams: false,
        },
        "out",
    )
    .unwrap();
    assert_eq!(args[position(&args, "-maxrate") + 1], "800k");
    assert_eq!(args[position(&args, "-c:v") + 1], "libx264");
}

#[test]
fn an_hls_pack_can_package_media_it_was_given_without_re_encoding_it() {
    let args = build_hls_args(
        "media.mp4",
        &HlsTarget {
            profile: TargetProfile::default(),
            segment_seconds: 6.0,
            copy_streams: true,
        },
        "out",
    )
    .unwrap();
    assert_eq!(args[position(&args, "-c:v") + 1], "copy");
    assert_eq!(args[position(&args, "-c:a") + 1], "copy");
    assert!(
        !args.iter().any(|arg| arg == "-crf" || arg == "-maxrate"),
        "a packaging invocation must not carry encode targets: {args:?}"
    );
    assert_eq!(args[position(&args, "-hls_segment_type") + 1], "fmp4");
}
