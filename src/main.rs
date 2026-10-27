use fb2converter_server::{config::Config, get_router, remove_temp_files, AppState};
use sentry::{types::Dsn, ClientOptions};
use sentry_tracing::EventFilter;
use std::{net::SocketAddr, str::FromStr, sync::Arc};
use tokio_cron_scheduler::{Job, JobScheduler};
use tracing::{error, info};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

async fn start_scheduler(state: Arc<AppState>) -> JobScheduler {
    let scheduler = JobScheduler::new().await.expect("scheduler init");

    let job = Job::new_async("0 0 */6 * * *", move |_uuid, _l| {
        let state = state.clone();
        Box::pin(async move {
            match remove_temp_files(&state.config).await {
                Ok(n) => info!("Temp files cleanup finished, removed {n} entries"),
                Err(err) => error!("Temp files cleanup error: {:?}", err),
            }
        })
    })
    .expect("cleanup job");

    scheduler.add(job).await.expect("add cleanup job");

    info!("Scheduler start...");
    scheduler.start().await.expect("scheduler start");
    scheduler
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

    info!("Shutdown signal received, draining in-flight requests...");
}

#[tokio::main]
async fn main() {
    let config = match Config::try_load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Configuration error: {e}");
            std::process::exit(1);
        }
    };

    let dsn = config
        .sentry_dsn
        .as_deref()
        .map(|d| match Dsn::from_str(d) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("Configuration error: invalid SENTRY_DSN: {e}");
                std::process::exit(1);
            }
        });

    // Default integrations (including panic reporting) stay enabled.
    let _guard = sentry::init(ClientOptions {
        dsn,
        ..Default::default()
    });

    let sentry_layer = sentry_tracing::layer().event_filter(|md| match md.level() {
        &tracing::Level::ERROR => EventFilter::Event,
        _ => EventFilter::Ignore,
    });

    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(tracing_subscriber::fmt::layer().with_target(false))
        .with(sentry_layer)
        .init();

    let state = AppState::new(config);

    let mut scheduler = start_scheduler(state.clone()).await;

    let addr = SocketAddr::from(([0, 0, 0, 0], 8080));
    let app = get_router(state);

    info!("Start webserver...");
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .unwrap();
    info!("Webserver shutdown...");

    info!("Scheduler shutdown...");
    if let Err(err) = scheduler.shutdown().await {
        error!("Scheduler shutdown error: {:?}", err);
    }
}
