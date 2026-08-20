use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use mm_domain::{OpKind, OpSpec, OpSpecError};
use prost::Message;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParameterType {
    Base64Spec,
}

impl std::fmt::Display for ParameterType {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("base64")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Parameter {
    name: &'static str,
    kind: ParameterType,
    required: bool,
    description: &'static str,
    bounds: Option<&'static str>,
}

impl Parameter {
    pub const fn new(
        name: &'static str,
        kind: ParameterType,
        required: bool,
        description: &'static str,
        bounds: Option<&'static str>,
    ) -> Self {
        Self {
            name,
            kind,
            required,
            description,
            bounds,
        }
    }

    pub fn name(&self) -> &'static str {
        self.name
    }

    pub fn kind(&self) -> ParameterType {
        self.kind
    }

    pub fn is_required(&self) -> bool {
        self.required
    }

    pub fn description(&self) -> &'static str {
        self.description
    }

    pub fn bounds(&self) -> Option<&'static str> {
        self.bounds
    }
}

const SPEC: Parameter = Parameter::new(
    "spec",
    ParameterType::Base64Spec,
    true,
    "base64-encoded OpSpec protobuf",
    None,
);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    id: &'static str,
    label: &'static str,
    description: &'static str,
    parameters: &'static [Parameter],
    enabled: bool,
    disabled_reason: Option<&'static str>,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MetadataError {
    #[error("command {id} has no {field}")]
    EmptyField { id: String, field: &'static str },
    #[error("command {id} is disabled without a reason")]
    DisabledWithoutReason { id: String },
    #[error("command {id} is enabled but carries a disabled reason")]
    ReasonOnEnabled { id: String },
}

fn validate_parts(
    id: &str,
    label: &str,
    description: &str,
    parameters: &[Parameter],
    enabled: bool,
    disabled_reason: Option<&str>,
) -> Result<(), MetadataError> {
    for (field, value) in [("id", id), ("label", label), ("description", description)] {
        if value.is_empty() {
            return Err(MetadataError::EmptyField {
                id: id.to_string(),
                field,
            });
        }
    }
    for parameter in parameters {
        if parameter.name.is_empty() {
            return Err(MetadataError::EmptyField {
                id: id.to_string(),
                field: "parameter name",
            });
        }
        if parameter.description.is_empty() {
            return Err(MetadataError::EmptyField {
                id: id.to_string(),
                field: "parameter description",
            });
        }
    }
    match (enabled, disabled_reason) {
        (false, None) => Err(MetadataError::DisabledWithoutReason { id: id.to_string() }),
        (true, Some(_)) => Err(MetadataError::ReasonOnEnabled { id: id.to_string() }),
        _ => Ok(()),
    }
}

impl Command {
    pub fn from_parts(
        id: &'static str,
        label: &'static str,
        description: &'static str,
        parameters: &'static [Parameter],
        enabled: bool,
        disabled_reason: Option<&'static str>,
    ) -> Result<Self, MetadataError> {
        validate_parts(id, label, description, parameters, enabled, disabled_reason)?;
        Ok(Self {
            id,
            label,
            description,
            parameters,
            enabled,
            disabled_reason,
        })
    }

    pub fn id(&self) -> &'static str {
        self.id
    }

    pub fn label(&self) -> &'static str {
        self.label
    }

    pub fn description(&self) -> &'static str {
        self.description
    }

    pub fn parameters(&self) -> &'static [Parameter] {
        self.parameters
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn disabled_reason(&self) -> Option<&'static str> {
        self.disabled_reason
    }

    pub fn validate_metadata(&self) -> Result<(), MetadataError> {
        validate_parts(
            self.id,
            self.label,
            self.description,
            self.parameters,
            self.enabled,
            self.disabled_reason,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CommandError {
    #[error("no command named {id:?}")]
    UnknownCommand { id: String },
    #[error("command {id} is not enabled")]
    Disabled { id: String },
    #[error("command {id} takes no parameter named {name:?}")]
    UnknownParameter { id: String, name: String },
    #[error("command {id} needs parameter {name:?}")]
    MissingParameter { id: String, name: &'static str },
    #[error("parameter {name:?} is repeated")]
    RepeatedParameter { name: String },
    #[error("parameter spec is not valid base64: {message}")]
    InvalidBase64 { message: String },
    #[error("parameter spec is not a contract spec: {message}")]
    InvalidSpec { message: String },
    #[error("command {id} received a {found} spec")]
    WrongArm { id: String, found: String },
    #[error("parameter {token:?} is not name=value")]
    MalformedParameter { token: String },
}

pub fn registry() -> Vec<Command> {
    OpKind::ALL
        .into_iter()
        .map(|kind| {
            let (label, description) = metadata(kind);
            Command::from_parts(
                kind_name(kind),
                label,
                description,
                &[SPEC],
                kind.ensure_executable().is_ok(),
                None,
            )
            .expect("command metadata is complete")
        })
        .collect()
}

fn metadata(kind: OpKind) -> (&'static str, &'static str) {
    match kind {
        OpKind::Concat => ("Concat", "Join two or more inputs into one output"),
        OpKind::Probe => ("Probe", "Read container and stream metadata"),
        OpKind::Transcode => ("Transcode", "Encode one input to a profile"),
        OpKind::Trim => ("Trim", "Cut a range from one input"),
        OpKind::Thumbnail => ("Thumbnail", "Extract frames or a sprite"),
        OpKind::Resize => ("Resize", "Resize a still image"),
        OpKind::Convert => ("Convert", "Convert a still image format"),
        OpKind::Composite => ("Composite", "Composite still images"),
        OpKind::Package => ("Package", "Package one rendition as HLS"),
        OpKind::Batch => ("Batch", "Run steps in order"),
        OpKind::Graph => ("Graph", "Run nodes in dependency order"),
        OpKind::Ladder => ("Ladder", "Encode and package ordered renditions"),
        OpKind::Loudness => ("Loudness", "Measure or normalise loudness"),
        OpKind::Subtitle => ("Subtitle", "Burn, mux or strip captions"),
        OpKind::Grade => ("Grade", "Adjust colour and optionally land a scope"),
        OpKind::Audio => ("Audio", "Process and mix audio"),
        OpKind::Title => ("Title", "Render text as a still"),
    }
}

fn kind_name(kind: OpKind) -> &'static str {
    match kind {
        OpKind::Concat => "concat",
        OpKind::Probe => "probe",
        OpKind::Transcode => "transcode",
        OpKind::Trim => "trim",
        OpKind::Thumbnail => "thumbnail",
        OpKind::Resize => "resize",
        OpKind::Convert => "convert",
        OpKind::Composite => "composite",
        OpKind::Package => "package",
        OpKind::Batch => "batch",
        OpKind::Graph => "graph",
        OpKind::Ladder => "ladder",
        OpKind::Loudness => "loudness",
        OpKind::Subtitle => "subtitle",
        OpKind::Grade => "grade",
        OpKind::Audio => "audio",
        OpKind::Title => "title",
    }
}

pub fn render(command: &Command) -> String {
    let parameters: Vec<String> = command
        .parameters
        .iter()
        .map(|parameter| {
            format!(
                "{}:{}:{}",
                parameter.name,
                parameter.kind,
                if parameter.required {
                    "required"
                } else {
                    "optional"
                }
            )
        })
        .collect();
    format!(
        "{} {} {}",
        command.id,
        parameters.join(","),
        if command.enabled {
            "enabled"
        } else {
            "disabled"
        }
    )
}

pub fn lines() -> Vec<String> {
    registry().iter().map(render).collect()
}

pub fn describe(id: &str) -> Result<Command, CommandError> {
    registry()
        .into_iter()
        .find(|command| command.id == id)
        .ok_or_else(|| CommandError::UnknownCommand { id: id.to_string() })
}

pub fn render_description(command: &Command) -> String {
    let mut lines = vec![
        format!("id: {}", command.id),
        format!("label: {}", command.label),
        format!("description: {}", command.description),
        format!("enabled: {}", command.enabled),
        format!(
            "disabled-reason: {}",
            command.disabled_reason.unwrap_or("none")
        ),
    ];
    for parameter in command.parameters {
        lines.push(format!("parameter: {}", parameter.name));
        lines.push(format!("  type: {}", parameter.kind));
        lines.push(format!("  required: {}", parameter.required));
        lines.push(format!("  description: {}", parameter.description));
        lines.push(format!("  bounds: {}", parameter.bounds.unwrap_or("none")));
    }
    lines.join("\n")
}

pub fn parse_line(line: &str) -> Result<OpSpec, CommandError> {
    let mut tokens = line.split_whitespace();
    let id = tokens
        .next()
        .ok_or_else(|| CommandError::UnknownCommand { id: String::new() })?;
    let command = registry()
        .into_iter()
        .find(|command| command.id == id)
        .ok_or_else(|| CommandError::UnknownCommand { id: id.to_string() })?;
    if !command.enabled {
        return Err(CommandError::Disabled { id: id.to_string() });
    }
    let mut spec = None;
    for token in tokens {
        let (name, value) =
            token
                .split_once('=')
                .ok_or_else(|| CommandError::MalformedParameter {
                    token: token.to_string(),
                })?;
        if name != "spec" {
            return Err(CommandError::UnknownParameter {
                id: id.to_string(),
                name: name.to_string(),
            });
        }
        if spec.is_some() {
            return Err(CommandError::RepeatedParameter {
                name: name.to_string(),
            });
        }
        spec = Some(value);
    }
    let value = spec.ok_or_else(|| CommandError::MissingParameter {
        id: id.to_string(),
        name: SPEC.name,
    })?;
    let bytes = BASE64_STANDARD
        .decode(value)
        .map_err(|error| CommandError::InvalidBase64 {
            message: error.to_string(),
        })?;
    let wire = mm_proto::v1::OpSpec::decode(bytes.as_slice()).map_err(|error| {
        CommandError::InvalidSpec {
            message: error.to_string(),
        }
    })?;
    let spec = OpSpec::try_from(wire).map_err(|error: OpSpecError| CommandError::InvalidSpec {
        message: error.to_string(),
    })?;
    let found = spec.kind().to_string();
    if found != id {
        return Err(CommandError::WrongArm {
            id: id.to_string(),
            found,
        });
    }
    Ok(spec)
}
