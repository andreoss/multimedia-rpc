use std::collections::HashSet;

use thiserror::Error;

use crate::id::{
    ParameterName, ParameterNameError, TemplateId, TemplateName, TemplateNameError, UserId,
};
use crate::op::{
    BatchSpec, ConcatSpec, GraphNode, GraphSpec, ImageSpec, OpSpec, OpSpecError, PackageSpec,
    ProbeSpec, ThumbnailSpec, TranscodeSpec, TrimSpec,
};
use crate::source::InputSource;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParameterType {
    Uri,
    Text,
}

impl ParameterType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Uri => "uri",
            Self::Text => "text",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ParameterValue {
    Uri(crate::op::MediaRef),
    Text(String),
}

impl ParameterValue {
    pub fn kind(&self) -> ParameterType {
        match self {
            Self::Uri(_) => ParameterType::Uri,
            Self::Text(_) => ParameterType::Text,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Error)]
pub enum TemplateError {
    #[error("invalid template name {name:?}: {source}")]
    InvalidName {
        name: String,
        #[source]
        source: TemplateNameError,
    },
    #[error("invalid parameter name {name:?}: {source}")]
    InvalidParameterName {
        name: String,
        #[source]
        source: ParameterNameError,
    },
    #[error("template declares the parameter {name:?} twice")]
    DuplicateParameter { name: String },
    #[error("parameter {name:?} is declared but bound to no slot")]
    UnboundParameter { name: String },
    #[error("a slot names the parameter {name:?}, which is not declared")]
    UndeclaredParameter { name: String },
    #[error("parameter {name:?} is {expected}, but the value is {got}")]
    WrongParameterType {
        name: String,
        expected: &'static str,
        got: &'static str,
    },
    #[error("parameter {name:?} has no value")]
    MissingParameter { name: String },
    #[error("a value was supplied for the unknown parameter {name:?}")]
    UnknownParameter { name: String },
    #[error("unknown parameter type {value}")]
    UnknownParameterType { value: i32 },
    #[error("a parameter binding carries no value")]
    MissingValue,
    #[error(transparent)]
    Spec(#[from] OpSpecError),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParameterDeclaration {
    name: ParameterName,
    kind: ParameterType,
}

impl ParameterDeclaration {
    pub fn new(name: &str, kind: ParameterType) -> Result<Self, TemplateError> {
        let name = name.parse::<ParameterName>().map_err(|source| {
            TemplateError::InvalidParameterName {
                name: name.to_string(),
                source,
            }
        })?;
        Ok(Self { name, kind })
    }

    pub fn from_name(name: ParameterName, kind: ParameterType) -> Self {
        Self { name, kind }
    }

    pub fn name(&self) -> &ParameterName {
        &self.name
    }

    pub fn kind(&self) -> ParameterType {
        self.kind
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Template {
    id: TemplateId,
    owner: UserId,
    name: TemplateName,
    spec: OpSpec,
    parameters: Vec<ParameterDeclaration>,
}

impl Template {
    pub fn new(
        owner: UserId,
        name: &str,
        spec: OpSpec,
        parameters: Vec<ParameterDeclaration>,
    ) -> Result<Self, TemplateError> {
        Self::from_parts(TemplateId::generate(), owner, name, spec, parameters)
    }

    pub fn from_parts(
        id: TemplateId,
        owner: UserId,
        name: &str,
        spec: OpSpec,
        parameters: Vec<ParameterDeclaration>,
    ) -> Result<Self, TemplateError> {
        let name = name
            .parse::<TemplateName>()
            .map_err(|source| TemplateError::InvalidName {
                name: name.to_string(),
                source,
            })?;
        let template = Self {
            id,
            owner,
            name,
            spec,
            parameters,
        };
        template.validate()?;
        Ok(template)
    }

    pub fn id(&self) -> TemplateId {
        self.id
    }

    pub fn owner(&self) -> &UserId {
        &self.owner
    }

    pub fn name(&self) -> &TemplateName {
        &self.name
    }

    pub fn spec(&self) -> &OpSpec {
        &self.spec
    }

    pub fn parameters(&self) -> &[ParameterDeclaration] {
        &self.parameters
    }

    fn validate(&self) -> Result<(), TemplateError> {
        let mut declared: HashSet<&str> = HashSet::new();
        for declaration in &self.parameters {
            if !declared.insert(declaration.name.as_str()) {
                return Err(TemplateError::DuplicateParameter {
                    name: declaration.name.as_str().to_string(),
                });
            }
            if declaration.kind != ParameterType::Uri {
                return Err(TemplateError::UnboundParameter {
                    name: declaration.name.as_str().to_string(),
                });
            }
        }
        let used: Vec<&ParameterName> = self
            .spec
            .inputs()
            .into_iter()
            .filter_map(InputSource::parameter)
            .collect();
        for name in &used {
            if !declared.contains(name.as_str()) {
                return Err(TemplateError::UndeclaredParameter {
                    name: name.as_str().to_string(),
                });
            }
        }
        let used_names: HashSet<&str> = used.iter().map(|name| name.as_str()).collect();
        for declaration in &self.parameters {
            if !used_names.contains(declaration.name.as_str()) {
                return Err(TemplateError::UnboundParameter {
                    name: declaration.name.as_str().to_string(),
                });
            }
        }
        Ok(())
    }
}

pub fn render(
    template: &Template,
    values: &[(ParameterName, ParameterValue)],
) -> Result<OpSpec, TemplateError> {
    for (name, _) in values {
        if !template
            .parameters
            .iter()
            .any(|declaration| declaration.name == *name)
        {
            return Err(TemplateError::UnknownParameter {
                name: name.as_str().to_string(),
            });
        }
    }
    let mut resolved: Vec<(&ParameterName, crate::op::MediaRef)> = Vec::new();
    for declaration in &template.parameters {
        let value = values
            .iter()
            .find(|(name, _)| name == declaration.name())
            .map(|(_, value)| value)
            .ok_or_else(|| TemplateError::MissingParameter {
                name: declaration.name.as_str().to_string(),
            })?;
        if value.kind() != declaration.kind() {
            return Err(TemplateError::WrongParameterType {
                name: declaration.name.as_str().to_string(),
                expected: declaration.kind().as_str(),
                got: value.kind().as_str(),
            });
        }
        let ParameterValue::Uri(media) = value else {
            return Err(TemplateError::WrongParameterType {
                name: declaration.name.as_str().to_string(),
                expected: declaration.kind().as_str(),
                got: value.kind().as_str(),
            });
        };
        resolved.push((declaration.name(), media.clone()));
    }
    substitute_spec(&template.spec, &resolved)
}

fn substitute_input(
    input: &InputSource,
    values: &[(&ParameterName, crate::op::MediaRef)],
) -> Result<InputSource, TemplateError> {
    match input.parameter() {
        Some(name) => {
            let media = values
                .iter()
                .find(|(declared, _)| *declared == name)
                .map(|(_, media)| media.clone())
                .ok_or_else(|| TemplateError::MissingParameter {
                    name: name.as_str().to_string(),
                })?;
            Ok(InputSource::from(media))
        }
        None => Ok(input.clone()),
    }
}

fn substitute_spec(
    spec: &OpSpec,
    values: &[(&ParameterName, crate::op::MediaRef)],
) -> Result<OpSpec, TemplateError> {
    Ok(match spec {
        OpSpec::Concat(concat) => {
            let inputs = concat
                .inputs()
                .iter()
                .map(|input| substitute_input(input, values))
                .collect::<Result<Vec<_>, _>>()?;
            OpSpec::Concat(ConcatSpec::new(inputs, concat.profile().cloned())?)
        }
        OpSpec::Probe(probe) => {
            OpSpec::Probe(ProbeSpec::new(substitute_input(probe.input(), values)?)?)
        }
        OpSpec::Transcode(transcode) => OpSpec::Transcode(TranscodeSpec::new(
            substitute_input(transcode.input(), values)?,
            *transcode.profile(),
        )?),
        OpSpec::Trim(trim) => OpSpec::Trim(TrimSpec::new(
            substitute_input(trim.input(), values)?,
            trim.start_seconds(),
            trim.duration_seconds(),
        )?),
        OpSpec::Thumbnail(thumbnail) => OpSpec::Thumbnail(ThumbnailSpec::new(
            substitute_input(thumbnail.input(), values)?,
            thumbnail.offsets_seconds().to_vec(),
            thumbnail.width(),
            thumbnail.height(),
        )?),
        OpSpec::Image(image) => {
            let inputs = image
                .inputs()
                .iter()
                .map(|input| substitute_input(input, values))
                .collect::<Result<Vec<_>, _>>()?;
            OpSpec::Image(ImageSpec::new(
                inputs,
                image.op(),
                image.width(),
                image.height(),
                image.mode(),
                image.declared_format(),
            )?)
        }
        OpSpec::Package(package) => OpSpec::Package(PackageSpec::new(
            substitute_input(package.input(), values)?,
            package.segment_seconds(),
        )?),
        OpSpec::Batch(batch) => {
            let steps = batch
                .steps()
                .iter()
                .map(|step| substitute_spec(step, values))
                .collect::<Result<Vec<_>, _>>()?;
            OpSpec::Batch(BatchSpec::new(steps, batch.stop_on_failure())?)
        }
        OpSpec::Graph(graph) => {
            let nodes = graph
                .nodes()
                .iter()
                .map(|node| {
                    let spec = substitute_spec(node.spec(), values)?;
                    GraphNode::new(node.id().as_str(), spec).map_err(TemplateError::Spec)
                })
                .collect::<Result<Vec<_>, _>>()?;
            OpSpec::Graph(GraphSpec::new(nodes)?)
        }
    })
}
