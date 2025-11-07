use std::time::SystemTime;

use mm_domain::{
    ConcatSpec, JobFailure, JobId, JobRecord, JobState, MediaRef, OpResult, OpSpec,
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
    JobFailure::new(
        "validation_blocked",
        "input 0: UnsupportedContainer; inputs: TooFewInputs",
    )
    .expect("failure")
}

#[test]
fn a_validation_failure_maps_file_errors_onto_the_spec_inputs() {
    let job = v1::Job::from(&record(JobState::Failed, Some(blocked()), None));
    assert_eq!(
        job.file_errors,
        vec![
            v1::FileError {
                media_ref: Some(v1::MediaRef {
                    r#ref: Some(v1::media_ref::Ref::ObjectUri(
                        "s3://media/in/a.mp4".to_string()
                    )),
                }),
                code: "UnsupportedContainer".to_string(),
            },
            v1::FileError {
                media_ref: None,
                code: "TooFewInputs".to_string(),
            },
        ]
    );
    assert_eq!(job.state, v1::JobState::Failed as i32);
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
    assert_eq!(outcome.error_code, "validation_blocked");
    assert_eq!(
        outcome.error_message,
        "input 0: UnsupportedContainer; inputs: TooFewInputs"
    );
    assert_eq!(outcome.result, None);
    assert_eq!(outcome.file_errors.len(), 2);
    assert_eq!(outcome.file_errors[1].code, "TooFewInputs");
}
