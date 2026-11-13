//! The HTTP admission series are on the first scrape of a started gateway:
//! the process-start gauge, the counters at zero and the limit gauges. They
//! are written during start-up, so they land only if the recorder is already
//! installed at that point; a write before it goes to the no-op recorder and
//! is lost. The gateway runs as a process on purpose: the order of the
//! start-up steps is what the test pins.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::allow_attributes
)]

use std::{fs, net::SocketAddr};

use gateway_launch::{base_args, free_ports, request, serial, Gateway, IP};

use crate::gateway_launch;

/// The value of the unlabelled series `name` in a scrape body.
fn value(body: &str, name: &str) -> Option<f64> {
    body.lines()
        .find_map(|line| line.strip_prefix(name)?.strip_prefix(' ')?.parse().ok())
}

/// Starts the gateway with `extra` flags; returns it with its metrics address.
fn start(extra: &[String], dir: &std::path::Path) -> (Gateway, SocketAddr) {
    let [port, metrics_port] = free_ports();
    let mut args = base_args(port, metrics_port);
    args.extend(extra.iter().cloned());
    let gateway = Gateway::start(&args, SocketAddr::from((IP, port)), &dir.join("gateway.log"));
    (gateway, SocketAddr::from((IP, metrics_port)))
}

/// One `/metrics` scrape.
fn scrape(gateway: &Gateway, metrics: SocketAddr) -> String {
    let (status, body) = request(metrics, "GET", "/metrics", None, None)
        .unwrap_or_else(|| panic!("the metrics listener did not answer\n{}", gateway.log()));
    assert_eq!(status, 200, "{body}");
    body
}

fn assert_startup_series(body: &str) {
    let start = value(body, "smg_http_admission_process_start_time_seconds")
        .unwrap_or_else(|| panic!("no process start gauge\n{body}"));
    assert!(start > 1.0e9, "process start {start} is not an epoch time");
    for name in [
        "smg_http_admission_received_total",
        "smg_http_admission_admitted_total",
        "smg_http_admission_rejected_total",
        "smg_http_admission_success_total",
        "smg_http_admission_http_4xx_total",
        "smg_http_admission_http_5xx_total",
        "smg_http_admission_interrupted_total",
        "smg_http_admission_pre_admission_interrupted_total",
        "smg_http_admission_active",
        "smg_http_admission_queued",
    ] {
        assert_eq!(value(body, name), Some(0.0), "{name} is not at zero\n{body}");
    }
}

/// With the legacy concurrency limit the limit gauge is the configured one.
#[test]
fn the_legacy_limit_gateway_exports_the_admission_series_at_start() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let _serial = serial();
    let (gateway, metrics) = start(
        &["--max-concurrent-requests", "60000", "--queue-size", "2000"].map(str::to_string),
        dir.path(),
    );
    let body = scrape(&gateway, metrics);
    assert_startup_series(&body);
    assert_eq!(value(&body, "smg_http_admission_limit"), Some(60000.0));
    assert_eq!(
        value(&body, "smg_http_admission_queue_capacity"),
        Some(2000.0)
    );
}

/// With static partitions the limit gauge is the sum of the partition limits.
#[test]
fn the_partitioned_scheduler_exports_the_sum_of_partition_limits() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let config = dir.path().join("scheduler.yaml");
    fs::write(
        &config,
        r#"{"admission_partitions":{"default":{"max_concurrent_requests":10,"queue_size":20},"other":{"max_concurrent_requests":32,"queue_size":40}},"default_admission_partition":"default"}"#,
    )
    .unwrap();
    let _serial = serial();
    let (gateway, metrics) = start(
        &[
            "--max-concurrent-requests",
            "60000",
            "--queue-size",
            "2000",
            "--priority-scheduler-enabled",
            "--priority-scheduler-config",
            config.to_str().unwrap(),
        ]
        .map(str::to_string),
        dir.path(),
    );
    assert_startup_series(&scrape(&gateway, metrics));
    // The sampler writes the sum on its first tick, a moment after start.
    let mut body = scrape(&gateway, metrics);
    for _ in 0..50 {
        if value(&body, "smg_http_admission_limit") == Some(42.0) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
        body = scrape(&gateway, metrics);
    }
    panic!("the limit is not the sum of the partition limits\n{body}");
}
