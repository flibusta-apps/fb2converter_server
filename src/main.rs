use std::{net::SocketAddr, time::SystemTime};
use axum::{Router, routing::{post, get}, extract::Multipart, response::{IntoResponse, AppendHeaders}, http::{StatusCode, header}, body::StreamBody};
use axum_prometheus::PrometheusMetricLayer;
use tokio::{fs::{remove_file, read_dir, remove_dir, File}, io::{AsyncWriteExt, copy}, process::Command};
use tower_http::trace::{TraceLayer, self};
use tracing::{info, log, Level};
use async_tempfile::TempFile;
use tokio_util::io::ReaderStream;


async fn remove_temp_files() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let _ = remove_file("./conversion.log").await?;

    let mut dir = read_dir("/tmp/").await?;

    let now = SystemTime::now();

    while let Some(child) = dir.next_entry().await? {
        let metadata = child.metadata().await?;

        if now.duration_since(metadata.modified()?)?.as_secs() < 60 * 60 * 3 {
            continue;
        }

        if metadata.is_dir() {
            let _ = remove_dir(child.path()).await;
        } else {
            let _ = remove_file(child.path()).await;
        }
    }

    Ok(())
}


async fn convert_file(
    mut multipart: Multipart
) -> impl IntoResponse {
    let mut file_format: Option<String> = None;

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

    while let Some(mut field) = multipart.next_field().await.unwrap() {
        let name = field.name().unwrap().to_string();

        match name.as_str() {
            "format" => {
                file_format = Some(field.text().await.unwrap());
            },
            "file" => {
                let mut tempfile_rw = match tempfile.open_rw().await {
                    Ok(v) => v,
                    Err(err) => {
                        log::error!("{:?}", err);
                        return StatusCode::INTERNAL_SERVER_ERROR.into_response()
                    },
                };

                while let Ok(result) = field.chunk().await {
                    let data = match result {
                        Some(v) => v,
                        None => break,
                    };

                    match tempfile_rw.write(data.as_ref()).await {
                        Ok(_) => (),
                        Err(err) => {
                            log::error!("{:?}", err);
                            return StatusCode::INTERNAL_SERVER_ERROR.into_response()
                        },
                    }
                }

                let _  =tempfile_rw.flush().await;
            },
            _ => panic!("unknown field")
        };
    }

    let file_format = file_format.unwrap();

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
        return StatusCode::BAD_REQUEST.into_response();
    }

    let result_tempfile = match TempFile::new().await {
        Ok(v) => v,
        Err(err) => {
            log::error!("{:?}", err);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response()
        },
    };

    let mut result_tempfile_rw = match result_tempfile.open_rw().await {
        Ok(v) => v,
        Err(err) => {
            log::error!("{:?}", err);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response()
        },
    };

    let mut result_file = match File::open(format!("{prefix}.{file_format}")).await {
        Ok(v) => v,
        Err(err) => {
            log::error!("{:?}", err);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response()
        },
    };

    let content_len = match copy(&mut result_file, &mut result_tempfile_rw).await {
        Ok(v) => v,
        Err(err) => {
            log::error!("{:?}", err);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response()
        },
    };

    let stream = ReaderStream::new(result_tempfile_rw);
    let body = StreamBody::new(stream);

    let headers = AppendHeaders([
        (
            header::CONTENT_LENGTH,
            content_len
        )
    ]);

    tokio::spawn(remove_temp_files());

    (headers, body).into_response()
}


fn get_router() -> Router {
    let (prometheus_layer, metric_handle) = PrometheusMetricLayer::pair();

    Router::new()
        .route("/", post(convert_file))
        .route("/metrics", get(|| async move { metric_handle.render() }))
        .layer(prometheus_layer)
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(trace::DefaultMakeSpan::new().level(Level::INFO))
                .on_response(trace::DefaultOnResponse::new().level(Level::INFO)),
        )
}


#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_target(false)
        .compact()
        .init();

    let addr = SocketAddr::from(([0, 0, 0, 0], 8080));

    let app = get_router();

    info!("Start webserver...");
    axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .await
        .unwrap();
    info!("Webserver shutdown...")
}
