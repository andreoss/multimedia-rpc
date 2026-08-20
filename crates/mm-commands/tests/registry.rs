use mm_commands::{CommandError, lines, parse_line, registry};
use mm_domain::{InputSource, MediaRef, OpKind, OpSpec, ProbeSpec};
use mm_proto::v1;
use prost::Message;

fn encode(spec: &OpSpec) -> String {
    let wire = v1::OpSpec::from(spec).encode_to_vec();
    base64::Engine::encode(&base64::engine::general_purpose::STANDARD, wire)
}

#[test]
fn the_registry_has_one_enabled_command_for_every_arm() {
    let commands = registry();
    assert_eq!(commands.len(), OpKind::ALL.len());
    for (command, kind) in commands.iter().zip(OpKind::ALL) {
        assert_eq!(command.id(), kind.to_string());
        assert!(command.is_enabled(), "{} is disabled", command.id());
        assert_eq!(command.parameters().len(), 1);
        assert_eq!(command.parameters()[0].name(), "spec");
        assert!(command.parameters()[0].is_required());
    }
    assert_eq!(lines().len(), 17);
}

#[test]
fn a_line_round_trips_a_typed_spec() {
    let spec = OpSpec::Probe(
        ProbeSpec::new(MediaRef::object_uri("s3://bucket/input.mp4").unwrap()).unwrap(),
    );
    let decoded = parse_line(&format!("probe spec={}", encode(&spec))).unwrap();
    assert_eq!(decoded, spec);
}

#[test]
fn a_refusal_names_the_parameter() {
    assert!(matches!(
        parse_line("probe"),
        Err(CommandError::MissingParameter { name: "spec", .. })
    ));
    assert!(matches!(
        parse_line("probe other=value"),
        Err(CommandError::UnknownParameter { name, .. }) if name == "other"
    ));
    assert!(matches!(
        parse_line("probe spec=!"),
        Err(CommandError::InvalidBase64 { .. })
    ));
    assert!(matches!(
        parse_line("probe spec=not-proto"),
        Err(CommandError::InvalidBase64 { .. }) | Err(CommandError::InvalidSpec { .. })
    ));
}

#[test]
fn a_line_naming_another_arm_is_refused() {
    let spec = OpSpec::Probe(
        ProbeSpec::new(InputSource::from(
            MediaRef::object_uri("s3://bucket/input.mp4").unwrap(),
        ))
        .unwrap(),
    );
    assert!(matches!(
        parse_line(&format!("transcode spec={}", encode(&spec))),
        Err(CommandError::WrongArm { .. })
    ));
}

#[test]
fn every_command_publishes_complete_metadata() {
    let commands = registry();
    assert_eq!(commands.len(), OpKind::ALL.len());
    for command in &commands {
        assert!(!command.label().is_empty(), "{} has no label", command.id());
        assert!(
            !command.description().is_empty(),
            "{} has no description",
            command.id()
        );
        command.validate_metadata().unwrap();
        for parameter in command.parameters() {
            assert!(!parameter.name().is_empty());
            assert!(
                !parameter.description().is_empty(),
                "{} parameter {} has no description",
                command.id(),
                parameter.name()
            );
        }
        if command.is_enabled() {
            assert!(command.disabled_reason().is_none());
        } else {
            assert!(command.disabled_reason().is_some());
        }
    }
}

#[test]
fn incomplete_metadata_is_refused() {
    use mm_commands::{Command, MetadataError, Parameter, ParameterType};

    static EMPTY_DESCRIPTION: Parameter =
        Parameter::new("spec", ParameterType::Base64Spec, true, "", None);
    static EMPTY_PARAMETERS: [Parameter; 1] = [EMPTY_DESCRIPTION];

    assert!(matches!(
        Command::from_parts("probe", "", "Read metadata", &[], true, None),
        Err(MetadataError::EmptyField { field: "label", .. })
    ));
    assert!(matches!(
        Command::from_parts("probe", "Probe", "", &[], true, None),
        Err(MetadataError::EmptyField {
            field: "description",
            ..
        })
    ));
    assert!(matches!(
        Command::from_parts(
            "probe",
            "Probe",
            "Read metadata",
            &EMPTY_PARAMETERS,
            true,
            None
        ),
        Err(MetadataError::EmptyField {
            field: "parameter description",
            ..
        })
    ));
    assert!(matches!(
        Command::from_parts("probe", "Probe", "Read metadata", &[], false, None),
        Err(MetadataError::DisabledWithoutReason { .. })
    ));
    assert!(matches!(
        Command::from_parts(
            "probe",
            "Probe",
            "Read metadata",
            &[],
            true,
            Some("not ready")
        ),
        Err(MetadataError::ReasonOnEnabled { .. })
    ));
}
