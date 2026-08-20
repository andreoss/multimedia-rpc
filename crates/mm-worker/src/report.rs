use mm_domain::{AudioReport, InputReport, OutputProfile, Verdict, VerdictCode, VideoReport};
use mm_ffmpeg::{MediaInfo, TargetProfile};

pub fn input_report(info: &MediaInfo) -> InputReport {
    InputReport {
        container: info.container.clone(),
        duration: info.duration,
        size_bytes: info.size_bytes,
        drm: info.drm,
        video: info
            .video
            .iter()
            .map(|video| VideoReport {
                codec: video.codec.clone(),
                width: video.width,
                height: video.height,
                avg_fps: video.avg_fps,
                real_fps: video.real_fps,
                rotation: video.rotation,
                sample_aspect_ratio: video.sample_aspect_ratio,
                interlaced: video.interlaced,
            })
            .collect(),
        audio: info
            .audio
            .iter()
            .map(|audio| AudioReport {
                codec: audio.codec.clone(),
                channels: audio.channels,
                sample_rate: audio.sample_rate,
            })
            .collect(),
    }
}

pub fn target_profile(profile: Option<&OutputProfile>) -> TargetProfile {
    let mut target = TargetProfile::default();
    let Some(profile) = profile else {
        return target;
    };
    if let Some(fps) = profile.frame_rate() {
        target.fps = fps.round().max(1.0) as u32;
    }
    if let Some(bitrate) = profile.video_max_bitrate() {
        target.video_max_bitrate_kbps = bitrate;
    }
    if let Some(quality) = profile.video_quality() {
        target.video_crf = quality;
    }
    if let Some(bitrate) = profile.audio_bitrate() {
        target.audio_bitrate_kbps = bitrate;
    }
    if let Some(rate) = profile.audio_sample_rate() {
        target.audio_sample_rate_hz = rate;
    }
    target
}

pub fn blocks(verdicts: &[Verdict]) -> Vec<Verdict> {
    verdicts
        .iter()
        .copied()
        .filter(|verdict| matches!(verdict.code, VerdictCode::Block(_)))
        .collect()
}

pub fn warnings(verdicts: &[Verdict]) -> Vec<Verdict> {
    verdicts
        .iter()
        .copied()
        .filter(|verdict| matches!(verdict.code, VerdictCode::Warn(_)))
        .collect()
}

pub fn render_verdicts(verdicts: &[Verdict]) -> String {
    verdicts
        .iter()
        .map(|verdict| {
            let code = match verdict.code {
                VerdictCode::Block(code) => format!("{code:?}"),
                VerdictCode::Warn(code) => format!("{code:?}"),
            };
            match verdict.input {
                Some(index) => format!("input {index}: {code}"),
                None => format!("inputs: {code}"),
            }
        })
        .collect::<Vec<_>>()
        .join("; ")
}
