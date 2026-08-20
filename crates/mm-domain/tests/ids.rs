use mm_domain::{
    JobId, JobIdError, RequestId, RequestIdError, TraceId, TraceIdError, UserId, UserIdError,
};

#[test]
fn generated_job_id_round_trips() {
    let id = JobId::generate();
    let parsed: JobId = id.to_string().parse().expect("round trip");
    assert_eq!(parsed, id);
}

#[test]
fn job_id_rejects_garbage() {
    let err = "not-a-uuid".parse::<JobId>().unwrap_err();
    assert_eq!(err, JobIdError::Malformed);
}

#[test]
fn job_id_rejects_truncated_uuid() {
    let err = "0199c2f6-7f4e-7bd3-9d2a".parse::<JobId>().unwrap_err();
    assert_eq!(err, JobIdError::Malformed);
}

#[test]
fn job_id_rejects_nil_uuid() {
    let err = "00000000-0000-0000-0000-000000000000"
        .parse::<JobId>()
        .unwrap_err();
    assert_eq!(err, JobIdError::Nil);
}

#[test]
fn job_id_rejects_embedded_whitespace() {
    let err = " 0199c2f6-7f4e-7bd3-9d2a-0f3b2a1c4d5e "
        .parse::<JobId>()
        .unwrap_err();
    assert_eq!(err, JobIdError::Malformed);
}

#[test]
fn valid_user_id_round_trips() {
    let id: UserId = "user-7@example.org".parse().expect("valid");
    assert_eq!(id.as_str(), "user-7@example.org");
    assert_eq!(id.to_string(), "user-7@example.org");
}

#[test]
fn user_id_rejects_empty() {
    let err = "".parse::<UserId>().unwrap_err();
    assert_eq!(err, UserIdError::Empty);
}

#[test]
fn user_id_rejects_overlong() {
    let long = "u".repeat(65);
    let err = long.parse::<UserId>().unwrap_err();
    assert_eq!(
        err,
        UserIdError::TooLong {
            length: 65,
            max: 64
        }
    );
}

#[test]
fn user_id_accepts_max_length() {
    let max = "u".repeat(64);
    assert!(max.parse::<UserId>().is_ok());
}

#[test]
fn user_id_rejects_inner_space() {
    let err = "user one".parse::<UserId>().unwrap_err();
    assert_eq!(err, UserIdError::ForbiddenCharacter { character: ' ' });
}

#[test]
fn user_id_rejects_control_character() {
    let err = "user\u{7}one".parse::<UserId>().unwrap_err();
    assert_eq!(err, UserIdError::ForbiddenCharacter { character: '\u{7}' });
}

#[test]
fn user_id_rejects_non_ascii() {
    let err = "usér".parse::<UserId>().unwrap_err();
    assert_eq!(err, UserIdError::ForbiddenCharacter { character: 'é' });
}

#[test]
fn valid_request_id_round_trips() {
    let id: RequestId = "req_01HZX4.K9-test".parse().expect("valid");
    assert_eq!(id.as_str(), "req_01HZX4.K9-test");
    assert_eq!(id.to_string(), "req_01HZX4.K9-test");
}

#[test]
fn request_id_rejects_empty() {
    let err = "".parse::<RequestId>().unwrap_err();
    assert_eq!(err, RequestIdError::Empty);
}

#[test]
fn request_id_rejects_overlong() {
    let long = "r".repeat(129);
    let err = long.parse::<RequestId>().unwrap_err();
    assert_eq!(
        err,
        RequestIdError::TooLong {
            length: 129,
            max: 128
        }
    );
}

#[test]
fn request_id_accepts_max_length() {
    let max = "r".repeat(128);
    assert!(max.parse::<RequestId>().is_ok());
}

#[test]
fn request_id_rejects_inner_space() {
    let err = "req 1".parse::<RequestId>().unwrap_err();
    assert_eq!(err, RequestIdError::ForbiddenCharacter { character: ' ' });
}

#[test]
fn request_id_rejects_newline() {
    let err = "req\n1".parse::<RequestId>().unwrap_err();
    assert_eq!(err, RequestIdError::ForbiddenCharacter { character: '\n' });
}

#[test]
fn request_id_rejects_non_ascii() {
    let err = "réq".parse::<RequestId>().unwrap_err();
    assert_eq!(err, RequestIdError::ForbiddenCharacter { character: 'é' });
}

#[test]
fn id_errors_render_messages() {
    assert!(!JobIdError::Malformed.to_string().is_empty());
    assert!(!JobIdError::Nil.to_string().is_empty());
    assert!(!UserIdError::Empty.to_string().is_empty());
    assert!(
        !RequestIdError::TooLong {
            length: 129,
            max: 128
        }
        .to_string()
        .is_empty()
    );
}

#[test]
fn generated_trace_id_round_trips() {
    let id = TraceId::generate();
    let parsed: TraceId = id.to_string().parse().expect("round trip");
    assert_eq!(parsed, id);
    assert_eq!(parsed.as_uuid(), id.as_uuid());
}

#[test]
fn trace_id_rejects_a_malformed_value() {
    assert_eq!(
        "not-a-uuid".parse::<TraceId>().unwrap_err(),
        TraceIdError::Malformed
    );
}

#[test]
fn trace_id_rejects_the_nil_uuid() {
    assert_eq!(
        "00000000-0000-0000-0000-000000000000"
            .parse::<TraceId>()
            .unwrap_err(),
        TraceIdError::Nil
    );
    assert_eq!(
        TraceId::from_uuid(uuid::Uuid::nil()).unwrap_err(),
        TraceIdError::Nil
    );
}

#[test]
fn trace_id_errors_render_messages() {
    assert!(!TraceIdError::Malformed.to_string().is_empty());
    assert!(!TraceIdError::Nil.to_string().is_empty());
}
