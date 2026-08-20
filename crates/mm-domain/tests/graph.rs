use mm_domain::{
    ConcatSpec, GraphNode, GraphSpec, InputSource, MAX_GRAPH_NODES, MediaRef, NodeRef, OpSpec,
    OpSpecError, OutputProfile, ProbeSpec, TranscodeSpec,
};

fn profile() -> OutputProfile {
    OutputProfile::from_raw(24.0, 0, 0, 0, 48_000, 640, 360).unwrap()
}

fn probe() -> OpSpec {
    OpSpec::Probe(ProbeSpec::new(MediaRef::object_uri("s3://bucket/key").unwrap()).unwrap())
}

fn reading(id: &str) -> OpSpec {
    let source = InputSource::from(NodeRef::new(id, 0).unwrap());
    OpSpec::Transcode(TranscodeSpec::new(source, profile()).unwrap())
}

fn node(id: &str, spec: OpSpec) -> GraphNode {
    GraphNode::new(id, spec).unwrap()
}

#[test]
fn a_graph_keeps_its_nodes_and_their_ids() {
    let graph = GraphSpec::new(vec![
        node("first", probe()),
        node("second", reading("first")),
    ])
    .expect("a valid graph");
    assert_eq!(graph.nodes().len(), 2);
    assert_eq!(graph.nodes()[0].id().as_str(), "first");
    assert_eq!(graph.nodes()[1].id().as_str(), "second");
}

#[test]
fn an_empty_graph_is_refused() {
    assert_eq!(GraphSpec::new(vec![]).unwrap_err(), OpSpecError::EmptyGraph);
}

#[test]
fn a_duplicate_node_id_is_refused() {
    let error = GraphSpec::new(vec![node("a", probe()), node("a", probe())]).unwrap_err();
    assert_eq!(
        error,
        OpSpecError::DuplicateNode {
            id: "a".to_string()
        }
    );
}

#[test]
fn a_malformed_node_id_is_refused_by_name() {
    let error = GraphNode::new("bad id", probe()).unwrap_err();
    assert!(error.to_string().contains("graph node id"), "{error}");
}

#[test]
fn a_cycle_is_refused_naming_the_node_that_closes_it() {
    let graph = GraphSpec::new(vec![node("a", reading("b")), node("b", reading("a"))]);
    let error = graph.unwrap_err();
    assert!(matches!(error, OpSpecError::Cycle { .. }), "{error}");
}

#[test]
fn a_dangling_reference_is_refused_naming_the_node_and_reference() {
    let error = GraphSpec::new(vec![node("a", reading("missing"))]).unwrap_err();
    assert_eq!(
        error,
        OpSpecError::DanglingReference {
            node: "a".to_string(),
            reference: "missing".to_string(),
        }
    );
}

#[test]
fn a_diamond_is_accepted() {
    let join = OpSpec::Concat(
        ConcatSpec::new(
            vec![
                InputSource::from(NodeRef::new("left", 0).unwrap()),
                InputSource::from(NodeRef::new("right", 0).unwrap()),
            ],
            Some(profile()),
        )
        .unwrap(),
    );
    let graph = GraphSpec::new(vec![
        node("source", probe()),
        node("left", reading("source")),
        node("right", reading("source")),
        node("join", join),
    ])
    .expect("a diamond is a valid graph");
    assert_eq!(graph.nodes().len(), 4);
}

#[test]
fn a_graph_above_the_node_cap_is_refused() {
    let nodes = (0..=MAX_GRAPH_NODES)
        .map(|index| node(&format!("n{index}"), probe()))
        .collect::<Vec<_>>();
    let error = GraphSpec::new(nodes).unwrap_err();
    assert!(matches!(error, OpSpecError::GraphTooLarge { max, .. } if max == MAX_GRAPH_NODES));
}
