use mm_domain::{QueueUrl, QueueUrlError};

#[test]
fn an_http_queue_url_parses_and_round_trips() {
    let url = QueueUrl::new("http://127.0.0.1:9324/queue/results").expect("valid url");
    assert_eq!(url.as_str(), "http://127.0.0.1:9324/queue/results");
    assert_eq!(url.to_string(), "http://127.0.0.1:9324/queue/results");
}

#[test]
fn an_https_queue_url_parses() {
    assert!(QueueUrl::new("https://queue.example/results").is_ok());
}

#[test]
fn a_non_http_queue_url_is_rejected() {
    for value in [
        "",
        "queue/results",
        "ftp://host/queue",
        "http://",
        "https://",
    ] {
        assert_eq!(
            QueueUrl::new(value),
            Err(QueueUrlError::Scheme {
                value: value.to_string()
            }),
            "{value}"
        );
    }
}
