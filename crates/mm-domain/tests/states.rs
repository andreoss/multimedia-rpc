use mm_domain::{
    Cancelled, Failed, Job, JobId, JobState, Pending, RequestId, Running, StateParseError,
    Succeeded, TransitionError, UserId,
};

fn new_pending() -> Job<Pending> {
    let owner: UserId = "owner-1".parse().expect("owner");
    let request: RequestId = "req-1".parse().expect("request");
    Job::create(JobId::generate(), owner, request)
}

#[test]
fn created_job_is_pending() {
    let job = new_pending();
    assert_eq!(job.state(), JobState::Pending);
    assert!(!job.state().is_terminal());
}

#[test]
fn pending_job_starts_running() {
    let job: Job<Running> = new_pending().start();
    assert_eq!(job.state(), JobState::Running);
}

#[test]
fn pending_job_cancels() {
    let job: Job<Cancelled> = new_pending().cancel();
    assert_eq!(job.state(), JobState::Cancelled);
    assert!(job.state().is_terminal());
}

#[test]
fn running_job_succeeds() {
    let job: Job<Succeeded> = new_pending().start().succeed();
    assert_eq!(job.state(), JobState::Succeeded);
    assert!(job.state().is_terminal());
}

#[test]
fn running_job_fails() {
    let job: Job<Failed> = new_pending().start().fail();
    assert_eq!(job.state(), JobState::Failed);
    assert!(job.state().is_terminal());
}

#[test]
fn running_job_cancels() {
    let job: Job<Cancelled> = new_pending().start().cancel();
    assert_eq!(job.state(), JobState::Cancelled);
}

#[test]
fn identity_survives_transitions() {
    let pending = new_pending();
    let id = pending.id();
    let done = pending.start().succeed();
    assert_eq!(done.id(), id);
    assert_eq!(done.owner().as_str(), "owner-1");
    assert_eq!(done.request().as_str(), "req-1");
}

#[test]
fn every_invalid_transition_is_rejected() {
    use JobState::*;
    let all = [Pending, Running, Succeeded, Failed, Cancelled];
    let allowed = [
        (Pending, Running),
        (Pending, Cancelled),
        (Running, Succeeded),
        (Running, Failed),
        (Running, Cancelled),
    ];
    for from in all {
        for to in all {
            let result = from.transition(to);
            if allowed.contains(&(from, to)) {
                assert_eq!(result, Ok(to));
            } else {
                assert_eq!(result, Err(TransitionError { from, to }));
            }
        }
    }
}

#[test]
fn terminal_states_admit_no_transition() {
    use JobState::*;
    for from in [Succeeded, Failed, Cancelled] {
        assert!(from.is_terminal());
        for to in [Pending, Running, Succeeded, Failed, Cancelled] {
            assert!(from.transition(to).is_err());
        }
    }
}

#[test]
fn transition_error_renders_states() {
    let err = TransitionError {
        from: JobState::Succeeded,
        to: JobState::Running,
    };
    let text = err.to_string();
    assert!(text.contains("SUCCEEDED"));
    assert!(text.contains("RUNNING"));
}

#[test]
fn state_names_round_trip() {
    for state in [
        JobState::Pending,
        JobState::Running,
        JobState::Succeeded,
        JobState::Failed,
        JobState::Cancelled,
    ] {
        let parsed: JobState = state.to_string().parse().expect("round trip");
        assert_eq!(parsed, state);
    }
}

#[test]
fn state_parse_rejects_unknown_name() {
    let err = "DONE".parse::<JobState>().unwrap_err();
    assert_eq!(err, StateParseError::Unknown("DONE".to_string()));
}

#[test]
fn state_parse_rejects_lowercase() {
    assert!("pending".parse::<JobState>().is_err());
}

#[test]
fn state_parse_rejects_empty() {
    assert!("".parse::<JobState>().is_err());
}
