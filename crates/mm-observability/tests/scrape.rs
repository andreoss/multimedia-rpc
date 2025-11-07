use std::sync::Arc;
use std::time::Duration;

use mm_observability::{Registry, serve};

#[test]
fn a_fresh_registry_renders_every_series_it_declares() {
    let (registry, metrics) = Registry::new();
    let rendered = registry.render();
    for name in [
        "mm_jobs_pending",
        "mm_jobs_started_total",
        "mm_jobs_finished_total",
        "mm_claim_latency_seconds",
        "mm_job_duration_seconds",
        "mm_tool_run_seconds",
        "mm_tool_speed_ratio",
        "mm_output_bytes_total",
    ] {
        assert!(
            rendered.contains(&format!("# TYPE {name} ")),
            "{name}: {rendered}"
        );
    }
    assert!(metrics.jobs_pending.is_zero());
}

#[test]
fn a_gauge_reports_the_value_it_was_given() {
    let (registry, metrics) = Registry::new();
    metrics.jobs_pending.set(7);
    metrics.jobs_pending.set(3);
    assert!(
        registry.render().contains("mm_jobs_pending 3\n"),
        "{}",
        registry.render()
    );
}

#[test]
fn a_counter_never_falls() {
    let (registry, metrics) = Registry::new();
    metrics.output_bytes.add(10);
    metrics.output_bytes.add(5);
    metrics.output_bytes.add(0);
    assert!(registry.render().contains("mm_output_bytes_total 15\n"));
}

#[test]
fn a_histogram_reports_buckets_sum_and_count() {
    let (registry, metrics) = Registry::new();
    metrics.job_duration.observe(0.4);
    metrics.job_duration.observe(2.0);
    metrics.job_duration.observe(30.0);
    let rendered = registry.render();
    assert!(
        rendered.contains("mm_job_duration_seconds_count 3"),
        "{rendered}"
    );
    assert!(
        rendered.contains("mm_job_duration_seconds_sum 32.4"),
        "{rendered}"
    );
    assert!(
        rendered.contains(r#"mm_job_duration_seconds_bucket{le="1"} 1"#),
        "{rendered}"
    );
    assert!(
        rendered.contains(r#"mm_job_duration_seconds_bucket{le="5"} 2"#),
        "{rendered}"
    );
    assert!(
        rendered.contains(r#"mm_job_duration_seconds_bucket{le="+Inf"} 3"#),
        "{rendered}"
    );
}

#[test]
fn a_declared_series_is_rendered_in_the_order_it_was_declared() {
    let (registry, _) = Registry::new();
    let rendered = registry.render();
    let pending = rendered.find("mm_jobs_pending ").expect("gauge rendered");
    let finished = rendered
        .find("mm_jobs_finished_total ")
        .expect("counter rendered");
    let duration = rendered
        .find("mm_job_duration_seconds_count")
        .expect("histogram rendered");
    assert!(pending < finished && finished < duration, "{rendered}");
}

#[tokio::test]
async fn the_scrape_endpoint_answers_the_registry_in_plain_text() {
    let (registry, metrics) = Registry::new();
    metrics.jobs_pending.set(5);
    let registry = Arc::new(registry);
    let scrape = serve(
        registry.clone(),
        "127.0.0.1:0".parse().expect("bind address"),
    )
    .await
    .expect("the scrape endpoint binds");
    assert_ne!(scrape.local_addr().port(), 0);

    let response = request(
        scrape.local_addr(),
        "GET /metrics HTTP/1.1\r\nhost: local\r\nconnection: close\r\n\r\n",
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    assert!(
        response.contains("content-type: text/plain; version=0.0.4; charset=utf-8"),
        "{response}"
    );
    assert!(response.contains("mm_jobs_pending 5"), "{response}");
    scrape.shutdown().await;
}

#[tokio::test]
async fn the_scrape_endpoint_answers_another_path_with_not_found() {
    let (registry, _) = Registry::new();
    let scrape = serve(
        Arc::new(registry),
        "127.0.0.1:0".parse().expect("bind address"),
    )
    .await
    .expect("the scrape endpoint binds");
    let response = request(
        scrape.local_addr(),
        "GET /nope HTTP/1.1\r\nhost: local\r\n\r\n",
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 404 Not Found"), "{response}");
    scrape.shutdown().await;
}

#[tokio::test]
async fn the_scrape_endpoint_serves_several_reads_and_stops_on_request() {
    let (registry, metrics) = Registry::new();
    let registry = Arc::new(registry);
    let scrape = serve(
        registry.clone(),
        "127.0.0.1:0".parse().expect("bind address"),
    )
    .await
    .expect("the scrape endpoint binds");
    metrics.jobs_pending.set(2);
    let first = request(scrape.local_addr(), "GET /metrics HTTP/1.0\r\n\r\n").await;
    metrics.jobs_pending.set(9);
    let second = request(scrape.local_addr(), "GET /metrics HTTP/1.0\r\n\r\n").await;
    assert!(first.contains("mm_jobs_pending 2"), "{first}");
    assert!(second.contains("mm_jobs_pending 9"), "{second}");
    scrape.shutdown().await;
}

#[tokio::test]
async fn a_read_of_the_scrape_endpoint_does_not_hang() {
    let (registry, _) = Registry::new();
    let scrape = serve(
        Arc::new(registry),
        "127.0.0.1:0".parse().expect("bind address"),
    )
    .await
    .expect("the scrape endpoint binds");
    let address = scrape.local_addr();
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::net::TcpStream::connect(address),
    )
    .await
    .expect("the endpoint answers within the budget");
    assert!(result.is_ok());
    scrape.shutdown().await;
}

async fn request(address: std::net::SocketAddr, message: &str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut stream = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::net::TcpStream::connect(address),
    )
    .await
    .expect("the scrape endpoint answers within the read budget")
    .expect("the scrape endpoint accepts a connection");
    tokio::time::timeout(Duration::from_secs(5), stream.write_all(message.as_bytes()))
        .await
        .expect("the request is written within the budget")
        .expect("the request is written");
    let mut buffer = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut buffer))
        .await
        .expect("the answer arrives within the read budget")
        .expect("the answer is read");
    String::from_utf8_lossy(&buffer).to_string()
}
