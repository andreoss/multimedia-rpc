use mm_domain::{MediaRef, OpResult, OutputProfile, TranscodeResult, TranscodeSpec, VideoEncoder};
use mm_proto::v1;

fn object() -> MediaRef {
    MediaRef::object_uri("s3://bucket/file.mp4").unwrap()
}

#[test]
fn a_profile_round_trips_its_encoder_and_requirement() {
    let profile = OutputProfile::from_raw(30.0, 4_000_000, 23, 128_000, 48_000, 1280, 720)
        .unwrap()
        .with_encoder(VideoEncoder::H264Nvenc, true)
        .unwrap();
    let spec = TranscodeSpec::new(mm_domain::InputSource::from(object()), profile).unwrap();
    let decoded = mm_domain::OpSpec::try_from(v1::OpSpec::from(&mm_domain::OpSpec::Transcode(
        spec.clone(),
    )))
    .unwrap();
    assert_eq!(decoded, mm_domain::OpSpec::Transcode(spec));
}

#[test]
fn a_transcode_result_names_the_encoder_used() {
    let result = TranscodeResult::new(object()).with_encoder(VideoEncoder::H264Qsv);
    let decoded =
        OpResult::try_from(v1::OpResult::from(&OpResult::Transcode(result.clone()))).unwrap();
    assert_eq!(decoded, OpResult::Transcode(result));
}
