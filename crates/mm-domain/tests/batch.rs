use mm_domain::{
    BatchSpec, MAX_BATCH_STEPS, MAX_NESTING_DEPTH, MediaRef, OpSpec, OpSpecError, ProbeSpec,
};

fn probe() -> OpSpec {
    OpSpec::Probe(ProbeSpec::new(MediaRef::object_uri("s3://bucket/key").unwrap()).unwrap())
}

fn batch(steps: Vec<OpSpec>) -> Result<OpSpec, OpSpecError> {
    BatchSpec::new(steps, true).map(OpSpec::Batch)
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
fn a_tree_above_the_leaf_cap_is_refused() {
    let leaves = || (0..MAX_BATCH_STEPS).map(|_| probe()).collect::<Vec<_>>();
    let mut branches = Vec::new();
    for _ in 0..5 {
        branches.push(batch(leaves()).unwrap());
    }
    let error = batch(branches).unwrap_err();
    assert!(matches!(error, OpSpecError::TreeTooLarge { .. }));
}
