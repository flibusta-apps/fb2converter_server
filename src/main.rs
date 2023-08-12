use std::{net::SocketAddr, str::FromStr, io::SeekFrom};
use axum::{Router, routing::{post, get}, extract::{Path, BodyStream}, response::{IntoResponse, AppendHeaders, Response}, http::{StatusCode, header, Request, self}, body::StreamBody, middleware::{Next, self}};
use axum_prometheus::PrometheusMetricLayer;
use futures_util::StreamExt;
use sentry::{ClientOptions, types::Dsn, integrations::debug_images::DebugImagesIntegration};
use tokio::{fs::{remove_file, read_dir, remove_dir, File}, io::{AsyncWriteExt, AsyncSeekExt}, process::Command};
use tokio_cron_scheduler::{JobScheduler, Job};
use tower_http::trace::{TraceLayer, self};
use tracing::{info, log, Level};
use async_tempfile::TempFile;
use tokio_util::io::ReaderStream;


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


async fn convert_file(
    Path(file_format): Path<String>,
    mut stream: BodyStream
) -> impl IntoResponse {
    let prefix = uuid::Uuid::new_v4().to_string();

    let tempfile = match TempFile::new_with_name(
        format!("{prefix}.fb2")
    ).await {
        Ok(v) => v,
        Err(err) => {
            log::error!("{:?}", err);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response()
        },
    };

    let mut tempfile_rw = match tempfile.open_rw().await {
        Ok(v) => v,
        Err(err) => {
            log::error!("{:?}", err);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response()
        },
    };

    while let Some(chunk) = stream.next().await {
        let data = match chunk {
            Ok(v) => v,
            Err(err) => {
                log::error!("{:?}", err);
                return StatusCode::INTERNAL_SERVER_ERROR.into_response()
            },
        };

        match tempfile_rw.write(data.as_ref()).await {
            Ok(_) => (),
            Err(err) => {
                log::error!("{:?}", err);
                return StatusCode::INTERNAL_SERVER_ERROR.into_response()
            },
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
        .await {
            Ok(v) => v,
            Err(err) => {
                log::error!("{:?}", err);
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            },
        };


    if status_code.code().unwrap() != 0 {
        log::error!("{:?}", status_code);
        return StatusCode::BAD_REQUEST.into_response();
    }

    let mut result_file = match File::open(format!("/tmp/{prefix}.{file_format}")).await {
        Ok(v) => v,
        Err(err) => {
            log::error!("{:?}", err);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response()
        },
    };

    let content_len = result_file.seek(SeekFrom::End(0)).await.unwrap();
    let _  = result_file.seek(SeekFrom::Start(0)).await;

    let stream = ReaderStream::new(result_file);
    let body = StreamBody::new(stream);

    let headers = AppendHeaders([
        (
            header::CONTENT_LENGTH,
            content_len
        )
    ]);

    (headers, body).into_response()
}


async fn auth<B>(req: Request<B>, next: Next<B>) -> Result<Response, StatusCode> {
    let auth_header = req
        .headers()
        .get(http::header::AUTHORIZATION)
        .and_then(|header| header.to_str().ok());

    let auth_header = if let Some(auth_header) = auth_header {
        auth_header
    } else {
        return Err(StatusCode::UNAUTHORIZED);
    };

    if auth_header != std::env::var("API_KEY").unwrap_or_else(|_| panic!("Cannot get the API_KEY env variable")) {
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

    let remote_temp_files_job = match Job::new_async("0 0 */6 * * *", |_uuid, _l| Box::pin(async {
        match remove_temp_files().await {
            Ok(_) => log::info!("Updated"),
            Err(err) => log::info!("Update err: {:?}", err),
        };
    })) {
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
    axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .await
        .unwrap();
    info!("Webserver shutdown...");
}


#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_target(false)
        .compact()
        .init();

    let options = ClientOptions {
        dsn: Some(Dsn::from_str(
            &std::env::var("SENTRY_DSN").unwrap_or_else(|_| panic!("Cannot get the SENTRY_DSN env variable"))).unwrap()
        ),
        default_integrations: false,
        ..Default::default()
    }
    .add_integration(DebugImagesIntegration::new());

    let _guard = sentry::init(options);

    tokio::join![
        cron_jobs(),
        start_app()
    ];
}
