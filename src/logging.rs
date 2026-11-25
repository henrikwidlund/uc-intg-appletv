// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Logger with a debug log that can be turned on and off while the driver runs.
//!
//! A user cannot set environment variables on the Remote, so the debug log is a setup option.
//! Where `RUST_LOG` can be set, for example on a computer, it overrides the option.

use env_logger::{Builder, Env, Logger};
use log::{Log, Metadata, Record};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

/// Log filter without the debug log.
const DEFAULT_FILTER: &str = "info";
/// Log filter of the debug log. `ratv` also matches the other ratv crates.
const DEBUG_FILTER: &str = "info,uc_intg_appletv=debug,ratv=debug";

static LOGGER: OnceLock<SwitchLogger> = OnceLock::new();

struct SwitchLogger {
    normal: Logger,
    /// `None` if `RUST_LOG` is set.
    debug: Option<Logger>,
    use_debug: AtomicBool,
}

impl SwitchLogger {
    fn active(&self) -> &Logger {
        match &self.debug {
            Some(debug) if self.use_debug.load(Ordering::Relaxed) => debug,
            _ => &self.normal,
        }
    }
}

impl Log for SwitchLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        self.active().enabled(metadata)
    }

    fn log(&self, record: &Record<'_>) {
        self.active().log(record);
    }

    fn flush(&self) {
        self.active().flush();
    }
}

/// Start the logger. Call it once, before anything is logged.
pub fn init(debug: bool) {
    let normal = Builder::from_env(Env::default().default_filter_or(DEFAULT_FILTER))
        .format_timestamp_millis()
        .build();
    let debug_logger = std::env::var_os("RUST_LOG").is_none().then(|| {
        Builder::new()
            .parse_filters(DEBUG_FILTER)
            .format_timestamp_millis()
            .build()
    });
    let logger = LOGGER.get_or_init(|| SwitchLogger {
        normal,
        debug: debug_logger,
        use_debug: AtomicBool::new(debug),
    });
    if log::set_logger(logger).is_ok() {
        set_max_level(logger);
    }
}

/// Turn the debug log on or off. Does nothing if `RUST_LOG` is set.
pub fn set_debug(on: bool) {
    if let Some(logger) = LOGGER.get() {
        logger.use_debug.store(on, Ordering::Relaxed);
        set_max_level(logger);
    }
}

/// Returns `true` if `RUST_LOG` sets the log filter.
pub fn env_filter() -> bool {
    LOGGER.get().is_some_and(|l| l.debug.is_none())
}

fn set_max_level(logger: &SwitchLogger) {
    log::set_max_level(logger.active().filter());
}

#[cfg(test)]
mod tests {
    use log::{Level, Log, Metadata, Record};
    use std::sync::Mutex;

    /// Keeps the log records of ratv.
    struct Capture(Mutex<Vec<String>>);

    impl Log for Capture {
        fn enabled(&self, _: &Metadata<'_>) -> bool {
            true
        }

        fn log(&self, record: &Record<'_>) {
            if record.target().starts_with("ratv")
                && let Ok(mut lines) = self.0.lock()
            {
                lines.push(format!("{} {}", record.level(), record.args()));
            }
        }

        fn flush(&self) {}
    }

    static CAPTURE: Capture = Capture(Mutex::new(Vec::new()));

    /// ratv logs with `tracing`. The driver needs these lines in its log. The only test that sets
    /// the global logger.
    #[tokio::test]
    async fn ratv_log_lines_reach_the_log() {
        log::set_logger(&CAPTURE).expect("logger");
        log::set_max_level(Level::Debug.to_level_filter());
        // Nothing listens on this port, so the connect fails at once and ratv logs it. A fixed
        // port can be in use on a test machine.
        let addr = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|l| l.local_addr())
            .expect("free port");
        let endpoints = ratv::Endpoints {
            companion: Some(addr),
            airplay: None,
        };
        let credentials = crate::config_store::tests::device().credentials;
        let result =
            ratv::AppleTv::connect(&endpoints, &credentials, &ratv::ClientOptions::default()).await;
        assert!(result.is_err());
        let lines = CAPTURE.0.lock().expect("lock").clone();
        assert!(
            lines.iter().any(|l| l.contains("connection failed")),
            "{lines:?}"
        );
    }
}
