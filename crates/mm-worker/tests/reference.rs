use mm_domain::{JobFailure, failure_code};
use mm_worker::codes;

fn reference(code: &str) -> String {
    let failure = JobFailure::new(code, "message").expect("a worker code is a valid code");
    failure_code(&failure).to_string()
}

#[test]
fn every_worker_failure_code_carries_a_reference_code() {
    for code in codes::ALL {
        let answer = reference(code);
        assert!(answer.starts_with("ERROR_"), "code {code} answers {answer}");
        assert_ne!(
            answer, "ERROR_INTERNAL",
            "code {code} has no reference mapping and fell to the catch-all"
        );
    }
}

#[test]
fn two_worker_codes_never_answer_one_reference_code() {
    let mut seen: Vec<String> = codes::ALL.iter().map(|code| reference(code)).collect();
    seen.sort();
    let count = seen.len();
    seen.dedup();
    assert_eq!(
        seen.len(),
        count,
        "two worker codes answer one reference code"
    );
}
