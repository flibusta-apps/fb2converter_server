mod config;

use async_tempfile::TempFile;
use axum::{
    body::Body,
    extract::{DefaultBodyLimit, Path},
    http::{self, header, Request, StatusCode},
    middleware::{self, Next},
    response::{AppendHeaders, IntoResponse, Response},
    routing::{get, post},
    Router,
};
use axum_prometheus::PrometheusMetricLayer;
use config::CONFIG;
use futures_util::StreamExt;
use once_cell::sync::Lazy;
use sentry::{integrations::debug_images::DebugImagesIntegration, types::Dsn, ClientOptions};
use sentry_tracing::EventFilter;
use std::{net::SocketAddr, path::PathBuf, str::FromStr, time::Instant};
use tokio::{
    fs::{create_dir, read_dir, remove_dir_all, remove_file, File},
    io::{AsyncWriteExt, BufWriter},
    process::Command,
    sync::Semaphore,
};
use tokio_cron_scheduler::{Job, JobScheduler};
use tokio_util::io::ReaderStream;
use tower_http::{
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer},
    trace::{self, TraceLayer},
};
use tracing::{info, log, Level};
use tracing_subscriber::{filter, layer::SubscriberExt, util::SubscriberInitExt};

/// Canonical, lowercase output formats accepted by this service.
const ALLOWED_FORMATS: [&str; 2] = ["epub", "mobi"];

/// Bounds how many `fb2c` child processes may run concurrently.
static CONVERSION_SEMAPHORE: Lazy<Semaphore> =
    Lazy::new(|| Semaphore::new(CONFIG.max_concurrent_conversions));

/// Normalizes a requested output format to the canonical lowercase form,
/// returning `None` if it isn't one of `ALLOWED_FORMATS`.
fn normalize_format(s: &str) -> Option<&'static str> {
    let lower = s.to_lowercase();
    ALLOWED_FORMATS.iter().find(|f| **f == lower).copied()
}

/// Pure decision logic for the cleanup cron: should the `/tmp` entry named
/// `name` (a `{uuid}` directory or `{uuid}.fb2` file created by this
/// service) be deleted, given how long ago it was last modified?
///
/// Entries that don't parse as a UUID (optionally followed by `.fb2`) are
/// never touched, regardless of age. Entries that do parse are only
/// eligible once older than `max_age_secs`.
fn should_cleanup(name: &str, elapsed_secs: u64, max_age_secs: u64) -> bool {
    let candidate = name.strip_suffix(".fb2").unwrap_or(name);

    if uuid::Uuid::parse_str(candidate).is_err() {
        return false;
    }

    elapsed_secs > max_age_secs
}

async fn remove_temp_files() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut dir = read_dir("/tmp/").await?;

    while let Some(child) = dir.next_entry().await? {
        let file_name = child.file_name();
        let name = file_name.to_string_lossy();

        let metadata = child.metadata().await?;

        let elapsed_secs = match metadata.modified() {
            Ok(modified) => modified.elapsed().map(|d| d.as_secs()).unwrap_or_default(),
            Err(_) => 0,
        };

        if !should_cleanup(&name, elapsed_secs, CONFIG.cleanup_max_age_secs) {
            continue;
        }

        if metadata.is_dir() {
            let _ = remove_dir_all(child.path()).await;
        } else {
            let _ = remove_file(child.path()).await;
        }
    }

    Ok(())
}

async fn health_check() -> impl IntoResponse {
    (StatusCode::OK, "OK")
}

/// RAII guard that removes the conversion output directory when dropped,
/// unless `disarm()` has been called first. This guarantees the directory
/// is cleaned up on every early-return error path in `convert_file`, while
/// still letting the success path take ownership back and schedule its own
/// (post-streaming) removal.
struct OutputDirGuard(Option<PathBuf>);

impl OutputDirGuard {
    fn disarm(mut self) -> PathBuf {
        self.0.take().expect("OutputDirGuard already disarmed")
    }
}

impl Drop for OutputDirGuard {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            tokio::spawn(async move {
                let _ = tokio::fs::remove_dir_all(path).await;
            });
        }
    }
}

async fn convert_file(Path(file_format): Path<String>, body: Body) -> impl IntoResponse {
    // Spec 02.3: validate/normalize the format before touching disk at all.
    let fmt = match normalize_format(&file_format) {
        Some(f) => f,
        None => {
            metrics::counter!("fb2conversion_total", "outcome" => "bad_request", "format" => file_format.to_lowercase()).increment(1);
            return StatusCode::BAD_REQUEST.into_response();
        }
    };

    let prefix = uuid::Uuid::new_v4().to_string();

    let output_dir = PathBuf::from("/tmp").join(&prefix);
    if let Err(err) = create_dir(&output_dir).await {
        log::error!("Failed to create output directory: {:?}", err);
        metrics::counter!("fb2conversion_total", "outcome" => "error", "format" => fmt)
            .increment(1);
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let output_dir_guard = OutputDirGuard(Some(output_dir.clone()));

    let tempfile =
        match TempFile::new_with_name_in(format!("{prefix}.fb2"), std::path::Path::new("/tmp/"))
            .await
        {
            Ok(v) => v,
            Err(err) => {
                log::error!("{:?}", err);
                metrics::counter!("fb2conversion_total", "outcome" => "error", "format" => fmt)
                    .increment(1);
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        };

    let tempfile_rw = match tempfile.open_rw().await {
        Ok(v) => v,
        Err(err) => {
            log::error!("{:?}", err);
            metrics::counter!("fb2conversion_total", "outcome" => "error", "format" => fmt)
                .increment(1);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let mut writer = BufWriter::with_capacity(256 * 1024, tempfile_rw);

    let mut data_stream = body.into_data_stream();
    let mut total_bytes: usize = 0;

    while let Some(chunk) = data_stream.next().await {
        let data = match chunk {
            Ok(v) => v,
            Err(_err) => {
                metrics::counter!("fb2conversion_total", "outcome" => "bad_request", "format" => fmt).increment(1);
                return StatusCode::BAD_REQUEST.into_response();
            }
        };

        total_bytes += data.len();
        if total_bytes > CONFIG.max_body_bytes {
            log::warn!(
                "Request body exceeded max_body_bytes ({})",
                CONFIG.max_body_bytes
            );
            metrics::counter!("fb2conversion_total", "outcome" => "too_large", "format" => fmt)
                .increment(1);
            return StatusCode::PAYLOAD_TOO_LARGE.into_response();
        }

        match writer.write_all(data.as_ref()).await {
            Ok(_) => (),
            Err(err) => {
                log::error!("Failed to write to temp file: {:?}", err);
                metrics::counter!("fb2conversion_total", "outcome" => "error", "format" => fmt)
                    .increment(1);
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        }
    }

    if let Err(err) = writer.flush().await {
        log::error!("Failed to flush temp file: {:?}", err);
        metrics::counter!("fb2conversion_total", "outcome" => "error", "format" => fmt)
            .increment(1);
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    // Spec 01.3: bound concurrent fb2c child processes.
    let permit = match CONVERSION_SEMAPHORE.try_acquire() {
        Ok(p) => p,
        Err(_) => {
            log::warn!("Conversion concurrency limit reached, rejecting request");
            metrics::counter!("fb2conversion_total", "outcome" => "unavailable", "format" => fmt)
                .increment(1);
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    };

    let fb2c_started = Instant::now();

    let mut child = match Command::new("/app/bin/fb2c")
        .arg("convert")
        .arg("--to")
        .arg(fmt)
        .arg(tempfile.file_path())
        .arg(&output_dir)
        .kill_on_drop(true)
        .spawn()
    {
        Ok(v) => v,
        Err(err) => {
            drop(permit);
            log::error!("Failed to execute fb2c: {:?}", err);
            metrics::counter!("fb2conversion_total", "outcome" => "error", "format" => fmt)
                .increment(1);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let wait_result = tokio::time::timeout(
        std::time::Duration::from_secs(CONFIG.fb2c_timeout_secs),
        child.wait(),
    )
    .await;

    let status_code = match wait_result {
        Ok(Ok(status)) => {
            drop(permit);
            status
        }
        Ok(Err(err)) => {
            drop(permit);
            log::error!("Failed to wait for fb2c: {:?}", err);
            metrics::counter!("fb2conversion_total", "outcome" => "error", "format" => fmt)
                .increment(1);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        Err(_) => {
            log::error!(
                "fb2c timed out after {}s, killing",
                CONFIG.fb2c_timeout_secs
            );
            let _ = child.kill().await;
            drop(permit);
            metrics::counter!("fb2conversion_total", "outcome" => "timeout", "format" => fmt)
                .increment(1);
            metrics::histogram!("fb2conversion_duration_seconds", "format" => fmt)
                .record(fb2c_started.elapsed().as_secs_f64());
            return StatusCode::GATEWAY_TIMEOUT.into_response();
        }
    };

    metrics::histogram!("fb2conversion_duration_seconds", "format" => fmt)
        .record(fb2c_started.elapsed().as_secs_f64());

    match status_code.code() {
        Some(0) => {}
        Some(_) => {
            log::error!("{:?}", status_code);
            metrics::counter!("fb2conversion_total", "outcome" => "bad_request", "format" => fmt)
                .increment(1);
            return StatusCode::BAD_REQUEST.into_response();
        }
        None => {
            log::error!("Process terminated by signal: {:?}", status_code);
            metrics::counter!("fb2conversion_total", "outcome" => "error", "format" => fmt)
                .increment(1);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }

    // Spec 04.4: fb2c's output name is deterministic from the input name;
    // try the direct path first and only fall back to scanning the
    // directory (logging when that happens) if the assumption doesn't hold.
    let direct_path = output_dir.join(format!("{prefix}.{fmt}"));
    let result_path = match File::open(&direct_path).await {
        Ok(_) => direct_path,
        Err(_) => {
            log::warn!(
                "Expected output file {:?} not found, falling back to directory scan",
                direct_path
            );

            let mut dir = match read_dir(&output_dir).await {
                Ok(v) => v,
                Err(err) => {
                    log::error!("Failed to read output directory: {:?}", err);
                    metrics::counter!("fb2conversion_total", "outcome" => "error", "format" => fmt)
                        .increment(1);
                    return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                }
            };

            let mut found: Option<PathBuf> = None;
            loop {
                match dir.next_entry().await {
                    Ok(Some(entry)) => {
                        let path = entry.path();
                        if let Some(ext) = path.extension() {
                            if ext.eq_ignore_ascii_case(fmt) {
                                found = Some(path);
                                break;
                            }
                        }
                    }
                    Ok(None) => break,
                    Err(err) => {
                        log::error!("Failed to read output directory entry: {:?}", err);
                        metrics::counter!("fb2conversion_total", "outcome" => "error", "format" => fmt).increment(1);
                        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                    }
                }
            }

            match found {
                Some(p) => p,
                None => {
                    log::error!("No .{fmt} file found in output directory");
                    metrics::counter!("fb2conversion_total", "outcome" => "error", "format" => fmt)
                        .increment(1);
                    return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                }
            }
        }
    };

    let result_file = match File::open(&result_path).await {
        Ok(v) => v,
        Err(err) => {
            log::error!("Failed to open result file: {:?}", err);
            metrics::counter!("fb2conversion_total", "outcome" => "error", "format" => fmt)
                .increment(1);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let content_len = match result_file.metadata().await {
        Ok(meta) => meta.len(),
        Err(err) => {
            log::error!("Failed to stat result file: {:?}", err);
            metrics::counter!("fb2conversion_total", "outcome" => "error", "format" => fmt)
                .increment(1);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    // Spec 01.4: don't stream back an oversized output file.
    if content_len > CONFIG.max_output_bytes {
        log::warn!(
            "Output file size {} exceeds max_output_bytes {}",
            content_len,
            CONFIG.max_output_bytes
        );
        metrics::counter!("fb2conversion_total", "outcome" => "error", "format" => fmt)
            .increment(1);
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    let stream = ReaderStream::with_capacity(result_file, 64 * 1024);

    let headers = AppendHeaders([(header::CONTENT_LENGTH, content_len)]);

    // Clean up output directory after streaming starts. Take ownership of
    // the path back out of the guard so its `Drop` impl doesn't also try
    // to remove it.
    let output_dir_owned = output_dir_guard.disarm();
    tokio::spawn(async move {
        let _ = remove_dir_all(&output_dir_owned).await;
    });

    metrics::counter!("fb2conversion_total", "outcome" => "success", "format" => fmt).increment(1);

    (headers, Body::from_stream(stream)).into_response()
}

/// Pure comparison used by the `auth` middleware: `true` only when a header
/// value was present and matches `expected_key` exactly.
fn is_authorized(auth_header: Option<&str>, expected_key: &str) -> bool {
    match auth_header {
        Some(h) => h == expected_key,
        None => false,
    }
}

async fn auth(req: Request<axum::body::Body>, next: Next) -> Result<Response, StatusCode> {
    let auth_header = req
        .headers()
        .get(http::header::AUTHORIZATION)
        .and_then(|header| header.to_str().ok());

    if !is_authorized(auth_header, &CONFIG.api_key) {
        return Err(StatusCode::UNAUTHORIZED);
    }

    Ok(next.run(req).await)
}

fn get_router() -> Router {
    let (prometheus_layer, metric_handle) = PrometheusMetricLayer::pair();

    let app_router = Router::new()
        .route("/{file_format}", post(convert_file))
        .layer(middleware::from_fn(auth))
        .layer(DefaultBodyLimit::max(CONFIG.max_body_bytes))
        .layer(prometheus_layer);

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

async fn cron_jobs() {
    let job_scheduler = JobScheduler::new().await.unwrap();

    let remote_temp_files_job = match Job::new_async("0 0 */6 * * *", |_uuid, _l| {
        Box::pin(async {
            match remove_temp_files().await {
                Ok(_) => log::info!("Temp files deleted!"),
                Err(err) => log::info!("Temp files deleting error: {:?}", err),
            };
        })
    }) {
        Ok(v) => v,
        Err(err) => panic!("{:?}", err),
    };

    job_scheduler.add(remote_temp_files_job).await.unwrap();

    log::info!("Scheduler start...");
    match job_scheduler.start().await {
        Ok(v) => v,
        Err(err) => panic!("{:?}", err),
    };

    log::info!("Scheduler shutdown...");
}

/// Resolves once SIGINT (Ctrl-C) or, on unix, SIGTERM is received, letting
/// `axum::serve` drain in-flight requests before shutting down.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    log::info!("Shutdown signal received, draining in-flight requests...");
}

async fn start_app() {
    let addr = SocketAddr::from(([0, 0, 0, 0], 8080));

    let app = get_router();

    info!("Start webserver...");
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .unwrap();
    info!("Webserver shutdown...");
}

#[tokio::main]
async fn main() {
    // Force config validation at boot rather than on first use.
    Lazy::force(&CONFIG);

    let options = ClientOptions {
        dsn: Some(Dsn::from_str(&CONFIG.sentry_dsn).unwrap()),
        default_integrations: false,
        ..Default::default()
    }
    .add_integration(DebugImagesIntegration::new());

    let _guard = sentry::init(options);

    let sentry_layer = sentry_tracing::layer().event_filter(|md| match md.level() {
        &tracing::Level::ERROR => EventFilter::Event,
        _ => EventFilter::Ignore,
    });

    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_target(false))
        .with(filter::LevelFilter::INFO)
        .with(sentry_layer)
        .init();

    tokio::join![cron_jobs(), start_app()];
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
    }

    #[test]
    fn is_authorized_rejects_missing_header() {
        assert!(!is_authorized(None, "secret"));
    }
}
