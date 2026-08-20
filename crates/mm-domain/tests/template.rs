use std::str::FromStr;

use mm_domain::{
    InputSource, MediaRef, OpSpec, OutputProfile, ParameterDeclaration, ParameterName,
    ParameterType, ParameterValue, ProbeSpec, Template, TemplateError, TranscodeSpec, UserId,
    render,
};

fn owner() -> UserId {
    UserId::from_str("owner-1").unwrap()
}

fn profile() -> OutputProfile {
    OutputProfile::from_raw(24.0, 0, 0, 0, 48_000, 640, 360).unwrap()
}

fn slot(name: &str) -> InputSource {
    InputSource::from(ParameterName::from_str(name).unwrap())
}

fn transcode_reading(name: &str) -> OpSpec {
    OpSpec::Transcode(TranscodeSpec::new(slot(name), profile()).unwrap())
}

fn declaration(name: &str) -> ParameterDeclaration {
    ParameterDeclaration::new(name, ParameterType::Uri).unwrap()
}

#[test]
fn a_template_binds_a_declared_parameter_to_its_slots() {
    let template = Template::new(
        owner(),
        "recipe",
        transcode_reading("source"),
        vec![declaration("source")],
    )
    .expect("a bound template");
    assert_eq!(template.name().as_str(), "recipe");
    assert_eq!(template.parameters().len(), 1);
}

#[test]
fn a_declared_name_bound_to_no_slot_is_refused() {
    let spec =
        OpSpec::Probe(ProbeSpec::new(MediaRef::object_uri("s3://bucket/key").unwrap()).unwrap());
    let error = Template::new(owner(), "recipe", spec, vec![declaration("missing")]).unwrap_err();
    assert_eq!(
        error,
        TemplateError::UnboundParameter {
            name: "missing".to_string()
        }
    );
}

#[test]
fn an_undeclared_slot_is_refused() {
    let error = Template::new(owner(), "recipe", transcode_reading("source"), vec![]).unwrap_err();
    assert_eq!(
        error,
        TemplateError::UndeclaredParameter {
            name: "source".to_string()
        }
    );
}

#[test]
fn a_render_fills_every_slot() {
    let template = Template::new(
        owner(),
        "recipe",
        transcode_reading("source"),
        vec![declaration("source")],
    )
    .unwrap();
    let media = MediaRef::object_uri("s3://bucket/in.mp4").unwrap();
    let rendered = render(
        &template,
        &[(
            ParameterName::from_str("source").unwrap(),
            ParameterValue::Uri(media.clone()),
        )],
    )
    .unwrap();
    let OpSpec::Transcode(transcode) = rendered else {
        panic!("expected a transcode spec");
    };
    assert_eq!(
        transcode.input().media().unwrap().media(),
        &media,
        "the slot was not filled"
    );
}

#[test]
fn a_wrong_type_is_refused_naming_the_parameter() {
    let template = Template::new(
        owner(),
        "recipe",
        transcode_reading("source"),
        vec![declaration("source")],
    )
    .unwrap();
    let error = render(
        &template,
        &[(
            ParameterName::from_str("source").unwrap(),
            ParameterValue::Text("not a uri".to_string()),
        )],
    )
    .unwrap_err();
    assert!(matches!(error, TemplateError::WrongParameterType { .. }));
    assert!(error.to_string().contains("source"), "{error}");
}
