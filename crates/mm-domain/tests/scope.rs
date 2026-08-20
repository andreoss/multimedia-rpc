use mm_domain::{
    Frame, GradeAdjustments, GradeScope, GradeSpec, InputSource, MediaRef, OpSpecError, ScopeData,
    ScopeError, ScopeMode, ScopeOutput, measure,
};

fn object() -> MediaRef {
    MediaRef::object_uri("s3://bucket/path/file.mp4").unwrap()
}

fn source() -> InputSource {
    InputSource::from(object())
}

fn fixture_frame() -> Frame {
    Frame::new(4, 2, known_pixels()).unwrap()
}

fn known_pixels() -> Vec<u8> {
    let text = include_str!("golden/scope-frame.txt");
    text.lines()
        .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
        .flat_map(|line| line.split_whitespace())
        .flat_map(|pixel| {
            (0..pixel.len())
                .step_by(2)
                .map(|at| {
                    u8::from_str_radix(&pixel[at..at + 2], 16).expect("the fixture holds hex bytes")
                })
                .collect::<Vec<u8>>()
        })
        .collect()
}

#[test]
fn the_fixture_frame_is_the_size_it_claims() {
    assert_eq!(known_pixels().len(), 4 * 2 * 3);
    assert_eq!(fixture_frame().width(), 4);
    assert_eq!(fixture_frame().height(), 2);
}

#[test]
fn a_frame_whose_bytes_do_not_match_its_size_is_refused_by_name() {
    assert!(matches!(
        Frame::new(4, 2, vec![0; 10]),
        Err(ScopeError::FrameSize { .. })
    ));
}

#[test]
fn a_frame_with_no_width_is_refused_by_name() {
    assert!(matches!(
        Frame::new(0, 2, Vec::new()),
        Err(ScopeError::ZeroFrameDimension { .. })
    ));
}

#[test]
fn a_waveform_counts_luma_into_columns_and_bins() {
    let data = measure(ScopeMode::Waveform, &fixture_frame(), 2, 4).unwrap();
    assert_eq!(data.mode(), ScopeMode::Waveform);
    assert_eq!(data.components(), 1);
    assert_eq!(data.columns(), 2);
    assert_eq!(data.bins(), 4);
    assert_eq!(
        data.counts(),
        &[2, 0, 1, 1, 0, 2, 1, 1],
        "waveform counts per column"
    );
    assert_eq!(data.total(), 8);
}

#[test]
fn a_parade_counts_each_channel_into_its_own_panel() {
    let data = measure(ScopeMode::Parade, &fixture_frame(), 2, 4).unwrap();
    assert_eq!(data.components(), 3);
    assert_eq!(data.panel(0), &[2, 0, 1, 1, 1, 1, 0, 2], "red panel");
    assert_eq!(data.panel(1), &[2, 0, 1, 1, 1, 1, 0, 2], "green panel");
    assert_eq!(data.panel(2), &[1, 0, 1, 2, 3, 1, 0, 0], "blue panel");
    assert_eq!(data.total(), 24);
}

#[test]
fn a_histogram_counts_every_luma_value_the_frame_carries() {
    let data = measure(ScopeMode::Histogram, &fixture_frame(), 1, 256).unwrap();
    assert_eq!(data.components(), 1);
    assert_eq!(data.columns(), 1);
    assert_eq!(data.bins(), 256);
    let present: Vec<(u32, u32)> = data
        .counts()
        .iter()
        .enumerate()
        .filter(|(_, count)| **count > 0)
        .map(|(luma, count)| (luma as u32, *count))
        .collect();
    assert_eq!(
        present,
        vec![
            (0, 1),
            (29, 1),
            (64, 1),
            (76, 1),
            (128, 1),
            (150, 1),
            (226, 1),
            (255, 1)
        ]
    );
}

#[test]
fn a_vectorscope_counts_chroma_by_hue() {
    let data = measure(ScopeMode::Vectorscope, &fixture_frame(), 1, 64).unwrap();
    assert_eq!(data.components(), 1);
    assert_eq!(data.bins(), 64);
    let present: Vec<(u32, u32)> = data
        .counts()
        .iter()
        .enumerate()
        .filter(|(_, count)| **count > 0)
        .map(|(hue, count)| (hue as u32, *count))
        .collect();
    assert_eq!(present, vec![(0, 4), (19, 1), (30, 1), (41, 1), (62, 1)]);
}

#[test]
fn a_frame_scope_carries_no_measurements() {
    assert!(matches!(
        measure(ScopeMode::Frame, &fixture_frame(), 2, 4),
        Err(ScopeError::NotMeasured {
            mode: ScopeMode::Frame
        })
    ));
}

#[test]
fn a_grid_of_zero_columns_or_bins_is_refused_by_name() {
    assert!(matches!(
        measure(ScopeMode::Waveform, &fixture_frame(), 0, 4),
        Err(ScopeError::ZeroGrid { field: "columns" })
    ));
    assert!(matches!(
        measure(ScopeMode::Waveform, &fixture_frame(), 2, 0),
        Err(ScopeError::ZeroGrid { field: "bins" })
    ));
}

#[test]
fn a_grid_whose_product_overflows_is_refused_by_name() {
    assert!(matches!(
        measure(ScopeMode::Waveform, &fixture_frame(), u32::MAX, u32::MAX),
        Err(ScopeError::GridOverflow { .. })
    ));
}

#[test]
fn counts_that_do_not_fill_the_grid_are_refused_by_name() {
    assert!(matches!(
        ScopeData::new(ScopeMode::Waveform, 1, 2, 4, vec![0; 7]),
        Err(ScopeError::GridMismatch {
            expected: 8,
            found: 7
        })
    ));
}

#[test]
fn counts_beyond_the_grid_are_refused_by_name() {
    assert!(matches!(
        ScopeData::new(ScopeMode::Waveform, 1, 2, 4, vec![0; 9]),
        Err(ScopeError::GridMismatch {
            expected: 8,
            found: 9
        })
    ));
}

#[test]
fn a_frame_mode_result_is_refused_by_name() {
    assert!(matches!(
        ScopeData::new(ScopeMode::Frame, 1, 1, 1, vec![0]),
        Err(ScopeError::NotMeasured {
            mode: ScopeMode::Frame
        })
    ));
}

#[test]
fn every_scope_mode_names_its_grid_and_its_panels() {
    assert_eq!(ScopeMode::Waveform.grid(), (1, 256, 64));
    assert_eq!(ScopeMode::Parade.grid(), (3, 256, 64));
    assert_eq!(ScopeMode::Histogram.grid(), (1, 1, 256));
    assert_eq!(ScopeMode::Vectorscope.grid(), (1, 1, 64));
}

#[test]
fn a_scope_asking_for_data_on_a_frame_mode_is_refused_by_name() {
    let scope = GradeScope::new(160, 120)
        .unwrap()
        .with_mode(ScopeMode::Frame)
        .with_output(ScopeOutput::Data);
    assert!(matches!(
        GradeSpec::new(
            source(),
            GradeAdjustments::new(0.1, 1.0, 1.0, 1.0).unwrap(),
            None,
            Some(scope)
        ),
        Err(OpSpecError::FrameScopeData)
    ));
}

#[test]
fn a_scope_keeps_its_output_and_grid() {
    let scope = GradeScope::new(160, 120)
        .unwrap()
        .with_mode(ScopeMode::Vectorscope)
        .with_output(ScopeOutput::Data)
        .with_grid(96, 48);
    assert_eq!(scope.mode(), ScopeMode::Vectorscope);
    assert_eq!(scope.output(), ScopeOutput::Data);
    assert_eq!(scope.columns(), 96);
    assert_eq!(scope.bins(), 48);
}

#[test]
fn a_scope_naming_no_grid_takes_the_mode_s_own() {
    let scope = GradeScope::new(160, 120)
        .unwrap()
        .with_mode(ScopeMode::Parade)
        .with_output(ScopeOutput::Data);
    let (components, columns, bins) = scope.grid();
    assert_eq!((components, columns, bins), (3, 256, 64));
}

#[test]
fn a_scope_asks_for_an_image_until_it_asks_otherwise() {
    let scope = GradeScope::new(160, 120).unwrap();
    assert_eq!(scope.output(), ScopeOutput::Image);
    assert_eq!(scope.mode(), ScopeMode::Frame);
}

#[test]
fn every_scope_mode_and_output_names_itself() {
    for mode in [
        ScopeMode::Frame,
        ScopeMode::Waveform,
        ScopeMode::Parade,
        ScopeMode::Histogram,
        ScopeMode::Vectorscope,
    ] {
        assert!(!mode.name().is_empty());
    }
    assert_eq!(ScopeMode::Histogram.name(), "histogram");
    assert_eq!(ScopeOutput::Data.name(), "data");
    assert_eq!(ScopeOutput::Image.name(), "image");
}
