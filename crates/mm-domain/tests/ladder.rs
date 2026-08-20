use mm_domain::{
    BatchSpec, LadderResult, LadderSpec, MAX_LADDER_RUNGS, MediaRef, OpKind, OpResultError, OpSpec,
    OpSpecError, OutputProfile, RenditionResult, UnsupportedOp,
};

fn input() -> MediaRef {
    MediaRef::object_uri("s3://bucket/in.mp4").unwrap()
}

fn object(key: &str) -> MediaRef {
    MediaRef::object_uri(&format!("s3://bucket/{key}")).unwrap()
}

fn rung(width: u32, height: u32, video_bitrate: u32, audio_bitrate: u32) -> OutputProfile {
    OutputProfile::from_raw(
        24.0,
        video_bitrate,
        23,
        audio_bitrate,
        48_000,
        width,
        height,
    )
    .unwrap()
}

fn three_rungs() -> Vec<OutputProfile> {
    vec![
        rung(640, 360, 800_000, 96_000),
        rung(1280, 720, 2_500_000, 96_000),
        rung(1920, 1080, 5_000_000, 96_000),
    ]
}

fn ladder(rungs: Vec<OutputProfile>) -> Result<LadderSpec, OpSpecError> {
    LadderSpec::new(input(), rungs)
}

#[test]
fn a_ladder_keeps_its_rungs_in_order() {
    let spec = ladder(three_rungs()).unwrap();
    assert_eq!(
        spec.input().media().unwrap().media().to_uri(),
        "s3://bucket/in.mp4"
    );
    assert_eq!(spec.rungs().len(), 3);
    assert_eq!(spec.rungs()[0].resolution(), Some((640, 360)));
    assert_eq!(spec.rungs()[2].resolution(), Some((1920, 1080)));
    assert_eq!(spec.segment_seconds(), None);
    assert_eq!(spec.poster_seconds(), None);
}

#[test]
fn a_ladder_refuses_fewer_than_two_rungs() {
    assert_eq!(
        ladder(vec![rung(640, 360, 800_000, 96_000)]).unwrap_err(),
        OpSpecError::EmptyLadder { count: 1 }
    );
    assert_eq!(
        ladder(vec![]).unwrap_err(),
        OpSpecError::EmptyLadder { count: 0 }
    );
}

#[test]
fn a_ladder_refuses_more_rungs_than_the_cap() {
    let rungs: Vec<OutputProfile> = (0..=MAX_LADDER_RUNGS)
        .map(|index| {
            let step = index as u32 + 1;
            rung(320 * step, 180 * step, 200_000 * step, 96_000)
        })
        .collect();
    assert_eq!(
        ladder(rungs).unwrap_err(),
        OpSpecError::TooManyRungs {
            count: MAX_LADDER_RUNGS + 1,
            max: MAX_LADDER_RUNGS,
        }
    );
}

#[test]
fn a_ladder_accepts_exactly_the_cap() {
    let rungs: Vec<OutputProfile> = (0..MAX_LADDER_RUNGS)
        .map(|index| {
            let step = index as u32 + 1;
            rung(320 * step, 180 * step, 200_000 * step, 96_000)
        })
        .collect();
    assert_eq!(ladder(rungs).unwrap().rungs().len(), MAX_LADDER_RUNGS);
}

#[test]
fn a_rung_without_a_resolution_is_refused_by_index() {
    let mut rungs = three_rungs();
    rungs[1] = OutputProfile::from_raw(24.0, 2_500_000, 23, 128_000, 48_000, 0, 0).unwrap();
    assert_eq!(
        ladder(rungs).unwrap_err(),
        OpSpecError::RungWithoutResolution { index: 1 }
    );
}

#[test]
fn a_rung_without_a_video_bitrate_is_refused_by_index() {
    let mut rungs = three_rungs();
    rungs[2] = OutputProfile::from_raw(24.0, 0, 23, 128_000, 48_000, 1920, 1080).unwrap();
    assert_eq!(
        ladder(rungs).unwrap_err(),
        OpSpecError::RungWithoutVideoBitrate { index: 2 }
    );
}

#[test]
fn an_empty_rung_profile_is_refused() {
    let mut rungs = three_rungs();
    rungs[0] = OutputProfile::from_raw(0.0, 0, 0, 0, 0, 0, 0).unwrap();
    assert_eq!(ladder(rungs).unwrap_err(), OpSpecError::EmptyProfile);
}

#[test]
fn a_rung_that_does_not_grow_is_refused_by_index() {
    let mut rungs = three_rungs();
    rungs[2] = rung(1280, 720, 6_000_000, 96_000);
    assert_eq!(
        ladder(rungs).unwrap_err(),
        OpSpecError::NonMonotonicRung { index: 2 }
    );
}

#[test]
fn a_rung_whose_bitrate_does_not_grow_is_refused_by_index() {
    let mut rungs = three_rungs();
    rungs[2] = rung(1920, 1080, 1_000_000, 96_000);
    assert_eq!(
        ladder(rungs).unwrap_err(),
        OpSpecError::NonMonotonicRung { index: 2 }
    );
}

#[test]
fn a_rung_whose_audio_bitrate_rises_is_refused_by_index() {
    let mut rungs = three_rungs();
    rungs[2] = rung(1920, 1080, 5_000_000, 192_000);
    assert_eq!(
        ladder(rungs).unwrap_err(),
        OpSpecError::RungAudioBitrateRises { index: 2 }
    );
}

#[test]
fn a_rung_carrying_its_own_poster_is_refused_by_index() {
    let mut rungs = three_rungs();
    rungs[0] = rung(640, 360, 800_000, 96_000).with_poster(1.0).unwrap();
    assert_eq!(
        ladder(rungs).unwrap_err(),
        OpSpecError::RungCarriesPoster { index: 0 }
    );
}

#[test]
fn a_rung_carrying_its_own_range_is_refused_by_index() {
    let range = mm_domain::OutputRange::new(0.0, 4.0).unwrap();
    let mut rungs = three_rungs();
    rungs[1] = rung(1280, 720, 2_500_000, 96_000).with_range(range);
    assert_eq!(
        ladder(rungs).unwrap_err(),
        OpSpecError::RungCarriesRange { index: 1 }
    );
}

#[test]
fn a_ladder_takes_a_segment_length_and_a_poster() {
    let spec = LadderSpec::new(input(), three_rungs())
        .unwrap()
        .with_segment_seconds(6.0)
        .unwrap()
        .with_poster(2.0)
        .unwrap();
    assert_eq!(spec.segment_seconds(), Some(6.0));
    assert_eq!(spec.poster_seconds(), Some(2.0));
}

#[test]
fn a_ladder_refuses_a_segment_length_that_is_not_positive_and_finite() {
    for value in [0.0, -1.0, f64::INFINITY] {
        assert_eq!(
            LadderSpec::new(input(), three_rungs())
                .unwrap()
                .with_segment_seconds(value)
                .unwrap_err(),
            OpSpecError::InvalidSegmentLength { value }
        );
    }
    assert!(matches!(
        LadderSpec::new(input(), three_rungs())
            .unwrap()
            .with_segment_seconds(f64::NAN)
            .unwrap_err(),
        OpSpecError::InvalidSegmentLength { value } if value.is_nan()
    ));
}

#[test]
fn a_ladder_refuses_a_poster_that_is_negative_or_not_finite() {
    for value in [-1.0, f64::NAN, f64::INFINITY] {
        assert!(matches!(
            LadderSpec::new(input(), three_rungs())
                .unwrap()
                .with_poster(value)
                .unwrap_err(),
            OpSpecError::Profile(_)
        ));
    }
}

fn rendition(index: usize) -> RenditionResult {
    RenditionResult::new(
        three_rungs()[index],
        object(&format!("renditions/{index}/output.mp4")),
        None,
        Vec::new(),
        8.0,
    )
    .unwrap()
    .with_measurement(640 * (index as u32 + 1), 360 * (index as u32 + 1), 800_000)
}

#[test]
fn a_ladder_result_keeps_one_entry_per_rung() {
    let result = LadderResult::new(
        Some(object("master.m3u8")),
        vec![rendition(0), rendition(1)],
    )
    .unwrap();
    assert_eq!(result.master_playlist(), Some(&object("master.m3u8")));
    assert_eq!(result.renditions().len(), 2);
    assert_eq!(result.renditions()[1].width(), 1280);
    assert_eq!(result.renditions()[1].height(), 720);
    assert_eq!(result.renditions()[0].duration_seconds(), 8.0);
    assert_eq!(result.renditions()[0].bandwidth_bps(), 800_000);
    assert_eq!(result.renditions()[0].playlist(), None);
}

#[test]
fn a_ladder_result_without_a_master_playlist_keeps_its_renditions() {
    let result = LadderResult::new(None, vec![rendition(0), rendition(1)]).unwrap();
    assert_eq!(result.master_playlist(), None);
    assert_eq!(result.renditions().len(), 2);
}

#[test]
fn a_ladder_result_with_no_renditions_is_refused() {
    assert_eq!(
        LadderResult::new(Some(object("master.m3u8")), Vec::new()).unwrap_err(),
        OpResultError::EmptyOutputs
    );
}

#[test]
fn a_ladder_result_rung_with_no_playlist_and_segments_is_accepted() {
    let result = rendition(0);
    assert_eq!(result.playlist(), None);
    assert!(result.segments().is_empty());
}

#[test]
fn a_ladder_result_rung_with_a_playlist_carries_its_segments() {
    let segments = vec![
        object("renditions/0/seg00000.m4s"),
        object("renditions/0/seg00001.m4s"),
    ];
    let result = RenditionResult::new(
        three_rungs()[0],
        object("renditions/0/output.mp4"),
        Some(object("renditions/0/playlist.m3u8")),
        segments.clone(),
        8.0,
    )
    .unwrap()
    .with_measurement(640, 360, 800_000);
    assert_eq!(result.segments(), segments.as_slice());
    assert_eq!(
        result.playlist(),
        Some(&object("renditions/0/playlist.m3u8"))
    );
}

#[test]
fn a_ladder_spec_names_one_input() {
    let spec = OpSpec::Ladder(LadderSpec::new(input(), three_rungs()).unwrap());
    assert_eq!(spec.kind(), OpKind::Ladder);
    assert_eq!(spec.inputs().len(), 1);
    assert_eq!(
        spec.inputs()[0].media().unwrap().media().to_uri(),
        "s3://bucket/in.mp4"
    );
}

#[test]
fn a_ladder_is_not_executable_until_the_worker_runs_it() {
    assert_eq!(
        OpSpec::Ladder(LadderSpec::new(input(), three_rungs()).unwrap())
            .kind()
            .ensure_executable()
            .unwrap_err(),
        UnsupportedOp {
            kind: OpKind::Ladder
        }
    );
}

#[test]
fn a_ladder_validates_its_input_under_the_transcode_policy() {
    let policy = OpKind::Ladder.input_policy().expect("a ladder takes video");
    assert_eq!(policy.min_inputs, 1);
    assert!(!policy.cross_input_checks);
}

#[test]
fn a_ladder_inside_a_batch_keeps_its_input_in_the_batch_order() {
    let ladder = OpSpec::Ladder(LadderSpec::new(input(), three_rungs()).unwrap());
    let batch = OpSpec::Batch(BatchSpec::new(vec![ladder.clone()], true).unwrap());
    assert_eq!(batch.inputs().len(), 1);
}
