use mm_domain::{
    BlockCode, InputFailure, OutputProfile, Verdict, VerdictCode, parse_rendered_verdicts,
    render_verdicts, validate_ladder_rungs, verdict_code,
};

fn rung(width: u32, height: u32) -> OutputProfile {
    OutputProfile::from_raw(24.0, 800_000, 23, 96_000, 48_000, width, height).unwrap()
}

#[test]
fn a_rung_no_larger_than_the_source_passes() {
    let rungs = vec![rung(640, 360), rung(1280, 720)];
    assert_eq!(validate_ladder_rungs(&rungs, (1280, 720)), Vec::new());
}

#[test]
fn a_rung_wider_than_the_source_is_blocked_by_rung() {
    let rungs = vec![rung(640, 360), rung(1920, 1080)];
    assert_eq!(
        validate_ladder_rungs(&rungs, (1280, 720)),
        vec![Verdict {
            input: None,
            rung: Some(1),
            code: VerdictCode::Block(BlockCode::RungUpscale),
        }]
    );
}

#[test]
fn a_rung_taller_than_the_source_is_blocked_by_rung() {
    let rungs = vec![rung(640, 360), rung(1280, 1440)];
    let verdicts = validate_ladder_rungs(&rungs, (1280, 720));
    assert_eq!(verdicts.len(), 1);
    assert_eq!(verdicts[0].rung, Some(1));
}

#[test]
fn every_offending_rung_is_reported_not_only_the_first() {
    let rungs = vec![rung(1920, 1080), rung(1280, 720), rung(2560, 1440)];
    let verdicts = validate_ladder_rungs(&rungs, (1280, 720));
    let rungs_hit: Vec<_> = verdicts.iter().filter_map(|verdict| verdict.rung).collect();
    assert_eq!(rungs_hit, vec![0, 2]);
}

#[test]
fn a_rendered_rung_verdict_round_trips_through_its_parser() {
    let verdicts = vec![
        Verdict {
            input: Some(0),
            rung: None,
            code: VerdictCode::Block(BlockCode::NoVideoStream),
        },
        Verdict {
            input: None,
            rung: Some(2),
            code: VerdictCode::Block(BlockCode::RungUpscale),
        },
    ];
    let text = render_verdicts(&verdicts);
    assert_eq!(text, "input 0: NoVideoStream; rung 2: RungUpscale");
    let parsed = parse_rendered_verdicts(&text).expect("the rendered grammar reads back");
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[0].code(), "NoVideoStream");
    assert_eq!(parsed[0].rung(), None);
    assert_eq!(parsed[1].code(), "RungUpscale");
    assert_eq!(parsed[1].rung(), Some(2));
}

#[test]
fn an_input_verdict_carries_no_rung() {
    let parsed = parse_rendered_verdicts("input 3: AspectDrift; inputs: TooFewInputs").unwrap();
    assert_eq!(parsed[0].rung(), None);
    assert_eq!(parsed[1].rung(), None);
    assert_eq!(parsed[0].input(), Some(3));
}

#[test]
fn a_rung_subject_that_is_not_a_number_is_refused() {
    let error = parse_rendered_verdicts("rung two: RungUpscale").unwrap_err();
    assert!(error.to_string().contains("names no input"), "{error}");
}

#[test]
fn a_rung_verdict_becomes_an_input_failure_naming_its_rung() {
    let failure = InputFailure::from(&Verdict {
        input: None,
        rung: Some(2),
        code: VerdictCode::Block(BlockCode::RungUpscale),
    });
    assert_eq!(failure.rung(), Some(2));
    assert_eq!(failure.code(), "RungUpscale");
}

#[test]
fn a_rung_upscale_keeps_its_own_name_in_the_reference_shape() {
    assert_eq!(verdict_code("RungUpscale"), "ERROR_UPSCALE_NOT_ALLOWED");
    assert_ne!(
        verdict_code("RungUpscale"),
        verdict_code("ResolutionMismatch")
    );
}
