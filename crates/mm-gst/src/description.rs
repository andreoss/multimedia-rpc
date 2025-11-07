#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Property {
    Flag(bool),
    Number(u64),
    Milliseconds(u64),
    Text(String),
    Element(String),
}

impl Property {
    pub fn text(&self) -> String {
        match self {
            Property::Flag(value) => value.to_string(),
            Property::Number(value) => value.to_string(),
            Property::Milliseconds(value) => value.to_string(),
            Property::Text(value) => value.clone(),
            Property::Element(value) => value.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Element {
    pub factory: String,
    pub name: String,
    pub properties: Vec<(String, Property)>,
}

impl Element {
    pub fn new(factory: &str, name: &str) -> Self {
        Self {
            factory: factory.to_string(),
            name: name.to_string(),
            properties: Vec::new(),
        }
    }

    pub fn with(mut self, key: &str, value: Property) -> Self {
        self.properties.push((key.to_string(), value));
        self
    }

    pub fn text(self, key: &str, value: impl Into<String>) -> Self {
        self.with(key, Property::Text(value.into()))
    }

    pub fn number(self, key: &str, value: u64) -> Self {
        self.with(key, Property::Number(value))
    }

    pub fn milliseconds(self, key: &str, value: u64) -> Self {
        self.with(key, Property::Milliseconds(value))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    Direct {
        from: String,
        from_pad: Option<String>,
        to: String,
        to_pad: Option<String>,
    },
    RequestPad {
        from: String,
        from_pad: Option<String>,
        to: String,
        request_pad: String,
    },
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DescriptionError {
    #[error("the description has no elements")]
    Empty,
    #[error("element name {name:?} is used twice")]
    DuplicateName { name: String },
    #[error("link refers to element {name:?}, which the description does not hold")]
    UnknownElement { name: String },
    #[error("a link needs two different elements")]
    SelfLink { name: String },
    #[error("element {name:?} has no factory")]
    MissingFactory { name: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipelineDescription {
    elements: Vec<Element>,
    links: Vec<Link>,
}

impl PipelineDescription {
    pub fn new(elements: Vec<Element>, links: Vec<Link>) -> Result<Self, DescriptionError> {
        if elements.is_empty() {
            return Err(DescriptionError::Empty);
        }
        let mut names: Vec<&str> = Vec::with_capacity(elements.len());
        for element in &elements {
            if element.factory.is_empty() {
                return Err(DescriptionError::MissingFactory {
                    name: element.name.clone(),
                });
            }
            if names.contains(&element.name.as_str()) {
                return Err(DescriptionError::DuplicateName {
                    name: element.name.clone(),
                });
            }
            names.push(&element.name);
        }
        for link in &links {
            for name in endpoints(link) {
                if !names.iter().any(|held| **held == name) {
                    return Err(DescriptionError::UnknownElement { name });
                }
            }
            if link_ends(link).0 == link_ends(link).1 && request_pad(link).is_none() {
                return Err(DescriptionError::SelfLink {
                    name: link_ends(link).0.to_string(),
                });
            }
        }
        Ok(Self { elements, links })
    }

    pub fn elements(&self) -> &[Element] {
        &self.elements
    }

    pub fn links(&self) -> &[Link] {
        &self.links
    }

    pub fn element(&self, name: &str) -> Option<&Element> {
        self.elements.iter().find(|element| element.name == name)
    }

    pub fn launcher_description(&self) -> String {
        self.launcher_arguments().join(" ")
    }

    pub fn launcher_arguments(&self) -> Vec<String> {
        let mut parts: Vec<String> = Vec::new();
        let mut emitted: Vec<&str> = Vec::new();
        for link in &self.links {
            let (from, from_pad, to, to_pad) = match link {
                Link::Direct {
                    from,
                    from_pad,
                    to,
                    to_pad,
                } => (
                    from.as_str(),
                    from_pad.as_deref(),
                    to.as_str(),
                    to_pad.as_deref(),
                ),
                Link::RequestPad {
                    from,
                    from_pad,
                    to,
                    request_pad,
                } => (
                    from.as_str(),
                    from_pad.as_deref(),
                    to.as_str(),
                    Some(request_pad.as_str()),
                ),
            };
            if !emitted.contains(&from)
                && let Some(element) = self.element(from)
            {
                parts.push(render(element));
                emitted.push(from);
            }
            if let Some(pad) = from_pad {
                parts.push(format!("{from}.{pad}"));
            }
            parts.push("!".to_string());
            if let Some(element) = self.element(to) {
                parts.push(render(element));
                emitted.push(to);
            }
            if let Some(pad) = to_pad {
                parts.push(format!("{to}.{pad}"));
            }
        }
        parts
    }
}

fn link_ends(link: &Link) -> (&str, &str) {
    match link {
        Link::Direct { from, to, .. } | Link::RequestPad { from, to, .. } => (from, to),
    }
}

fn request_pad(link: &Link) -> Option<&str> {
    match link {
        Link::RequestPad { request_pad, .. } => Some(request_pad),
        Link::Direct { .. } => None,
    }
}

fn endpoints(link: &Link) -> Vec<String> {
    match link {
        Link::Direct { from, to, .. } | Link::RequestPad { from, to, .. } => {
            vec![from.clone(), to.clone()]
        }
    }
}
fn render(element: &Element) -> String {
    let mut parts = vec![format!("{} name={}", element.factory, element.name)];
    for (key, value) in &element.properties {
        parts.push(format!("{key}={}", value.text()));
    }
    parts.join(" ")
}
