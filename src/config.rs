use std::path::PathBuf;
use std::str::FromStr;

fn get_env(env: &'static str) -> Result<String, String> {
    std::env::var(env).map_err(|_| format!("Cannot get the {} env variable", env))
}

/// Parses an optional numeric env variable. Unset (or empty) falls back to
/// `default`; a set-but-unparsable value is an error.
fn get_env_parsed_or<T: FromStr>(env: &'static str, default: T) -> Result<T, String> {
    match std::env::var(env) {
        Ok(v) if !v.trim().is_empty() => v
            .trim()
            .parse::<T>()
            .map_err(|_| format!("Invalid value for the {} env variable: {:?}", env, v)),
        _ => Ok(default),
    }
}

fn get_env_positive_u64_or(env: &'static str, default: u64) -> Result<u64, String> {
    let v = get_env_parsed_or::<u64>(env, default)?;
    if v == 0 {
        return Err(format!("The {} env variable must be greater than 0", env));
    }
    Ok(v)
}

pub struct Config {
    pub api_key: String,
    /// Sentry DSN; `None` (variable unset or empty) disables Sentry.
    pub sentry_dsn: Option<String>,

    /// Path to the `fb2c` binary. See `FB2C_BIN`.
    pub fb2c_bin: PathBuf,

    /// Directory in which per-job directories are created. See `JOBS_DIR`.
    pub jobs_dir: PathBuf,

    /// Maximum accepted request body size, in bytes. See `MAX_BODY_BYTES`.
    pub max_body_bytes: usize,

    /// Maximum time allowed for reading the request body. See
    /// `BODY_READ_TIMEOUT_SECS`.
    pub body_read_timeout_secs: u64,

    /// Maximum time allowed for the `fb2c` child process to finish. See
    /// `FB2C_TIMEOUT_SECS`.
    pub fb2c_timeout_secs: u64,

    /// Maximum number of `fb2c` conversions allowed to run concurrently.
    /// See `MAX_CONCURRENT_CONVERSIONS`.
    pub max_concurrent_conversions: usize,

    /// How long a request waits for a free conversion slot before a 503.
    /// See `QUEUE_WAIT_SECS`.
    pub queue_wait_secs: u64,

    /// Maximum size of a produced output file before it is rejected instead
    /// of streamed back. See `MAX_OUTPUT_BYTES`.
    pub max_output_bytes: u64,

    /// Minimum age (in seconds) a job entry must have before the cleanup
    /// cron is allowed to delete it. See `CLEANUP_MAX_AGE_SECS`.
    pub cleanup_max_age_secs: u64,
}

impl Config {
    pub fn try_load() -> Result<Config, String> {
        let api_key = get_env("API_KEY")?;
        if api_key.trim().is_empty() {
            return Err("The API_KEY env variable must not be empty".to_string());
        }

        let sentry_dsn = std::env::var("SENTRY_DSN")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());

        let max_concurrent_conversions =
            get_env_parsed_or::<usize>("MAX_CONCURRENT_CONVERSIONS", 4)?;
        if max_concurrent_conversions == 0 {
            return Err(
                "The MAX_CONCURRENT_CONVERSIONS env variable must be greater than 0".into(),
            );
        }

        Ok(Config {
            api_key,
            sentry_dsn,

            fb2c_bin: PathBuf::from(
                std::env::var("FB2C_BIN").unwrap_or_else(|_| "/app/bin/fb2c".to_string()),
            ),
            jobs_dir: PathBuf::from(
                std::env::var("JOBS_DIR").unwrap_or_else(|_| "/tmp".to_string()),
            ),

            max_body_bytes: get_env_parsed_or("MAX_BODY_BYTES", 50 * 1024 * 1024)?,
            body_read_timeout_secs: get_env_positive_u64_or("BODY_READ_TIMEOUT_SECS", 60)?,
            fb2c_timeout_secs: get_env_positive_u64_or("FB2C_TIMEOUT_SECS", 90)?,
            max_concurrent_conversions,
            queue_wait_secs: get_env_parsed_or("QUEUE_WAIT_SECS", 20)?,
            max_output_bytes: get_env_parsed_or("MAX_OUTPUT_BYTES", 200 * 1024 * 1024)?,
            cleanup_max_age_secs: get_env_parsed_or("CLEANUP_MAX_AGE_SECS", 3600)?,
        })
    }
}
