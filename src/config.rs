use once_cell::sync::Lazy;

fn get_env(env: &'static str) -> Result<String, String> {
    std::env::var(env).map_err(|_| format!("Cannot get the {} env variable", env))
}

/// Like `get_env`, but returns `default` instead of erroring when the
/// variable is unset. Used for optional string configuration with a
/// sensible fallback. Not currently exercised (all current optional fields
/// are numeric), kept for parity with the sibling service's config helpers
/// and future string-valued options.
#[allow(dead_code)]
fn get_env_or(env: &'static str, default: &str) -> String {
    std::env::var(env).unwrap_or_else(|_| default.to_string())
}

/// Like `get_env`, but parses the value as a `usize`, falling back to
/// `default` when the variable is unset or fails to parse.
fn get_env_usize_or(env: &'static str, default: usize) -> usize {
    std::env::var(env)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(default)
}

/// Like `get_env_usize_or`, but parses the value as a `u64`.
fn get_env_u64_or(env: &'static str, default: u64) -> u64 {
    std::env::var(env)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(default)
}

pub struct Config {
    pub api_key: String,
    pub sentry_dsn: String,

    /// Maximum accepted request body size, in bytes. See `MAX_BODY_BYTES`.
    pub max_body_bytes: usize,

    /// Maximum time allowed for the `fb2c` child process to finish. See
    /// `FB2C_TIMEOUT_SECS`.
    pub fb2c_timeout_secs: u64,

    /// Maximum number of `fb2c` conversions allowed to run concurrently.
    /// See `MAX_CONCURRENT_CONVERSIONS`.
    pub max_concurrent_conversions: usize,

    /// Maximum size of a produced output file before it is rejected instead
    /// of streamed back. See `MAX_OUTPUT_BYTES`.
    pub max_output_bytes: u64,

    /// Minimum age (in seconds) a `/tmp` entry must have before the cleanup
    /// cron is allowed to delete it. See `CLEANUP_MAX_AGE_SECS`.
    pub cleanup_max_age_secs: u64,
}

impl Config {
    pub fn try_load() -> Result<Config, String> {
        Ok(Config {
            api_key: get_env("API_KEY")?,
            sentry_dsn: get_env("SENTRY_DSN")?,

            max_body_bytes: get_env_usize_or("MAX_BODY_BYTES", 50 * 1024 * 1024),
            fb2c_timeout_secs: get_env_u64_or("FB2C_TIMEOUT_SECS", 120),
            max_concurrent_conversions: get_env_usize_or("MAX_CONCURRENT_CONVERSIONS", 4),
            max_output_bytes: get_env_u64_or("MAX_OUTPUT_BYTES", 200 * 1024 * 1024),
            cleanup_max_age_secs: get_env_u64_or("CLEANUP_MAX_AGE_SECS", 3600),
        })
    }
}

pub static CONFIG: Lazy<Config> = Lazy::new(|| match Config::try_load() {
    Ok(config) => config,
    Err(e) => {
        eprintln!("Configuration error: {e}");
        std::process::exit(1);
    }
});
