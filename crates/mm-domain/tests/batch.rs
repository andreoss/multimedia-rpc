use mm_domain::{
    BatchSpec, InputSource, MAX_BATCH_STEPS, MAX_NESTING_DEPTH, MediaRef, NodeRef, OpSpec,
    OpSpecError, OutputProfile, ProbeSpec, TranscodeSpec,
};

fn probe() -> OpSpec {
    OpSpec::Probe(ProbeSpec::new(MediaRef::object_uri("s3://bucket/key").unwrap()).unwrap())
}

fn profile() -> OutputProfile {
    OutputProfile::from_raw(24.0, 0, 0, 0, 48_000, 640, 360).unwrap()
}

fn batch(steps: Vec<OpSpec>) -> Result<OpSpec, OpSpecError> {
    BatchSpec::new(steps, true).map(OpSpec::Batch)
}

fn reading(node: &str) -> OpSpec {
    let source = InputSource::from(NodeRef::new(node, 0).unwrap());
    OpSpec::Transcode(TranscodeSpec::new(source, profile()).unwrap())
}

#[test]
fn a_batch_holds_its_steps_in_order() {
    let spec = BatchSpec::new(vec![probe(), probe()], false).unwrap();
    assert_eq!(spec.steps().len(), 2);
    assert!(!spec.stop_on_failure());
}

#[test]
fn an_empty_batch_is_refused_by_name() {
    assert_eq!(
        BatchSpec::new(vec![], true).unwrap_err(),
        OpSpecError::EmptyBatch
    );
}

#[test]
fn a_batch_above_the_step_cap_is_refused() {
    let steps = vec![probe(); MAX_BATCH_STEPS + 1];
    assert!(matches!(
        BatchSpec::new(steps, true),
        Err(OpSpecError::BatchTooLarge { max, .. }) if max == MAX_BATCH_STEPS
    ));
}

#[test]
fn a_nested_batch_is_accepted() {
    let inner = batch(vec![probe()]).unwrap();
    let outer = BatchSpec::new(vec![inner, probe()], true).unwrap();
    assert_eq!(outer.steps().len(), 2);
}

#[test]
fn nesting_past_the_depth_cap_is_refused() {
    let mut spec = batch(vec![probe()]).unwrap();
    for _ in 0..MAX_NESTING_DEPTH - 1 {
        spec = batch(vec![spec]).unwrap();
    }
    let error = batch(vec![spec]).unwrap_err();
    assert!(matches!(error, OpSpecError::NestingTooDeep { max, .. } if max == MAX_NESTING_DEPTH));
}

#[test]
fn a_step_may_read_an_earlier_step_by_position() {
    let spec = batch(vec![probe(), reading("0")]).unwrap();
    assert!(spec.ensure_resolvable().is_ok());
}

#[test]
fn a_step_may_not_read_itself_or_a_later_step() {
    let itself = batch(vec![probe(), reading("1")]).unwrap();
    assert_eq!(itself.ensure_resolvable().unwrap_err().node, "1");

    let later = batch(vec![reading("1"), probe()]).unwrap();
    assert_eq!(later.ensure_resolvable().unwrap_err().node, "1");
}

#[test]
fn a_reference_outside_the_tree_is_refused_naming_the_node() {
    let spec = batch(vec![probe(), reading("9")]).unwrap();
    assert_eq!(spec.ensure_resolvable().unwrap_err().node, "9");
}

#[test]
fn a_tree_above_the_leaf_cap_is_refused() {
    let leaves = || (0..MAX_BATCH_STEPS).map(|_| probe()).collect::<Vec<_>>();
    let mut branches = Vec::new();
    for _ in 0..5 {
        branches.push(batch(leaves()).unwrap());
    }
    let error = batch(branches).unwrap_err();
    assert!(matches!(error, OpSpecError::TreeTooLarge { .. }));
}
