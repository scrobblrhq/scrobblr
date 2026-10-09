use std::backtrace::{Backtrace, BacktraceStatus};

/// Sends panics to the log instead of stderr, with a backtrace when
/// `RUST_BACKTRACE` asks for one. Whoever catches the panic decides what
/// happens next.
pub fn log_panics() {
    std::panic::set_hook(Box::new(|info| {
        let message = info.payload_as_str().unwrap_or("(no message)");
        let location = info.location().map(ToString::to_string).unwrap_or_default();
        let backtrace = Backtrace::capture();
        if backtrace.status() == BacktraceStatus::Captured {
            tracing::error!("panicked at {location}: {message}\n{backtrace}");
        } else {
            tracing::error!("panicked at {location}: {message}");
        }
    }));
}
