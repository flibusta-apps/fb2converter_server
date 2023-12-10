use async_tempfile::TempFile;
use axum::{
    body::Body,
    extract::Path,
    http::{self, header, Request, StatusCode},
    middleware::{self, Next},
    response::{AppendHeaders, IntoResponse, Response},
    routing::{get, post},
    Router,
};
use axum_prometheus::PrometheusMetricLayer;
use futures_util::StreamExt;
use sentry::{integrations::debug_images::DebugImagesIntegration, types::Dsn, ClientOptions};
use std::{io::SeekFrom, net::SocketAddr, str::FromStr};
use tokio::{
    fs::{read_dir, remove_dir, remove_file, File},
    io::{AsyncSeekExt, AsyncWriteExt},
    process::Command,
};
use tokio_cron_scheduler::{Job, JobScheduler};
use tokio_util::io::ReaderStream;
use tower_http::trace::{self, TraceLayer};
use tracing::{info, log, Level};

async fn remove_temp_files() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut dir = read_dir("/tmp/").await?;

    while let Some(child) = dir.next_entry().await? {
        let metadata = child.metadata().await?;

        if metadata.is_dir() {
            let _ = remove_dir(child.path()).await;
        } else {
            let _ = remove_file(child.path()).await;
        }
    }

    Ok(())
}

async fn convert_file(Path(file_format): Path<String>, body: Body) -> impl IntoResponse {
    let prefix = uuid::Uuid::new_v4().to_string();

    let tempfile = match TempFile::new_with_name(format!("{prefix}.fb2")).await {
        Ok(v) => v,
        Err(err) => {
            log::error!("{:?}", err);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let mut tempfile_rw = match tempfile.open_rw().await {
        Ok(v) => v,
        Err(err) => {
            log::error!("{:?}", err);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let mut data_stream = body.into_data_stream();

    while let Some(chunk) = data_stream.next().await {
        let data = match chunk {
            Ok(v) => v,
            Err(err) => {
                log::error!("{:?}", err);
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        };

        match tempfile_rw.write(data.as_ref()).await {
            Ok(_) => (),
            Err(err) => {
                log::error!("{:?}", err);
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        }
    }

    let _ = tempfile_rw.flush().await;

    let allowed_formats = vec!["epub".to_string(), "mobi".to_string()];
    if !allowed_formats.contains(&file_format.clone().to_lowercase()) {
        return StatusCode::BAD_REQUEST.into_response();
    }

    let status_code = match Command::new("/app/bin/fb2c")
        .current_dir("/tmp/")
        .arg("convert")
        .arg("--to")
        .arg(&file_format)
        .arg(tempfile.file_path())
        .status()
        .await
    {
        Ok(v) => v,
        Err(err) => {
            log::error!("{:?}", err);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    if status_code.code().unwrap() != 0 {
        log::error!("{:?}", status_code);
        return StatusCode::BAD_REQUEST.into_response();
    }

    let mut result_file = match File::open(format!("/tmp/{prefix}.{file_format}")).await {
        Ok(v) => v,
        Err(err) => {
            log::error!("{:?}", err);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let content_len = result_file.seek(SeekFrom::End(0)).await.unwrap();
    let _ = result_file.seek(SeekFrom::Start(0)).await;

    let stream = ReaderStream::new(result_file);

    let headers = AppendHeaders([(header::CONTENT_LENGTH, content_len)]);

    (headers, Body::from_stream(stream)).into_response()
}

async fn auth(req: Request<axum::body::Body>, next: Next) -> Result<Response, StatusCode> {
    let auth_header = req
        .headers()
        .get(http::header::AUTHORIZATION)
        .and_then(|header| header.to_str().ok());

    let auth_header = if let Some(auth_header) = auth_header {
        auth_header
    } else {
        return Err(StatusCode::UNAUTHORIZED);
    };

    if auth_header
        != std::env::var("API_KEY")
            .unwrap_or_else(|_| panic!("Cannot get the API_KEY env variable"))
    {
        return Err(StatusCode::UNAUTHORIZED);
    }

    Ok(next.run(req).await)
}

fn get_router() -> Router {
    let (prometheus_layer, metric_handle) = PrometheusMetricLayer::pair();

    let app_router = Router::new()
        .route("/:file_format", post(convert_file))
        .layer(middleware::from_fn(auth))
        .layer(prometheus_layer);

    let metric_router =
        Router::new().route("/metrics", get(|| async move { metric_handle.render() }));

    Router::new()
        .nest("/", app_router)
        .nest("/", metric_router)
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(trace::DefaultMakeSpan::new().level(Level::INFO))
                .on_response(trace::DefaultOnResponse::new().level(Level::INFO)),
        )
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

async fn start_app() {
    let addr = SocketAddr::from(([0, 0, 0, 0], 8080));

    let app = get_router();

    info!("Start webserver...");
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
    info!("Webserver shutdown...");
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_target(false)
        .compact()
        .init();

    let options = ClientOptions {
        dsn: Some(
            Dsn::from_str(
                &std::env::var("SENTRY_DSN")
                    .unwrap_or_else(|_| panic!("Cannot get the SENTRY_DSN env variable")),
            )
            .unwrap(),
        ),
        default_integrations: false,
        ..Default::default()
    }
    .add_integration(DebugImagesIntegration::new());

    let _guard = sentry::init(options);

    tokio::join![cron_jobs(), start_app()];
}
