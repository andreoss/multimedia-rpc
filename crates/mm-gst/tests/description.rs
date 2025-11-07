use mm_gst::{DescriptionError, Element, Link, PipelineDescription, Property};

fn packaging() -> PipelineDescription {
    PipelineDescription::new(
        vec![
            Element::new("filesrc", "source").text("location", "in.mp4"),
            Element::new("qtdemux", "demux"),
            Element::new("mp4mux", "mux").with("faststart", Property::Flag(true)),
            Element::new("splitmuxsink", "sink")
                .with("muxer", Property::Element("mux".to_string()))
                .text("location", "seg%05d.m4s"),
        ],
        vec![
            Link::Direct {
                from: "source".to_string(),
                from_pad: None,
                to: "demux".to_string(),
                to_pad: None,
            },
            Link::RequestPad {
                from: "demux".to_string(),
                from_pad: Some("video_0".to_string()),
                to: "sink".to_string(),
                request_pad: "video".to_string(),
            },
        ],
    )
    .expect("valid description")
}

#[test]
fn a_description_holds_its_elements_properties_and_links() {
    let description = packaging();

    assert_eq!(description.elements().len(), 4);
    assert_eq!(
        description
            .element("sink")
            .map(|element| element.properties.clone()),
        Some(vec![
            ("muxer".to_string(), Property::Element("mux".to_string())),
            (
                "location".to_string(),
                Property::Text("seg%05d.m4s".to_string())
            ),
        ])
    );
    assert_eq!(description.links().len(), 2);
}

#[test]
fn a_description_renders_the_launcher_syntax() {
    let rendered = packaging().launcher_description();

    assert!(
        rendered.starts_with("filesrc name=source location=in.mp4 !"),
        "{rendered}"
    );
    assert!(rendered.contains("qtdemux name=demux"), "{rendered}");
    assert!(
        rendered.contains("splitmuxsink name=sink muxer=mux location=seg%05d.m4s"),
        "{rendered}"
    );
    assert!(rendered.contains("demux.video_0 !"), "{rendered}");
    assert!(rendered.ends_with("sink.video"), "{rendered}");
}

#[test]
fn an_empty_description_is_rejected() {
    assert_eq!(
        PipelineDescription::new(Vec::new(), Vec::new()).unwrap_err(),
        DescriptionError::Empty
    );
}

#[test]
fn a_repeated_element_name_is_rejected() {
    let error = PipelineDescription::new(
        vec![
            Element::new("filesrc", "source"),
            Element::new("qtdemux", "source"),
        ],
        Vec::new(),
    )
    .unwrap_err();

    assert_eq!(
        error,
        DescriptionError::DuplicateName {
            name: "source".to_string()
        }
    );
}

#[test]
fn a_link_to_an_absent_element_is_rejected() {
    let error = PipelineDescription::new(
        vec![Element::new("filesrc", "source")],
        vec![Link::Direct {
            from: "source".to_string(),
            from_pad: None,
            to: "nowhere".to_string(),
            to_pad: None,
        }],
    )
    .unwrap_err();

    assert_eq!(
        error,
        DescriptionError::UnknownElement {
            name: "nowhere".to_string()
        }
    );
}

#[test]
fn an_element_without_a_factory_is_rejected() {
    let error = PipelineDescription::new(vec![Element::new("", "source")], Vec::new()).unwrap_err();

    assert_eq!(
        error,
        DescriptionError::MissingFactory {
            name: "source".to_string()
        }
    );
}
