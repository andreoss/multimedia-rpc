use std::time::Duration;

use mm_ffmpeg::ProgressParser;

fn feed(parser: &mut ProgressParser, lines: &[&str]) -> Vec<mm_ffmpeg::ProgressUpdate> {
    lines
        .iter()
        .filter_map(|line| parser.feed_line(line))
        .collect()
}

#[test]
fn a_block_emits_one_update_on_its_progress_line() {
    let mut parser = ProgressParser::new(Duration::from_secs(10));
    let updates = feed(
        &mut parser,
        &[
            "frame=120",
            "fps=24.0",
            "out_time_us=2500000",
            "out_time=00:00:02.500000",
            "speed=1.2x",
            "progress=continue",
        ],
    );
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0].out_time, Duration::from_micros(2_500_000));
    assert!((updates[0].percent - 25.0).abs() < 0.01);
    assert!(!updates[0].complete);
}

#[test]
fn the_end_block_reports_one_hundred_percent_complete() {
    let mut parser = ProgressParser::new(Duration::from_secs(10));
    let updates = feed(&mut parser, &["out_time_us=9900000", "progress=end"]);
    assert_eq!(updates.len(), 1);
    assert!((updates[0].percent - 100.0).abs() < f64::EPSILON);
    assert!(updates[0].complete);
}

#[test]
fn percent_never_regresses_and_never_exceeds_one_hundred() {
    let mut parser = ProgressParser::new(Duration::from_secs(10));
    let first = parser.feed_line("out_time_us=5000000").map(|_| ());
    assert!(first.is_none());
    let half = parser.feed_line("progress=continue").unwrap();
    assert!((half.percent - 50.0).abs() < 0.01);

    parser.feed_line("out_time_us=3000000");
    let regressed = parser.feed_line("progress=continue").unwrap();
    assert!((regressed.percent - 50.0).abs() < 0.01);

    parser.feed_line("out_time_us=25000000");
    let capped = parser.feed_line("progress=continue").unwrap();
    assert!((capped.percent - 100.0).abs() < f64::EPSILON);
}

#[test]
fn the_clock_time_field_backs_up_a_missing_microsecond_field() {
    let mut parser = ProgressParser::new(Duration::from_secs(8));
    let updates = feed(
        &mut parser,
        &["out_time=00:00:02.000000", "progress=continue"],
    );
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0].out_time, Duration::from_secs(2));
    assert!((updates[0].percent - 25.0).abs() < 0.01);
}

#[test]
fn a_block_without_any_time_field_reports_the_last_known_position() {
    let mut parser = ProgressParser::new(Duration::from_secs(10));
    parser.feed_line("out_time_us=4000000");
    parser.feed_line("progress=continue");
    let repeat = parser.feed_line("progress=continue").unwrap();
    assert!((repeat.percent - 40.0).abs() < 0.01);
    assert_eq!(repeat.out_time, Duration::from_micros(4_000_000));
}

#[test]
fn a_zero_total_stays_at_zero_until_the_end_block() {
    let mut parser = ProgressParser::new(Duration::ZERO);
    parser.feed_line("out_time_us=1000000");
    let running = parser.feed_line("progress=continue").unwrap();
    assert!(running.percent.abs() < f64::EPSILON);
    let done = parser.feed_line("progress=end").unwrap();
    assert!((done.percent - 100.0).abs() < f64::EPSILON);
}

#[test]
fn malformed_lines_are_ignored() {
    let mut parser = ProgressParser::new(Duration::from_secs(10));
    assert!(parser.feed_line("garbage").is_none());
    assert!(parser.feed_line("out_time_us=not-a-number").is_none());
    assert!(parser.feed_line("out_time=zzz").is_none());
    let update = parser.feed_line("progress=continue").unwrap();
    assert!(update.percent.abs() < f64::EPSILON);
}
