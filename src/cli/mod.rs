pub mod args;
pub mod pipeline;
pub mod progress;
pub mod ui;
mod writer;

pub use args::Args;
pub use pipeline::{run_download_pipeline, run_list_versions_pipeline};
use indicatif::MultiProgress;
use steam_vent_cdn::config::{Config, Credentials, RetryPolicy, ServerMode};
use steam_vent_cdn::error::{Result, SteamVentError};
use tracing_subscriber::Layer;
use tracing_subscriber::prelude::*;
use tracing_subscriber::util::SubscriberInitExt;

pub fn setup_tracing(verbose: bool, output_dir: &std::path::Path) -> Result<MultiProgress> {
    std::fs::create_dir_all(output_dir)?;
    let log_path = output_dir.join("steam-vent-cdn.log");
    let log_file = std::fs::File::create(&log_path)?;
    let (non_blocking, guard) = tracing_appender::non_blocking(log_file);

    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        if verbose {
            tracing_subscriber::EnvFilter::new("steam_vent_cdn=debug,steam_vent=debug,info")
        } else {
            tracing_subscriber::EnvFilter::new("info")
        }
    });

    let file_layer = tracing_subscriber::fmt::layer()
        .with_level(true)
        .with_target(true)
        .with_thread_ids(false)
        .with_thread_names(false)
        .with_ansi(false)
        .with_writer(non_blocking)
        .boxed();

    let multi = MultiProgress::new();
    let writer =
        tracing_indicatif::IndicatifWriter::<tracing_indicatif::writer::Stdout>::new(multi.clone());

    let base_stdout = tracing_subscriber::fmt::layer()
        .with_thread_ids(false)
        .with_thread_names(false)
        .with_ansi(true)
        .with_writer(writer);

    let stdout_layer = if verbose {
        base_stdout
            .with_level(true)
            .with_target(true)
            .with_timer(tracing_subscriber::fmt::time::UtcTime::rfc_3339())
            .boxed()
    } else {
        base_stdout
            .with_level(false)
            .with_target(false)
            .without_time()
            .boxed()
    };

    tracing_subscriber::registry()
        .with(env_filter)
        .with(stdout_layer)
        .with(file_layer)
        .init();

    std::mem::forget(guard);
    Ok(multi)
}

pub fn resolve_credentials(
    username: Option<String>,
    password: Option<String>,
) -> Result<Credentials> {
    match (username, password) {
        (Some(username), Some(password)) => Ok(Credentials::UserPass { username, password }),
        (Some(username), None) => {
            let password = rpassword::prompt_password("Password: ").map_err(|e| {
                SteamVentError::Io(std::io::Error::other(format!(
                    "Password input failed: {}",
                    e
                )))
            })?;
            Ok(Credentials::UserPass { username, password })
        }
        _ => Ok(Credentials::Anonymous),
    }
}

pub fn create_config(
    credentials: Credentials,
    app_id: u32,
    parallel_download: usize,
    server_mode: ServerMode,
) -> Config {
    Config::builder()
        .credentials(credentials)
        .app_id(app_id)
        .parallel_download(parallel_download)
        .server_mode(server_mode)
        .timeout(std::time::Duration::from_secs(30))
        .retry_policy(RetryPolicy {
            max_retries: 3,
            retry_delay: std::time::Duration::from_secs(1),
        })
        .build()
}

