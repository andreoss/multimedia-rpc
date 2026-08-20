use std::time::SystemTime;

use mm_domain::{
    ConcatSpec, InputFailure, JobFailure, JobId, JobRecord, JobState, MediaRef, OpResult, OpSpec,
    ProgressPercent, QueueUrl, RequestId, TimedOutput, TraceId, UserId,
};
use mm_proto::v1;
use std::str::FromStr;

fn spec() -> OpSpec {
    OpSpec::Concat(
        ConcatSpec::new(
            vec![
                MediaRef::object_uri("s3://media/in/a.mp4").expect("valid ref"),
                MediaRef::object_uri("s3://media/in/b.mp4").expect("valid ref"),
            ],
            None,
        )
        .expect("valid concat spec"),
    )
}

fn record(state: JobState, failure: Option<JobFailure>, result: Option<OpResult>) -> JobRecord {
    JobRecord {
        id: JobId::generate(),
        owner: UserId::from_str("owner-1").expect("owner"),
        request: RequestId::from_str("req-1").expect("request"),
        spec: spec(),
        state,
        progress: ProgressPercent::ZERO,
        failure,
        result,
        created_at: SystemTime::now(),
        started_at: None,
        finished_at: None,
        attempts: 1,
        next_attempt_at: None,
        deadline_at: None,
        claimed_by: None,
        lease_expires_at: None,
        cancel_requested: false,
        callback: Some(QueueUrl::new("http://127.0.0.1:9324/queue/results").expect("queue url")),
        trace_id: Some(TraceId::generate()),
    }
}

fn blocked() -> JobFailure {
    JobFailure::new("validation_blocked", "inputs rejected by the validator")
        .expect("failure")
        .with_inputs(vec![
            InputFailure::new(None, None, Some(0), "UnsupportedContainer").expect("valid failure"),
            InputFailure::new(None, None, None, "TooFewInputs").expect("valid failure"),
        ])
}

fn file_error(
    code: &str,
    step: Option<u32>,
    node: Option<&str>,
    input: Option<u32>,
) -> v1::FileError {
    v1::FileError {
        media_ref: Some(v1::MediaRef {
            r#ref: Some(v1::media_ref::Ref::ObjectUri(
                "s3://media/in/a.mp4".to_string(),
            )),
        }),
        code: code.to_string(),
        step_index: step,
        node_id: node.map(str::to_string),
        input_index: input,
    }
}

#[test]
fn a_validation_failure_maps_file_errors_onto_the_spec_inputs() {
    let job = v1::Job::from(&record(JobState::Failed, Some(blocked()), None));
    assert_eq!(
        job.file_errors,
        vec![
            file_error("ERROR_UNSUPPORTED_CONTAINER", None, None, Some(0)),
            v1::FileError {
                media_ref: None,
                code: "ERROR_TOO_FEW_INPUTS".to_string(),
                step_index: None,
                node_id: None,
                input_index: None,
            },
        ]
    );
    assert_eq!(job.state, v1::JobState::Failed as i32);
}

#[test]
fn file_errors_are_read_off_the_row_and_not_back_out_of_the_message() {
    let failure = JobFailure::new("validation_blocked", "step 2: inputs: TooFewInputs")
        .expect("failure")
        .with_inputs(vec![
            InputFailure::new(Some(2), Some("join"), Some(0), "UnsupportedContainer")
                .expect("valid failure"),
        ]);

    let job = v1::Job::from(&record(JobState::Failed, Some(failure), None));

    assert_eq!(
        job.file_errors,
        vec![file_error(
            "ERROR_UNSUPPORTED_CONTAINER",
            Some(2),
            Some("join"),
            Some(0)
        )]
    );
}

#[test]
fn a_file_error_reads_back_into_the_input_failure_the_row_carries() {
    let failure = InputFailure::try_from(&v1::FileError {
        media_ref: None,
        code: "ERROR_UNSUPPORTED_CONTAINER".to_string(),
        step_index: Some(1),
        node_id: Some("join".to_string()),
        input_index: Some(0),
    })
    .expect("a rendered file error is a valid input failure");

    assert_eq!(failure.step(), Some(1));
    assert_eq!(failure.node().map(|node| node.as_str()), Some("join"));
    assert_eq!(failure.input(), Some(0));
    assert_eq!(failure.code(), "ERROR_UNSUPPORTED_CONTAINER");
}

#[test]
fn a_file_error_read_back_and_rendered_again_reads_the_same() {
    let rendered = v1::FileError {
        media_ref: None,
        code: "ERROR_UNSUPPORTED_CONTAINER".to_string(),
        step_index: Some(1),
        node_id: Some("join".to_string()),
        input_index: Some(0),
    };

    let failure = InputFailure::try_from(&rendered).expect("input failure");
    let again = v1::InputFailureList::from(&[failure][..]);

    assert_eq!(again.failures, vec![rendered]);
}

#[test]
fn a_file_error_that_names_no_valid_identity_is_refused() {
    assert!(
        InputFailure::try_from(&v1::FileError {
            media_ref: None,
            code: "UnsupportedContainer".to_string(),
            step_index: None,
            node_id: Some("join step".to_string()),
            input_index: None,
        })
        .is_err()
    );
    assert!(
        InputFailure::try_from(&v1::FileError {
            media_ref: None,
            code: String::new(),
            step_index: None,
            node_id: None,
            input_index: None,
        })
        .is_err()
    );
}

#[test]
fn a_free_form_failure_maps_to_no_file_errors() {
    let failure = JobFailure::new("merge_failed", "tool exited with 1").expect("failure");
    let job = v1::Job::from(&record(JobState::Failed, Some(failure), None));
    assert_eq!(job.file_errors, Vec::new());
}

#[test]
fn a_job_proto_carries_the_callback_and_identity() {
    let sample = record(JobState::Pending, None, None);
    let job = v1::Job::from(&sample);
    assert_eq!(job.job_id, sample.id.to_string());
    assert_eq!(job.user_id, "owner-1");
    assert_eq!(job.request_id, "req-1");
    assert_eq!(job.state, v1::JobState::Pending as i32);
    assert_eq!(job.spec, Some(v1::OpSpec::from(&sample.spec)));
    assert_eq!(
        job.callback,
        Some(v1::Callback {
            queue_url: "http://127.0.0.1:9324/queue/results".to_string(),
        })
    );
}

#[test]
fn a_succeeded_outcome_mirrors_the_terminal_record() {
    let result = OpResult::Concat(
        TimedOutput::new(
            MediaRef::object_uri("s3://media/jobs/x/output.mp4").expect("valid ref"),
            10.0,
        )
        .expect("valid result"),
    );
    let sample = record(JobState::Succeeded, None, Some(result.clone()));
    let outcome = v1::JobOutcome::from(&sample);
    assert_eq!(outcome.job_id, sample.id.to_string());
    assert_eq!(outcome.request_id, "req-1");
    assert_eq!(outcome.state, v1::JobState::Succeeded as i32);
    assert_eq!(outcome.result, Some(v1::OpResult::from(&result)));
    assert_eq!(outcome.error_code, "");
    assert_eq!(outcome.error_message, "");
    assert_eq!(outcome.file_errors, Vec::new());
}

#[test]
fn a_failed_outcome_carries_the_error_and_file_errors() {
    let outcome = v1::JobOutcome::from(&record(JobState::Failed, Some(blocked()), None));
    assert_eq!(outcome.state, v1::JobState::Failed as i32);
    assert_eq!(outcome.error_code, "ERROR_UNSUPPORTED_CONTAINER");
    assert_eq!(
        outcome.error_message, "inputs rejected by the validator",
        "the message stays for a human while the file errors come off the row"
    );
    assert_eq!(outcome.result, None);
    assert_eq!(outcome.file_errors.len(), 2);
    assert_eq!(outcome.file_errors[1].code, "ERROR_TOO_FEW_INPUTS");
}
