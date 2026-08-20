#![allow(deprecated)]
use std::str::FromStr;
use std::time::{Duration, SystemTime};

use mm_domain::{
    ConcatSpec, InputFailure, InputOrder, JobFailure, JobId, JobRecord, JobRequester, JobState,
    MediaRef, OpResult, OpSpec, ProbeResult, ProgressPercent, QueueUrl, RequesterId, RequesterType,
    TimedOutput, TraceId, UserId,
};
use mm_proto::merge::{self, MergeJobError};
use mm_proto::v1;
use mm_proto::vm::v1::{InputFile, ListUserJobsRequest, OutputProfile, Requester, VideoMergeJob};

fn job() -> VideoMergeJob {
    VideoMergeJob {
        job_id: String::new(),
        request_id: "req-1".to_string(),
        requester: Requester::ChatMessage as i32,
        user_id: "owner-1".to_string(),
        status: None,
        input_files: vec![
            InputFile {
                sequence_order: 1,
                media_id: "s3://media/in/b.mp4".to_string(),
            },
            InputFile {
                sequence_order: 0,
                media_id: "s3://media/in/a.mp4".to_string(),
            },
        ],
        created_at_millis: 0,
        error_code: None,
        file_errors: Vec::new(),
        output: None,
        r2_uri: None,
    }
}

fn profile(frame_rate: Option<i32>, bitrate: Option<i32>) -> OutputProfile {
    OutputProfile {
        frame_rate,
        video_max_bitrate: bitrate,
        video_qvbr_quality: None,
        audio_bitrate: None,
        audio_sample_rate: None,
    }
}

fn concat(spec: OpSpec) -> ConcatSpec {
    match spec {
        OpSpec::Concat(spec) => spec,
        other => panic!("a reference create compiles a concat spec, got {other:?}"),
    }
}

#[test]
fn a_reference_job_compiles_the_inputs_the_order_names() {
    let spec = concat(merge::op_spec(&job()).expect("the spec compiles"));

    assert_eq!(
        spec.inputs()
            .iter()
            .map(|input| input
                .media()
                .expect("an input names media")
                .media()
                .to_uri())
            .collect::<Vec<_>>(),
        vec!["s3://media/in/a.mp4", "s3://media/in/b.mp4"],
        "the first input is the one the lowest sequence order names"
    );
}

#[test]
fn the_output_profile_reaches_the_spec_it_asked_for() {
    let mut sample = job();
    sample.output = Some(profile(Some(30), Some(8_000_000)));

    let spec = concat(merge::op_spec(&sample).expect("the spec compiles"));

    let profile = spec.profile().expect("the profile is carried");
    assert_eq!(profile.frame_rate(), Some(30.0));
    assert_eq!(profile.video_max_bitrate(), Some(8_000_000));
}

#[test]
fn an_absent_output_profile_carries_none_rather_than_a_zero() {
    let spec = concat(merge::op_spec(&job()).expect("the spec compiles"));

    assert_eq!(spec.profile(), None);
}

#[test]
fn a_partial_output_profile_keeps_the_absent_field_at_the_default() {
    let mut sample = job();
    sample.output = Some(profile(None, Some(4_000_000)));

    let profile = concat(merge::op_spec(&sample).expect("the spec compiles"))
        .profile()
        .cloned()
        .expect("the profile is carried");

    assert_eq!(profile.frame_rate(), None, "an unset frame rate is no rate");
    assert_eq!(profile.video_max_bitrate(), Some(4_000_000));
}

#[test]
fn an_output_profile_off_the_contract_is_refused_naming_the_field() {
    let mut sample = job();
    sample.output = Some(profile(Some(-1), None));

    assert!(
        matches!(merge::op_spec(&sample), Err(MergeJobError::Profile(_))),
        "a negative frame rate is refused"
    );
}

#[test]
fn a_repeated_sequence_order_is_refused_naming_the_position() {
    let mut sample = job();
    sample.input_files[1].sequence_order = 1;

    match merge::op_spec(&sample) {
        Err(MergeJobError::InputOrder(error)) => {
            assert!(error.to_string().contains('1'), "{error}");
        }
        other => panic!("a repeated sequence order is refused, got {other:?}"),
    }
}

#[test]
fn fewer_than_two_inputs_is_refused() {
    let mut sample = job();
    sample.input_files.pop();

    assert!(matches!(
        merge::op_spec(&sample),
        Err(MergeJobError::TooFewInputs)
    ));
}

#[test]
fn a_malformed_media_id_is_refused() {
    let mut sample = job();
    sample.input_files[0].media_id = String::new();

    assert!(matches!(
        merge::op_spec(&sample),
        Err(MergeJobError::Media(_))
    ));
}

#[test]
fn every_requester_value_this_service_accepts_round_trips() {
    for (value, kind) in [
        (Requester::ChatMessage, RequesterType::ChatMessage),
        (
            Requester::CanvasExecutionTask,
            RequesterType::CanvasExecutionTask,
        ),
        (Requester::SeriesEpisode, RequesterType::SeriesEpisode),
        (Requester::Studio, RequesterType::Studio),
    ] {
        assert_eq!(merge::requester_kind(value).expect("a known value"), kind);
        assert_eq!(merge::requester_value(Some(kind)), value);
    }
    assert_eq!(merge::requester_value(None), Requester::Unspecified);
}

#[test]
fn an_unset_requester_is_refused_rather_than_read_as_one() {
    let mut sample = job();
    sample.requester = Requester::Unspecified as i32;

    assert!(matches!(
        merge::requester(&sample),
        Err(MergeJobError::MissingRequester)
    ));
}

#[test]
fn a_blank_request_id_falls_back_to_the_idempotency_key() {
    let mut sample = job();
    sample.request_id = String::new();
    let message = mm_proto::vm::v1::CreateJobRequest {
        job: Some(sample),
        idempotency_key: "req-key".to_string(),
        callback: None,
    };

    assert_eq!(merge::request_id(&message).expect("a key"), "req-key");
}

#[test]
fn no_request_id_and_no_key_is_refused() {
    let mut sample = job();
    sample.request_id = String::new();
    let message = mm_proto::vm::v1::CreateJobRequest {
        job: Some(sample),
        idempotency_key: String::new(),
        callback: None,
    };

    assert!(matches!(
        merge::request_id(&message),
        Err(MergeJobError::MissingRequestId)
    ));
}

fn record(state: JobState, result: Option<OpResult>, failure: Option<JobFailure>) -> JobRecord {
    JobRecord {
        id: JobId::generate(),
        owner: UserId::from_str("owner-1").expect("owner"),
        request: mm_domain::RequestId::from_str("req-1").expect("request"),
        spec: merge::op_spec(&job()).expect("the spec compiles"),
        state,
        progress: ProgressPercent::ZERO,
        failure,
        result,
        created_at: SystemTime::UNIX_EPOCH + Duration::from_millis(1_726_000_000_123),
        started_at: None,
        finished_at: None,
        attempts: 0,
        next_attempt_at: None,
        deadline_at: None,
        claimed_by: None,
        lease_expires_at: None,
        cancel_requested: false,
        callback: Some(QueueUrl::new("http://127.0.0.1:9324/queue/results").expect("queue")),
        trace_id: Some(TraceId::generate()),
        requester: Some(JobRequester::new(
            RequesterId::parse("chat-42").expect("requester id"),
            RequesterType::ChatMessage,
        )),
        input_order: Some(InputOrder::new(vec![0, 1]).expect("the order")),
        duration_ms: None,
        rungs: Vec::new(),
    }
}

#[test]
fn a_settled_job_answers_the_reference_status() {
    for (state, status) in [
        (JobState::Succeeded, v1::StitchStatus::Completed),
        (JobState::Failed, v1::StitchStatus::Failed),
        (JobState::Cancelled, v1::StitchStatus::Cancelled),
    ] {
        let answer = merge::to_proto(&record(state, None, None));
        assert_eq!(answer.status, Some(status as i32), "{state}");
    }
}

#[test]
fn an_unsettled_job_answers_no_status() {
    for state in [JobState::Pending, JobState::Running] {
        let answer = merge::to_proto(&record(state, None, None));
        assert_eq!(answer.status, None, "{state} makes no claim");
    }
}

#[test]
fn a_job_answers_its_created_at_the_requester_and_the_input_order() {
    let answer = merge::to_proto(&record(JobState::Pending, None, None));

    assert_eq!(answer.created_at_millis, 1_726_000_000_123);
    assert_eq!(answer.requester, Requester::ChatMessage as i32);
    assert_eq!(
        answer
            .input_files
            .iter()
            .map(|file| (file.sequence_order, file.media_id.as_str()))
            .collect::<Vec<_>>(),
        vec![(0, "s3://media/in/a.mp4"), (1, "s3://media/in/b.mp4"),]
    );
}

#[test]
fn a_completed_job_answers_the_output_location() {
    let result = OpResult::Concat(
        TimedOutput::new(
            MediaRef::object_uri("s3://media/jobs/x/output.mp4").expect("valid ref"),
            9.5,
        )
        .expect("valid result"),
    );

    let answer = merge::to_proto(&record(JobState::Succeeded, Some(result), None));

    assert_eq!(
        answer.r2_uri,
        Some("s3://media/jobs/x/output.mp4".to_string())
    );
}

#[test]
fn a_probe_job_names_no_output_location() {
    let result =
        OpResult::Probe(ProbeResult::new(3.0, 1, 1, "mp4".to_string()).expect("valid result"));

    let answer = merge::to_proto(&record(JobState::Succeeded, Some(result), None));

    assert_eq!(answer.r2_uri, None);
}

#[test]
fn a_failed_job_answers_the_reference_error_code_and_file_errors() {
    let failure = JobFailure::new("validation_blocked", "inputs rejected")
        .expect("failure")
        .with_inputs(vec![
            InputFailure::new(None, None, Some(0), "UnsupportedContainer").expect("failure"),
        ]);

    let answer = merge::to_proto(&record(JobState::Failed, None, Some(failure)));

    assert_eq!(
        answer.error_code,
        Some("ERROR_UNSUPPORTED_CONTAINER".to_string())
    );
    assert_eq!(answer.file_errors.len(), 1);
    assert_eq!(answer.file_errors[0].code, "ERROR_UNSUPPORTED_CONTAINER");
    assert_eq!(answer.file_errors[0].input_index, Some(0));
}

#[test]
fn the_status_filter_reads_the_reference_statuses() {
    let request = ListUserJobsRequest {
        statuses: vec![
            v1::StitchStatus::Cancelled as i32,
            v1::StitchStatus::Completed as i32,
        ],
        ..Default::default()
    };

    assert_eq!(
        merge::states(&request).expect("the filter compiles"),
        vec![JobState::Cancelled, JobState::Succeeded],
        "the filter reads the statuses in the order the caller wrote them"
    );
}

#[test]
fn an_empty_status_filter_asks_for_every_state() {
    let request = ListUserJobsRequest::default();

    assert!(
        merge::states(&request)
            .expect("the filter compiles")
            .is_empty()
    );
}

#[test]
fn a_status_the_reference_does_not_settle_on_is_refused() {
    let request = ListUserJobsRequest {
        statuses: vec![v1::StitchStatus::Unspecified as i32],
        ..Default::default()
    };

    assert!(matches!(
        merge::states(&request),
        Err(MergeJobError::UnsupportedStatus { .. })
    ));
}

#[test]
fn the_input_order_a_job_reads_back_is_the_one_it_was_created_with() {
    let mut sample = job();
    sample.input_files = vec![
        InputFile {
            sequence_order: 30,
            media_id: "s3://media/in/a.mp4".to_string(),
        },
        InputFile {
            sequence_order: 10,
            media_id: "s3://media/in/b.mp4".to_string(),
        },
        InputFile {
            sequence_order: 20,
            media_id: "s3://media/in/c.mp4".to_string(),
        },
    ];

    let order = merge::input_order(&sample).expect("the order compiles");

    assert_eq!(order.sequence(), vec![10, 20, 30]);
}

#[test]
fn an_order_that_does_not_cover_the_inputs_is_refused() {
    let mut sample = job();
    sample.input_files = vec![InputFile {
        sequence_order: 0,
        media_id: "s3://media/in/a.mp4".to_string(),
    }];

    assert!(merge::input_order(&sample).is_err());
}
