use std::str::FromStr;
use std::time::{Duration, SystemTime};

use mm_domain::{
    JobFailure, JobFailureError, JobId, JobState, JobStoreError, MediaRef, NewJob, OpSpec,
    PageCursor, ProbeSpec, ProgressError, ProgressPercent, RequestId, TextIdError, UserId,
    WorkerId,
};

fn sample_spec() -> OpSpec {
    OpSpec::Probe(ProbeSpec::new(
        MediaRef::object_uri("s3://bucket/key").expect("valid media ref"),
    ))
}

#[test]
fn progress_percent_accepts_bounds() {
    assert_eq!(ProgressPercent::new(0).expect("zero is valid").value(), 0);
    assert_eq!(
        ProgressPercent::new(100).expect("hundred is valid").value(),
        100
    );
}

#[test]
fn progress_percent_rejects_overflow() {
    assert_eq!(
        ProgressPercent::new(101),
        Err(ProgressError::OutOfRange { value: 101 })
    );
}

#[test]
fn progress_percent_constants_cover_endpoints() {
    assert_eq!(ProgressPercent::ZERO.value(), 0);
    assert_eq!(ProgressPercent::COMPLETE.value(), 100);
}

#[test]
fn worker_id_parses_graphic_ascii() {
    let worker = WorkerId::from_str("worker-a.0").expect("valid worker id");
    assert_eq!(worker.as_str(), "worker-a.0");
    assert_eq!(worker.to_string(), "worker-a.0");
}

#[test]
fn worker_id_rejects_empty_and_whitespace() {
    assert_eq!(WorkerId::from_str(""), Err(TextIdError::Empty));
    assert_eq!(
        WorkerId::from_str("worker a"),
        Err(TextIdError::ForbiddenCharacter { character: ' ' })
    );
}

#[test]
fn job_id_from_uuid_rejects_nil() {
    let id = JobId::generate();
    assert_eq!(JobId::from_uuid(id.as_uuid()), Ok(id));
    assert!(JobId::from_uuid(uuid::Uuid::nil()).is_err());
}

#[test]
fn job_failure_keeps_code_and_message() {
    let failure = JobFailure::new("TOOL_FAILED", "exit status 1").expect("valid failure");
    assert_eq!(failure.code(), "TOOL_FAILED");
    assert_eq!(failure.message(), "exit status 1");
}

#[test]
fn job_failure_rejects_bad_code() {
    assert_eq!(
        JobFailure::new("", "boom"),
        Err(JobFailureError::Code(TextIdError::Empty))
    );
    assert!(matches!(
        JobFailure::new("bad code", "boom"),
        Err(JobFailureError::Code(TextIdError::ForbiddenCharacter {
            character: ' '
        }))
    ));
}

#[test]
fn new_job_exposes_fields() {
    let id = JobId::generate();
    let owner = UserId::from_str("owner-1").expect("valid user id");
    let request = RequestId::from_str("req-1").expect("valid request id");
    let deadline = SystemTime::now() + Duration::from_secs(600);
    let job = NewJob::new(
        id,
        owner.clone(),
        request.clone(),
        sample_spec(),
        Some(deadline),
    );
    assert_eq!(job.id(), id);
    assert_eq!(job.owner(), &owner);
    assert_eq!(job.request(), &request);
    assert_eq!(job.spec(), &sample_spec());
    assert_eq!(job.deadline_at(), Some(deadline));
}

#[test]
fn page_cursor_orders_by_time_then_id() {
    let id = JobId::generate();
    let at = SystemTime::now();
    let cursor = PageCursor { created_at: at, id };
    assert_eq!(cursor.created_at, at);
    assert_eq!(cursor.id, id);
}

#[test]
fn store_errors_render_context() {
    let id = JobId::generate();
    let existing = JobId::generate();
    let worker = WorkerId::from_str("worker-a").expect("valid worker id");
    assert!(
        JobStoreError::AlreadyExists { existing }
            .to_string()
            .contains(&existing.to_string())
    );
    assert!(
        JobStoreError::NotFound { id }
            .to_string()
            .contains(&id.to_string())
    );
    let transition = JobStoreError::InvalidTransition {
        id,
        from: JobState::Succeeded,
        to: JobState::Running,
    }
    .to_string();
    assert!(transition.contains("SUCCEEDED") && transition.contains("RUNNING"));
    assert!(
        JobStoreError::LeaseNotHeld {
            id,
            worker: worker.clone()
        }
        .to_string()
        .contains("worker-a")
    );
    assert!(
        JobStoreError::CorruptRecord {
            id,
            detail: "bad spec".to_string()
        }
        .to_string()
        .contains("bad spec")
    );
    assert!(
        JobStoreError::Backend("down".to_string())
            .to_string()
            .contains("down")
    );
}
