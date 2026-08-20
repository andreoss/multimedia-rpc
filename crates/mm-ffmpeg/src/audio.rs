use std::fmt::Write as _;

use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AudioTarget {
    pub gain_db: f64,
    pub fade_in_micros: u64,
    pub fade_out_micros: u64,
    pub duration_micros: u64,
    pub high_pass_hz: Option<u32>,
    pub low_pass_hz: Option<u32>,
    pub limiter_true_peak_db: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Error)]
pub enum AudioError {
    #[error("audio needs at least one input")]
    NoInputs,
    #[error(
        "fade-in {fade_in_micros} us plus fade-out {fade_out_micros} us exceeds duration {duration_micros} us"
    )]
    FadeExceedsDuration {
        fade_in_micros: u64,
        fade_out_micros: u64,
        duration_micros: u64,
    },
}

fn seconds(micros: u64) -> String {
    format!("{}.{:06}", micros / 1_000_000, micros % 1_000_000)
}

pub fn build_audio_args(
    inputs: &[String],
    target: &AudioTarget,
    output: &str,
) -> Result<Vec<String>, AudioError> {
    if inputs.is_empty() {
        return Err(AudioError::NoInputs);
    }
    if target
        .fade_in_micros
        .checked_add(target.fade_out_micros)
        .is_none_or(|total| total > target.duration_micros)
    {
        return Err(AudioError::FadeExceedsDuration {
            fade_in_micros: target.fade_in_micros,
            fade_out_micros: target.fade_out_micros,
            duration_micros: target.duration_micros,
        });
    }
    let mut args: Vec<String> = Vec::new();
    for input in inputs {
        args.push("-i".to_string());
        args.push(input.clone());
    }
    let mut graph = String::new();
    for index in 0..inputs.len() {
        let _ = write!(
            graph,
            "[{index}:a]aformat=sample_fmts=fltp:sample_rates=48000:channel_layouts=stereo,\
             volume={}dB",
            target.gain_db
        );
        if let Some(hz) = target.high_pass_hz {
            let _ = write!(graph, ",highpass=f={hz}");
        }
        if let Some(hz) = target.low_pass_hz {
            let _ = write!(graph, ",lowpass=f={hz}");
        }
        if target.fade_in_micros > 0 {
            let _ = write!(
                graph,
                ",afade=t=in:st=0:d={}",
                seconds(target.fade_in_micros)
            );
        }
        if target.fade_out_micros > 0 {
            let start = target.duration_micros - target.fade_out_micros;
            let _ = write!(
                graph,
                ",afade=t=out:st={}:d={}",
                seconds(start),
                seconds(target.fade_out_micros)
            );
        }
        let _ = write!(graph, "[a{index}];");
    }
    let mix_inputs: String = (0..inputs.len())
        .map(|index| format!("[a{index}]"))
        .collect();
    if inputs.len() == 1 {
        let _ = write!(graph, "{mix_inputs}anull[a]");
    } else {
        let _ = write!(
            graph,
            "{mix_inputs}amix=inputs={}:duration=longest:dropout_transition=0:normalize=0[mix]",
            inputs.len()
        );
        let _ = write!(graph, ";[mix]anull[a]");
    }
    if let Some(ceiling) = target.limiter_true_peak_db {
        let limit = 10f64.powf(ceiling / 20.0);
        let _ = write!(graph, ";[a]alimiter=limit={limit:.6}:level=false[aout]");
    } else {
        let _ = write!(graph, ";[a]anull[aout]");
    }
    args.push("-filter_complex".to_string());
    args.push(graph);
    args.push("-map".to_string());
    args.push("[aout]".to_string());
    args.push("-vn".to_string());
    args.extend(
        [
            "-c:a",
            "pcm_s16le",
            "-ar",
            "48000",
            "-ac",
            "2",
            "-movflags",
            "+faststart",
            "-f",
            "wav",
            output,
        ]
        .into_iter()
        .map(str::to_string),
    );
    Ok(args)
}
