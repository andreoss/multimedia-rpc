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
}

impl Parameter {
    pub fn name(&self) -> &'static str {
        self.name
    }

    pub fn kind(&self) -> ParameterType {
        self.kind
    }

    pub fn is_required(&self) -> bool {
        self.required
    }
}

const SPEC: Parameter = Parameter {
    name: "spec",
    kind: ParameterType::Base64Spec,
    required: true,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    id: &'static str,
    parameters: &'static [Parameter],
    enabled: bool,
}

impl Command {
    pub fn id(&self) -> &'static str {
        self.id
    }

    pub fn parameters(&self) -> &'static [Parameter] {
        self.parameters
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
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
        .map(|kind| Command {
            id: kind_name(kind),
            parameters: &[SPEC],
            enabled: kind.ensure_executable().is_ok(),
        })
        .collect()
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
