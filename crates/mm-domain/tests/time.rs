use mm_domain::{
    LadderSpec, MediaRef, OutputProfile, OutputRange, PackageSpec, Rate, TICKS_PER_SECOND,
    ThumbnailSpec, Ticks, TimeError, TrimRange, TrimSpec,
};
use proptest::prelude::*;

const FRAME_RATES: [(u32, u32); 10] = [
    (24_000, 1_001),
    (30_000, 1_001),
    (60_000, 1_001),
    (24, 1),
    (25, 1),
    (30, 1),
    (48, 1),
    (50, 1),
    (60, 1),
    (120, 1),
];

const SAMPLE_RATES: [u32; 3] = [44_100, 48_000, 96_000];

fn object() -> MediaRef {
    MediaRef::object_uri("s3://bucket/path/file.mp4").unwrap()
}

#[test]
fn the_tick_is_the_published_count_per_second() {
    assert_eq!(TICKS_PER_SECOND, 254_016_000_000);
    assert_eq!(Ticks::from_seconds(1.0).unwrap().get(), TICKS_PER_SECOND);
}

#[test]
fn a_frame_at_every_common_rate_is_a_whole_number_of_ticks() {
    for (numerator, denominator) in FRAME_RATES {
        let rate = Rate::new(numerator, denominator).unwrap();
        let frame = Ticks::from_frames(1, rate).unwrap();
        assert_eq!(
            frame.get() * u64::from(numerator),
            TICKS_PER_SECOND * u64::from(denominator),
            "rate {numerator}/{denominator}"
        );
    }
}

#[test]
fn a_sample_at_every_common_rate_is_a_whole_number_of_ticks() {
    for hz in SAMPLE_RATES {
        let rate = Rate::new(hz, 1).unwrap();
        let sample = Ticks::from_frames(1, rate).unwrap();
        assert_eq!(sample.get() * u64::from(hz), TICKS_PER_SECOND, "rate {hz}");
    }
}

#[test]
fn a_rate_that_does_not_divide_the_tick_is_refused_by_name() {
    let rate = Rate::new(7_919, 1).unwrap();
    assert_eq!(
        Ticks::from_frames(1, rate),
        Err(TimeError::RateNotExact {
            numerator: 7_919,
            denominator: 1
        })
    );
}

#[test]
fn a_rate_with_a_zero_term_is_refused() {
    assert_eq!(
        Rate::new(0, 1),
        Err(TimeError::ZeroRate {
            numerator: 0,
            denominator: 1
        })
    );
    assert_eq!(
        Rate::new(24, 0),
        Err(TimeError::ZeroRate {
            numerator: 24,
            denominator: 0
        })
    );
}

#[test]
fn seconds_that_are_negative_or_not_finite_are_refused() {
    for value in [-0.001, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert!(
            matches!(Ticks::from_seconds(value), Err(TimeError::NotFinite { .. })),
            "{value} was accepted"
        );
    }
}

#[test]
fn seconds_past_the_largest_tick_are_refused() {
    assert!(matches!(
        Ticks::from_seconds(1.0e12),
        Err(TimeError::Overflow)
    ));
}

#[test]
fn the_microsecond_rendering_rounds_to_nearest() {
    let one_frame = Ticks::from_frames(1, Rate::new(24_000, 1_001).unwrap()).unwrap();
    assert_eq!(one_frame.micros(), 41_708);
    assert_eq!(Ticks::from_seconds(0.5).unwrap().micros(), 500_000);
    assert_eq!(Ticks::new(TICKS_PER_SECOND / 2_000_000).micros(), 1);
    assert_eq!(Ticks::new(TICKS_PER_SECOND / 2_000_000 - 1).micros(), 0);
}

#[test]
fn arithmetic_is_checked() {
    let max = Ticks::new(u64::MAX);
    assert_eq!(max.checked_add(Ticks::new(1)), None);
    assert_eq!(Ticks::new(1).checked_sub(Ticks::new(2)), None);
    assert_eq!(
        Ticks::new(3).checked_add(Ticks::new(4)),
        Some(Ticks::new(7))
    );
    assert_eq!(
        Ticks::new(4).checked_sub(Ticks::new(3)),
        Some(Ticks::new(1))
    );
}

#[test]
fn a_trim_range_holds_ticks_and_still_answers_seconds() {
    let trim = TrimRange::new(1.5, 4.0).unwrap();
    assert_eq!(trim.start(), Ticks::from_seconds(1.5).unwrap());
    assert_eq!(trim.duration(), Ticks::from_seconds(4.0).unwrap());
    assert_eq!(trim.start_seconds(), 1.5);
    assert_eq!(trim.duration_seconds(), 4.0);
    let exact = TrimRange::from_ticks(Ticks::new(7), Ticks::new(9)).unwrap();
    assert_eq!(exact.start().get(), 7);
    assert_eq!(exact.duration().get(), 9);
}

#[test]
fn a_trim_range_refuses_a_duration_that_rounds_to_no_tick() {
    assert!(TrimRange::new(0.0, 1.0e-13).is_err());
    assert!(TrimRange::from_ticks(Ticks::new(0), Ticks::new(0)).is_err());
}

#[test]
fn an_output_range_holds_ticks() {
    let range = OutputRange::new(0.25, 2.0).unwrap();
    assert_eq!(range.start(), Ticks::from_seconds(0.25).unwrap());
    assert_eq!(range.duration(), Ticks::from_seconds(2.0).unwrap());
    let exact = OutputRange::from_ticks(Ticks::new(1), Ticks::new(2)).unwrap();
    assert_eq!(exact.start_seconds(), 1.0 / TICKS_PER_SECOND as f64);
    assert!(OutputRange::from_ticks(Ticks::new(1), Ticks::new(0)).is_err());
}

#[test]
fn a_trim_spec_holds_ticks() {
    let spec = TrimSpec::new(object(), 2.0, 3.5).unwrap();
    assert_eq!(spec.start(), Ticks::from_seconds(2.0).unwrap());
    assert_eq!(spec.duration(), Ticks::from_seconds(3.5).unwrap());
    let exact = TrimSpec::from_ticks(object(), Ticks::new(0), Ticks::new(1)).unwrap();
    assert_eq!(exact.duration().get(), 1);
    assert!(TrimSpec::from_ticks(object(), Ticks::new(0), Ticks::new(0)).is_err());
}

#[test]
fn thumbnail_offsets_hold_ticks() {
    let spec = ThumbnailSpec::new(object(), vec![0.0, 1.5], 320, 180).unwrap();
    assert_eq!(
        spec.offsets(),
        &[Ticks::new(0), Ticks::from_seconds(1.5).unwrap()]
    );
    assert_eq!(spec.offsets_seconds(), vec![0.0, 1.5]);
}

#[test]
fn a_segment_length_holds_ticks() {
    let spec = PackageSpec::new(object(), 4.0).unwrap();
    assert_eq!(spec.segment(), Ticks::from_seconds(4.0).unwrap());
    assert_eq!(spec.segment_seconds(), 4.0);
    assert!(PackageSpec::from_ticks(object(), Ticks::new(0)).is_err());
}

#[test]
fn a_poster_and_a_ladder_segment_hold_ticks() {
    let profile = OutputProfile::from_raw(24.0, 1000, 0, 96, 48_000, 640, 360)
        .unwrap()
        .with_poster(1.25)
        .unwrap();
    assert_eq!(profile.poster(), Some(Ticks::from_seconds(1.25).unwrap()));
    assert_eq!(profile.poster_seconds(), Some(1.25));

    let low = OutputProfile::from_raw(24.0, 1000, 0, 96, 48_000, 640, 360).unwrap();
    let high = OutputProfile::from_raw(24.0, 2000, 0, 96, 48_000, 1280, 720).unwrap();
    let ladder = LadderSpec::new(object(), vec![low, high])
        .unwrap()
        .with_segment_seconds(6.0)
        .unwrap()
        .with_poster(0.5)
        .unwrap();
    assert_eq!(ladder.segment(), Some(Ticks::from_seconds(6.0).unwrap()));
    assert_eq!(ladder.poster(), Some(Ticks::from_seconds(0.5).unwrap()));
    assert_eq!(ladder.segment_seconds(), Some(6.0));
    assert_eq!(ladder.poster_seconds(), Some(0.5));
}

proptest! {
    #[test]
    fn frame_counts_add_exactly(
        a in 0u64..1_000_000,
        b in 0u64..1_000_000,
        index in 0usize..FRAME_RATES.len(),
    ) {
        let (numerator, denominator) = FRAME_RATES[index];
        let rate = Rate::new(numerator, denominator).unwrap();
        let sum = Ticks::from_frames(a, rate).unwrap()
            .checked_add(Ticks::from_frames(b, rate).unwrap())
            .unwrap();
        prop_assert_eq!(sum, Ticks::from_frames(a + b, rate).unwrap());
    }

    #[test]
    fn seconds_round_trip_to_the_tick_they_were_parsed_into(ticks in 0u64..(1u64 << 51)) {
        let value = Ticks::new(ticks);
        prop_assert_eq!(Ticks::from_seconds(value.seconds()).unwrap(), value);
    }

    #[test]
    fn a_trim_range_answers_the_seconds_it_was_given(
        start in 0.0f64..10_000.0,
        duration in 0.001f64..10_000.0,
    ) {
        let trim = TrimRange::new(start, duration).unwrap();
        prop_assert!((trim.start_seconds() - start).abs() < 1.0 / TICKS_PER_SECOND as f64);
        prop_assert!((trim.duration_seconds() - duration).abs() < 1.0 / TICKS_PER_SECOND as f64);
    }
}

#[test]
fn the_float_round_trip_is_exact_up_to_the_published_bound() {
    let bound = Ticks::new((1u64 << 51) - 1);
    assert_eq!(Ticks::from_seconds(bound.seconds()).unwrap(), bound);
    assert!(bound.seconds() > 8_000.0);
}
