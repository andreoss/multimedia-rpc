use std::io::Write;

use mm_domain::TraceId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Component {
    Daemon,
    Server,
    Worker,
}

impl Component {
    pub fn name(self) -> &'static str {
        match self {
            Component::Daemon => "daemon",
            Component::Server => "server",
            Component::Worker => "worker",
        }
    }
}

pub fn format_line(
    component: Component,
    trace: Option<&TraceId>,
    event: &str,
    fields: &[(&str, String)],
) -> String {
    let trace = trace
        .map(TraceId::to_string)
        .unwrap_or_else(|| "none".to_string());
    let mut line = format!("trace={trace} component={} event={event}", component.name());
    for (name, value) in fields {
        line.push(' ');
        line.push_str(name);
        line.push('=');
        line.push_str(&quote(value));
    }
    line.push('\n');
    line
}

fn quote(value: &str) -> String {
    let plain = !value.is_empty() && value.chars().all(|character| character.is_ascii_graphic());
    if plain {
        return value.to_string();
    }
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

pub fn write_record<W: Write>(
    sink: &mut W,
    component: Component,
    trace: Option<&TraceId>,
    event: &str,
    fields: &[(&str, String)],
) {
    let line = format_line(component, trace, event, fields);
    let _ = sink.write_all(line.as_bytes());
    let _ = sink.flush();
}

pub fn record(
    component: Component,
    trace: Option<&TraceId>,
    event: &str,
    fields: &[(&str, String)],
) {
    let mut stderr = std::io::stderr().lock();
    write_record(&mut stderr, component, trace, event, fields);
}

pub fn record_to<W: Write>(
    sink: &mut W,
    component: Component,
    trace: Option<&TraceId>,
    event: &str,
    fields: &[(&str, String)],
) {
    write_record(sink, component, trace, event, fields);
}
