use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use fb2converter_server::{api_router, config::Config, AppState};
use http_body_util::BodyExt;
use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tower::ServiceExt;

fn setup(name: &str, script: &str, timeout: u64) -> (axum::Router, PathBuf) {
    let root = std::env::temp_dir().join(format!("fb2c-test-{name}-{}", uuid_like()));
    std::fs::create_dir_all(&root).unwrap();
    let bin = root.join("fake-fb2c.sh");
    std::fs::write(&bin, format!("#!/bin/sh\n{script}\n")).unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    let jobs = root.join("jobs");
    std::fs::create_dir_all(&jobs).unwrap();

    let config = Config {
        api_key: "k".into(),
        sentry_dsn: None,
        fb2c_bin: bin,
        jobs_dir: jobs.clone(),
        max_body_bytes: 1024 * 1024,
        body_read_timeout_secs: 5,
        fb2c_timeout_secs: timeout,
        max_concurrent_conversions: 2,
        queue_wait_secs: 1,
        max_output_bytes: 1024 * 1024,
        cleanup_max_age_secs: 3600,
    };
    (api_router(AppState::new(config)), jobs)
}

fn uuid_like() -> String {
    format!(
        "{}-{:?}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}

fn req(fmt: &str, key: &str) -> Request<Body> {
    Request::post(format!("/{fmt}"))
        .header("authorization", key)
        .body(Body::from("<FictionBook/>"))
        .unwrap()
}

async fn wait_empty(dir: &Path) {
    for _ in 0..50 {
        if std::fs::read_dir(dir).unwrap().next().is_none() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("job dir {dir:?} not cleaned up");
}

// fake fb2c args: convert --to FMT INPUT OUTDIR
const OK_SCRIPT: &str = r#"
in="$4"; out="$5"; base=$(basename "$in" .fb2)
echo "TMPDIR=$TMPDIR" > /dev/null
[ -z "$API_KEY" ] || exit 9
echo converted > "$out/$base.epub"
"#;

#[tokio::test]
async fn success_streams_result_and_cleans_job_dir() {
    let (app, jobs) = setup("ok", OK_SCRIPT, 10);
    std::env::set_var("API_KEY", "must-not-leak");
    let resp = app.oneshot(req("epub", "k")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&body[..], b"converted\n");
    wait_empty(&jobs).await;
}

#[tokio::test]
async fn unauthorized_and_invalid_format() {
    let (app, _) = setup("auth", OK_SCRIPT, 10);
    let resp = app.clone().oneshot(req("epub", "bad")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let resp = app.oneshot(req("pdf", "k")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn nonzero_exit_is_422_and_cleans_job_dir() {
    let (app, jobs) = setup("fail", "echo boom >&2; exit 3", 10);
    let resp = app.oneshot(req("epub", "k")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    wait_empty(&jobs).await;
}

#[tokio::test]
async fn timeout_kills_process_group_and_cleans_job_dir() {
    // The shell spawns a background grandchild that records its pid, then both hang.
    let script = r#"sleep 300 &
echo $! > "$(dirname "$4")/child.pid"
wait"#;
    let (app, jobs) = setup("timeout", script, 1);
    let started = Instant::now();

    // Watch for the pid file while the request is in flight.
    let jobs2 = jobs.clone();
    let watcher = tokio::spawn(async move {
        for _ in 0..40 {
            if let Some(entry) = std::fs::read_dir(&jobs2).unwrap().flatten().next() {
                let p = entry.path().join("child.pid");
                if let Ok(s) = std::fs::read_to_string(&p) {
                    if let Ok(pid) = s.trim().parse::<i32>() {
                        return Some(pid);
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        None
    });

    let resp = app.oneshot(req("epub", "k")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::GATEWAY_TIMEOUT);
    assert!(started.elapsed() < Duration::from_secs(10));

    let pid = watcher.await.unwrap().expect("grandchild pid recorded");
    let proc_path = format!("/proc/{pid}");
    let mut gone = false;
    for _ in 0..50 {
        // A killed-but-unreaped zombie (reparented to init) still has /proc entry;
        // treat zombie state as dead.
        match std::fs::read_to_string(format!("{proc_path}/stat")) {
            Err(_) => {
                gone = true;
                break;
            }
            Ok(s)
                if s.rsplit(')')
                    .next()
                    .unwrap_or("")
                    .trim_start()
                    .starts_with('Z') =>
            {
                gone = true;
                break;
            }
            _ => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
    assert!(gone, "grandchild {pid} survived the timeout");
    wait_empty(&jobs).await;
}
