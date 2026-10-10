use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

/// Logs to stdout, filtered by `RUST_LOG`: readable lines, or one JSON
/// object per event with `LOG_FORMAT=json`.
pub fn init() -> Result<(), String> {
    let format = std::env::var("LOG_FORMAT").unwrap_or_default();
    let registry = tracing_subscriber::registry().with(EnvFilter::from_default_env());
    match format.trim() {
        "" | "text" => registry.with(tracing_subscriber::fmt::layer()).init(),
        "json" => registry
            .with(
                tracing_subscriber::fmt::layer()
                    .json()
                    .flatten_event(true)
                    .with_current_span(false),
            )
            .init(),
        other => return Err(format!("LOG_FORMAT={other}: expected text or json")),
    }
    Ok(())
}
