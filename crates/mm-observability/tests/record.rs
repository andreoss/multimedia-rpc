use mm_domain::TraceId;
use mm_observability::{Component, format_line, write_record};

#[test]
fn a_record_without_a_trace_still_names_its_component_and_event() {
    let line = format_line(Component::Server, None, "server.started", &[]);
    assert!(line.starts_with("trace=none "), "{line}");
    assert!(line.contains("component=server"), "{line}");
    assert!(line.contains("event=server.started"), "{line}");
}

#[test]
fn a_trace_identifier_is_carried_into_the_record() {
    let trace = TraceId::generate();
    let line = format_line(Component::Worker, Some(&trace), "job.finished", &[]);
    assert!(line.contains(&format!("trace={trace}")), "{line}");
    assert!(line.contains("component=worker"), "{line}");
}

#[test]
fn a_value_with_a_space_stays_inside_its_field() {
    let trace = TraceId::generate();
    let line = format_line(
        Component::Server,
        Some(&trace),
        "job.failed",
        &[("error", "exit status 1: invalid data found".to_string())],
    );
    assert!(
        line.contains(r#"error="exit status 1: invalid data found""#),
        "{line}"
    );
}

#[test]
fn a_record_is_exactly_one_line() {
    let trace = TraceId::generate();
    let line = format_line(
        Component::Worker,
        Some(&trace),
        "job.progress",
        &[
            ("percent", "42".to_string()),
            ("media", "a b c".to_string()),
        ],
    );
    assert_eq!(line.matches('\n').count(), 1, "{line}");
    assert!(line.ends_with('\n'));
}

#[test]
fn a_component_renders_as_the_name_it_is_named_by() {
    assert_eq!(Component::Server.name(), "server");
    assert_eq!(Component::Worker.name(), "worker");
    assert_eq!(Component::Daemon.name(), "daemon");
}

#[test]
fn a_written_record_is_the_formatted_record() {
    let trace = TraceId::generate();
    let fields = [("job", "job-1".to_string())];
    let mut sink: Vec<u8> = Vec::new();
    write_record(
        &mut sink,
        Component::Server,
        Some(&trace),
        "job.created",
        &fields,
    );
    let written = String::from_utf8(sink).expect("utf8");
    assert_eq!(
        written,
        format_line(Component::Server, Some(&trace), "job.created", &fields)
    );
}

#[test]
fn a_record_is_written_even_when_the_sink_is_gone() {
    struct Closed;

    impl std::io::Write for Closed {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
        }
    }

    let trace = TraceId::generate();
    write_record(
        &mut Closed,
        Component::Worker,
        Some(&trace),
        "job.progress",
        &[],
    );
}
