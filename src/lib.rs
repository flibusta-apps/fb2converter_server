pub mod config;

use axum::{
    body::Body,
    extract::{Path, State},
    http::{self, header, Request, StatusCode},
    middleware::{self, Next},
    response::{AppendHeaders, IntoResponse, Response},
    routing::{get, post},
    Router,
};
use axum_prometheus::PrometheusMetricLayer;
use config::Config;
use futures_util::StreamExt;
use std::{
    path::PathBuf,
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    fs::{create_dir_all, read_dir, remove_dir_all, remove_file, File},
    io::{AsyncReadExt, AsyncWriteExt, BufWriter},
    process::Command,
    sync::Semaphore,
};
use tokio_util::io::ReaderStream;
use tower_http::{
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer},
    trace::{self, TraceLayer},
};
use tracing::{error, warn, Level};

/// Canonical, lowercase output formats accepted by this service.
const ALLOWED_FORMATS: [&str; 2] = ["epub", "mobi"];

/// Maximum number of bytes of `fb2c` stderr that are captured and logged.
const STDERR_CAP: usize = 8 * 1024;

/// Shared application state.
pub struct AppState {
    pub config: Config,
    /// Bounds how many `fb2c` child processes may run concurrently.
    pub semaphore: Semaphore,
}

impl AppState {
    pub fn new(config: Config) -> Arc<AppState> {
        let semaphore = Semaphore::new(config.max_concurrent_conversions);
        Arc::new(AppState { config, semaphore })
    }
}

/// Normalizes a requested output format to the canonical lowercase form,
/// returning `None` if it isn't one of `ALLOWED_FORMATS`.
pub fn normalize_format(s: &str) -> Option<&'static str> {
    let lower = s.to_lowercase();
    ALLOWED_FORMATS.iter().find(|f| **f == lower).copied()
}

/// Pure decision logic for the cleanup cron: should the jobs-dir entry named
/// `name` (a `{uuid}` directory or legacy `{uuid}.fb2` file created by this
/// service) be deleted, given how long ago it was last modified?
///
/// Entries that don't parse as a UUID (optionally followed by `.fb2`) are
/// never touched, regardless of age.
pub fn should_cleanup(name: &str, elapsed_secs: u64, max_age_secs: u64) -> bool {
    let candidate = name.strip_suffix(".fb2").unwrap_or(name);

    if uuid::Uuid::parse_str(candidate).is_err() {
        return false;
    }

    elapsed_secs > max_age_secs
}

/// Removes stale job entries. Entries that vanish concurrently are skipped;
/// other per-entry errors are logged and do not abort the sweep. Returns the
/// number of removed entries.
pub async fn remove_temp_files(
    config: &Config,
) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    let mut dir = read_dir(&config.jobs_dir).await?;
    let mut removed = 0usize;

    while let Some(child) = dir.next_entry().await? {
        let file_name = child.file_name();
        let name = file_name.to_string_lossy();

        let metadata = match child.metadata().await {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                error!("Cleanup: failed to stat {:?}: {:?}", child.path(), e);
                continue;
            }
        };

        let elapsed_secs = match metadata.modified() {
            Ok(modified) => modified.elapsed().map(|d| d.as_secs()).unwrap_or_default(),
            Err(_) => 0,
        };

        if !should_cleanup(&name, elapsed_secs, config.cleanup_max_age_secs) {
            continue;
        }

        let res = if metadata.is_dir() {
            remove_dir_all(child.path()).await
        } else {
            remove_file(child.path()).await
        };

        match res {
            Ok(()) => removed += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => error!("Cleanup: failed to remove {:?}: {:?}", child.path(), e),
        }
    }

    Ok(removed)
}

async fn health_check() -> impl IntoResponse {
    (StatusCode::OK, "OK")
}

/// Kills an entire process group (SIGKILL). Errors (e.g. ESRCH) are ignored.
fn kill_group(pgid: i32) {
    // SAFETY: killpg has no memory-safety preconditions.
    unsafe {
        libc::killpg(pgid, libc::SIGKILL);
    }
}

/// RAII guard for a conversion job: on drop (any exit path, including the
/// handler future being cancelled) it kills the fb2c process group if one is
/// registered and removes the whole job directory — unless `disarm()` was
/// called, in which case the caller takes over directory removal.
struct JobGuard {
    dir: Option<PathBuf>,
    pgid: Option<i32>,
}

impl JobGuard {
    fn disarm(mut self) -> PathBuf {
        self.pgid = None;
        self.dir.take().expect("JobGuard already disarmed")
    }
}

impl Drop for JobGuard {
    fn drop(&mut self) {
        if let Some(pgid) = self.pgid.take() {
            kill_group(pgid);
        }
        if let Some(path) = self.dir.take() {
            // Handler may be dropped outside a runtime in odd cases; fall
            // back to a blocking removal then.
            match tokio::runtime::Handle::try_current() {
                Ok(h) => {
                    h.spawn(async move {
                        let _ = tokio::fs::remove_dir_all(path).await;
                    });
                }
                Err(_) => {
                    let _ = std::fs::remove_dir_all(path);
                }
            }
        }
    }
}

fn count(outcome: &'static str, format: &'static str) {
    metrics::counter!("fb2conversion_total", "outcome" => outcome, "format" => format).increment(1);
}

/// Reads up to `STDERR_CAP` bytes from `r`, then drains (discards) the rest
/// so the child never blocks on a full pipe.
async fn capture_stderr(mut r: tokio::process::ChildStderr) -> String {
    let mut buf = Vec::with_capacity(1024);
    let _ = (&mut r).take(STDERR_CAP as u64).read_to_end(&mut buf).await;
    let _ = tokio::io::copy(&mut r, &mut tokio::io::sink()).await;
    String::from_utf8_lossy(&buf).into_owned()
}

/// Spools the request body to `path`, enforcing `max_bytes`.
async fn spool_body(
    body: Body,
    path: &std::path::Path,
    max_bytes: usize,
) -> Result<(), StatusCode> {
    let file = File::create(path).await.map_err(|err| {
        error!("Failed to create input file: {:?}", err);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let mut writer = BufWriter::with_capacity(256 * 1024, file);

    let mut data_stream = body.into_data_stream();
    let mut total_bytes: usize = 0;

    while let Some(chunk) = data_stream.next().await {
        let data = chunk.map_err(|_| StatusCode::BAD_REQUEST)?;

        total_bytes += data.len();
        if total_bytes > max_bytes {
            warn!("Request body exceeded max_body_bytes ({})", max_bytes);
            return Err(StatusCode::PAYLOAD_TOO_LARGE);
        }

        writer.write_all(data.as_ref()).await.map_err(|err| {
            error!("Failed to write to input file: {:?}", err);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    }

    writer.flush().await.map_err(|err| {
        error!("Failed to flush input file: {:?}", err);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    Ok(())
}

async fn find_result_file(
    output_dir: &std::path::Path,
    prefix: &str,
    fmt: &str,
) -> Result<Option<File>, std::io::Error> {
    // fb2c's output name is deterministic from the input name; fall back to
    // scanning the directory only if the assumption doesn't hold.
    let direct_path = output_dir.join(format!("{prefix}.{fmt}"));
    match File::open(&direct_path).await {
        Ok(f) => return Ok(Some(f)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }

    warn!(
        "Expected output file {:?} not found, falling back to directory scan",
        direct_path
    );

    let mut dir = read_dir(output_dir).await?;
    while let Some(entry) = dir.next_entry().await? {
        let path = entry.path();
        if let Some(ext) = path.extension() {
            if ext.eq_ignore_ascii_case(fmt) {
                return Ok(Some(File::open(&path).await?));
            }
        }
    }
    Ok(None)
}

async fn convert_file(
    State(state): State<Arc<AppState>>,
    Path(file_format): Path<String>,
    body: Body,
) -> Response {
    let cfg = &state.config;

    let fmt = match normalize_format(&file_format) {
        Some(f) => f,
        None => {
            count("bad_request", "invalid");
            return StatusCode::BAD_REQUEST.into_response();
        }
    };

    // Acquire a concurrency permit BEFORE spooling the body, waiting up to
    // `queue_wait_secs`.
    let permit = match tokio::time::timeout(
        Duration::from_secs(cfg.queue_wait_secs),
        state.semaphore.acquire(),
    )
    .await
    {
        Ok(Ok(p)) => p,
        _ => {
            warn!(
                "Conversion concurrency limit reached after waiting {}s",
                cfg.queue_wait_secs
            );
            count("unavailable", fmt);
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    };

    let prefix = uuid::Uuid::new_v4().to_string();
    let job_dir = cfg.jobs_dir.join(&prefix);
    let output_dir = job_dir.join("out");
    let tmp_dir = job_dir.join("tmp");
    let input_path = job_dir.join(format!("{prefix}.fb2"));

    if let Err(err) = create_dir_all(&output_dir).await {
        error!("Failed to create job directory: {:?}", err);
        count("error", fmt);
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let mut guard = JobGuard {
        dir: Some(job_dir.clone()),
        pgid: None,
    };
    if let Err(err) = create_dir_all(&tmp_dir).await {
        error!("Failed to create job tmp directory: {:?}", err);
        count("error", fmt);
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    match tokio::time::timeout(
        Duration::from_secs(cfg.body_read_timeout_secs),
        spool_body(body, &input_path, cfg.max_body_bytes),
    )
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(status)) => {
            count(
                match status {
                    StatusCode::PAYLOAD_TOO_LARGE => "too_large",
                    StatusCode::BAD_REQUEST => "bad_request",
                    _ => "error",
                },
                fmt,
            );
            return status.into_response();
        }
        Err(_) => {
            warn!("Request body read timed out");
            count("body_timeout", fmt);
            return StatusCode::REQUEST_TIMEOUT.into_response();
        }
    }

    let path_env = std::env::var_os("PATH").unwrap_or_default();
    let home_env = std::env::var_os("HOME").unwrap_or_else(|| "/tmp".into());

    let fb2c_started = Instant::now();

    let mut child = match Command::new(&cfg.fb2c_bin)
        .arg("convert")
        .arg("--to")
        .arg(fmt)
        .arg(&input_path)
        .arg(&output_dir)
        .env_clear()
        .env("PATH", path_env)
        .env("HOME", home_env)
        .env("TMPDIR", &tmp_dir)
        .current_dir(&job_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
    {
        Ok(v) => v,
        Err(err) => {
            error!("Failed to execute fb2c: {:?}", err);
            count("error", fmt);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    // process_group(0) makes the child's pid its pgid.
    guard.pgid = child.id().map(|p| p as i32);

    let stderr_task = child.stderr.take().map(|s| tokio::spawn(capture_stderr(s)));

    let wait_result =
        tokio::time::timeout(Duration::from_secs(cfg.fb2c_timeout_secs), child.wait()).await;

    let record_duration = || {
        metrics::histogram!("fb2conversion_duration_seconds", "format" => fmt)
            .record(fb2c_started.elapsed().as_secs_f64());
    };

    let exit_status = match wait_result {
        Ok(Ok(status)) => status,
        Ok(Err(err)) => {
            error!("Failed to wait for fb2c: {:?}", err);
            count("error", fmt);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        Err(_) => {
            error!(
                "fb2c timed out after {}s, killing process group",
                cfg.fb2c_timeout_secs
            );
            if let Some(pgid) = guard.pgid {
                kill_group(pgid);
            }
            let _ = child.kill().await; // also reaps
            record_duration();
            count("timeout", fmt);
            return StatusCode::GATEWAY_TIMEOUT.into_response();
        }
    };

    record_duration();

    let stderr_text = match stderr_task {
        Some(t) => tokio::time::timeout(Duration::from_secs(2), t)
            .await
            .ok()
            .and_then(|r| r.ok())
            .unwrap_or_default(),
        None => String::new(),
    };

    // The child has been reaped; stop tracking the group so drop doesn't
    // signal a possibly reused pgid.
    guard.pgid = None;

    match exit_status.code() {
        Some(0) => {}
        Some(code) => {
            error!("fb2c exited with code {}: {}", code, stderr_text.trim());
            count("conversion_failed", fmt);
            return (StatusCode::UNPROCESSABLE_ENTITY, "conversion_failed").into_response();
        }
        None => {
            error!(
                "fb2c terminated by signal ({:?}): {}",
                exit_status,
                stderr_text.trim()
            );
            count("error", fmt);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }

    // Open the result exactly once.
    let result_file = match find_result_file(&output_dir, &prefix, fmt).await {
        Ok(Some(f)) => f,
        Ok(None) => {
            error!("No .{fmt} file found in output directory");
            count("error", fmt);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        Err(err) => {
            error!("Failed to open result file: {:?}", err);
            count("error", fmt);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let content_len = match result_file.metadata().await {
        Ok(meta) => meta.len(),
        Err(err) => {
            error!("Failed to stat result file: {:?}", err);
            count("error", fmt);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    if content_len > cfg.max_output_bytes {
        warn!(
            "Output file size {} exceeds max_output_bytes {}",
            content_len, cfg.max_output_bytes
        );
        count("error", fmt);
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    // Conversion is done; free the slot before streaming.
    drop(permit);

    let stream = ReaderStream::with_capacity(result_file, 64 * 1024);
    let headers = AppendHeaders([(header::CONTENT_LENGTH, content_len)]);

    // The open file handle stays valid after unlinking, so the whole job
    // directory can be removed right away.
    let job_dir_owned = guard.disarm();
    tokio::spawn(async move {
        let _ = remove_dir_all(&job_dir_owned).await;
    });

    count("success", fmt);

    (headers, Body::from_stream(stream)).into_response()
}

/// Constant-time byte-string equality (length is not hidden, content is).
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    let n = a.len().max(b.len());
    for i in 0..n {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= (x ^ y) as usize;
    }
    std::hint::black_box(diff) == 0
}

/// `true` only when a header value was present and matches `expected_key`.
pub fn is_authorized(auth_header: Option<&str>, expected_key: &str) -> bool {
    match auth_header {
        Some(h) => ct_eq(h.as_bytes(), expected_key.as_bytes()),
        None => false,
    }
}

async fn auth(
    State(state): State<Arc<AppState>>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    let auth_header = req
        .headers()
        .get(http::header::AUTHORIZATION)
        .and_then(|header| header.to_str().ok());

    if !is_authorized(auth_header, &state.config.api_key) {
        return Err(StatusCode::UNAUTHORIZED);
    }

    Ok(next.run(req).await)
}

/// The authenticated conversion API (no metrics / tracing layers).
pub fn api_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/{file_format}", post(convert_file))
        .layer(middleware::from_fn_with_state(state.clone(), auth))
        .with_state(state)
}

pub fn get_router(state: Arc<AppState>) -> Router {
    let (prometheus_layer, metric_handle) = PrometheusMetricLayer::pair();

    let app_router = api_router(state).layer(prometheus_layer);

    let metric_router =
        Router::new().route("/metrics", get(|| async move { metric_handle.render() }));

    let health_router = Router::new().route("/health", get(health_check));

    Router::new()
        .merge(app_router)
        .merge(metric_router)
        .merge(health_router)
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(trace::DefaultMakeSpan::new().level(Level::INFO))
                .on_response(trace::DefaultOnResponse::new().level(Level::INFO)),
        )
        .layer(PropagateRequestIdLayer::x_request_id())
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_format_accepts_known_formats_case_insensitively() {
        assert_eq!(normalize_format("epub"), Some("epub"));
        assert_eq!(normalize_format("EPUB"), Some("epub"));
        assert_eq!(normalize_format("Mobi"), Some("mobi"));
    }

    #[test]
    fn normalize_format_rejects_unknown_formats() {
        assert_eq!(normalize_format("pdf"), None);
        assert_eq!(normalize_format(""), None);
    }

    #[test]
    fn should_cleanup_skips_young_uuid_entries() {
        let name = uuid::Uuid::new_v4().to_string();
        assert!(!should_cleanup(&name, 10, 3600));
    }

    #[test]
    fn should_cleanup_deletes_old_uuid_dir_entries() {
        let name = uuid::Uuid::new_v4().to_string();
        assert!(should_cleanup(&name, 7200, 3600));
    }

    #[test]
    fn should_cleanup_deletes_old_uuid_fb2_files() {
        let name = format!("{}.fb2", uuid::Uuid::new_v4());
        assert!(should_cleanup(&name, 7200, 3600));
    }

    #[test]
    fn should_cleanup_never_deletes_non_uuid_entries() {
        assert!(!should_cleanup("some-file", 7200, 3600));
        assert!(!should_cleanup(".bashrc", 7200, 3600));
        assert!(!should_cleanup("hostname-thing", 7200, 3600));
    }

    #[test]
    fn is_authorized_accepts_matching_header() {
        assert!(is_authorized(Some("secret"), "secret"));
    }

    #[test]
    fn is_authorized_rejects_wrong_header() {
        assert!(!is_authorized(Some("wrong"), "secret"));
        assert!(!is_authorized(Some("secre"), "secret"));
        assert!(!is_authorized(Some("secrets"), "secret"));
    }

    #[test]
    fn is_authorized_rejects_missing_header() {
        assert!(!is_authorized(None, "secret"));
    }
}
