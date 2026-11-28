// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Logger with a debug log that can be turned on and off while the driver runs.
//!
//! A user cannot set environment variables on the Remote, so the debug log is a setup option.
//! Where `RUST_LOG` can be set, for example on a computer, it overrides the option.
//!
//! ratv logs with `tracing`. The driver and other crates log with `log`: [`LogTracer`] forwards
//! these records to the same subscriber.
//!
//! Line layout: `[<UTC time with ms> <level> <target>] <message> <fields> <spans>`, for example
//! `[2026-11-26T22:04:31.442Z DEBUG ratv::mrp] connected
//! session{protocol="apple tv" id=1}:session{protocol="mrp" id=3 addr=192.168.1.20:49152}`
//! (one line). Nested spans are joined with `:`.
//! The spans come after the message, so that a search for `] <message>` still finds the line.
//! On Linux, when stderr goes to the systemd journal, a priority prefix such as `<7>` comes first.

use jiff::Timestamp;
use std::fmt;
use std::sync::OnceLock;
use tracing::{Event, Level, Subscriber};
use tracing_log::{AsLog, LogTracer, NormalizeEvent};
use tracing_subscriber::filter::{EnvFilter, LevelFilter};
use tracing_subscriber::fmt::format::{DefaultFields, Writer};
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields, FormattedFields, MakeWriter};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::{LookupSpan, Registry};
use tracing_subscriber::reload;

/// Log filter without the debug log.
const DEFAULT_FILTER: &str = "info";
/// Log filter of the debug log. A name matches each module that starts with it, so `ratv` also
/// covers the other ratv crates.
const DEBUG_FILTER: &str = "info,uc_intg_appletv=debug,ratv=debug";

/// Switch of the log filter. `None` if `RUST_LOG` is set.
static FILTER: OnceLock<Option<reload::Handle<EnvFilter, Registry>>> = OnceLock::new();

/// Start the logger. Call it once, before anything is logged.
pub fn init(debug: bool) {
    let rust_log = std::env::var("RUST_LOG").ok();
    let filter = rust_log
        .as_deref()
        .map_or_else(|| option_filter(debug), env_filter_of);
    let (filter, handle) = reload::Layer::new(filter);
    let subscriber = Registry::default()
        .with(filter)
        .with(fmt_layer(journal_stream(), std::io::stderr));
    if tracing::subscriber::set_global_default(subscriber).is_err() {
        return;
    }
    let _ = FILTER.set(rust_log.is_none().then_some(handle));
    let _ = LogTracer::builder()
        .with_max_level(LevelFilter::current().as_log())
        .init();
}

/// Turn the debug log on or off. Does nothing if `RUST_LOG` is set.
pub fn set_debug(on: bool) {
    if let Some(Some(handle)) = FILTER.get()
        && handle.reload(option_filter(on)).is_ok()
    {
        log::set_max_level(LevelFilter::current().as_log());
    }
}

/// Returns `true` if `RUST_LOG` sets the log filter.
pub fn rust_log_sets_filter() -> bool {
    FILTER.get().is_some_and(Option::is_none)
}

/// The filter of the setup option.
fn option_filter(debug: bool) -> EnvFilter {
    EnvFilter::builder()
        .with_regex(false)
        .parse_lossy(if debug { DEBUG_FILTER } else { DEFAULT_FILTER })
}

/// The filter of `RUST_LOG`. Bad parts are left out, and each one gives a message on stderr.
/// Without a valid part only errors are logged.
fn env_filter_of(rust_log: &str) -> EnvFilter {
    EnvFilter::builder()
        .with_regex(false)
        .with_default_directive(LevelFilter::ERROR.into())
        .parse_lossy(rust_log)
}

/// Returns `true` if stderr goes to the systemd journal: `JOURNAL_STREAM` names the device and
/// inode of stderr. A child process can get the variable with another stderr.
#[cfg(target_os = "linux")]
fn journal_stream() -> bool {
    use std::os::fd::AsFd;
    use std::os::unix::fs::MetadataExt;

    let Some(value) = std::env::var_os("JOURNAL_STREAM") else {
        return false;
    };
    let Some(value) = value.to_str() else {
        return false;
    };
    let Ok(fd) = std::io::stderr().as_fd().try_clone_to_owned() else {
        return false;
    };
    std::fs::File::from(fd)
        .metadata()
        .is_ok_and(|meta| same_stream(value, meta.dev(), meta.ino()))
}

/// Returns `true` if `value` (`<dev>:<ino>` from `JOURNAL_STREAM`) names this device and inode.
#[cfg(any(target_os = "linux", test))]
fn same_stream(value: &str, dev: u64, ino: u64) -> bool {
    value.split_once(':').is_some_and(|(d, i)| {
        d.parse::<u64>().is_ok_and(|d| d == dev) && i.parse::<u64>().is_ok_and(|i| i == ino)
    })
}

/// Returns `true` if the output goes to the systemd journal.
#[cfg(not(target_os = "linux"))]
const fn journal_stream() -> bool {
    false
}

/// The fmt layer that writes lines with [`LineFormat`] to `writer`.
fn fmt_layer<S, W>(
    journal: bool,
    writer: W,
) -> tracing_subscriber::fmt::Layer<S, DefaultFields, LineFormat, W>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    W: for<'w> MakeWriter<'w> + 'static,
{
    tracing_subscriber::fmt::layer()
        .event_format(LineFormat { journal })
        .with_writer(writer)
}

/// The sd-daemon priority prefix of a line for the journal, else an empty string.
const fn priority_prefix(level: Level, journal: bool) -> &'static str {
    if !journal {
        return "";
    }
    match level {
        Level::ERROR => "<3>",
        Level::WARN => "<4>",
        Level::INFO => "<6>",
        Level::DEBUG | Level::TRACE => "<7>",
    }
}

/// The level name, padded to 5 characters.
const fn level_name(level: Level) -> &'static str {
    match level {
        Level::ERROR => "ERROR",
        Level::WARN => "WARN ",
        Level::INFO => "INFO ",
        Level::DEBUG => "DEBUG",
        Level::TRACE => "TRACE",
    }
}

/// Writes the lines in the layout of the module docs.
struct LineFormat {
    /// Start each line with a priority prefix for the journal.
    journal: bool,
}

impl<S, N> FormatEvent<S, N> for LineFormat
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        // A record of the `log` crate has its real target in its fields.
        let normalized = event.normalized_metadata();
        let meta = normalized.as_ref().unwrap_or_else(|| event.metadata());
        let level = *meta.level();
        write!(
            writer,
            "{}[{:.3} {} {}] ",
            priority_prefix(level, self.journal),
            Timestamp::now(),
            level_name(level),
            meta.target()
        )?;
        ctx.format_fields(writer.by_ref(), event)?;
        if let Some(scope) = ctx.event_scope() {
            let mut separator = ' ';
            for span in scope.from_root() {
                write!(writer, "{separator}{}", span.name())?;
                separator = ':';
                if let Some(fields) = span.extensions().get::<FormattedFields<N>>()
                    && !fields.is_empty()
                {
                    write!(writer, "{{{fields}}}")?;
                }
            }
        }
        writeln!(writer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use log::Log;
    use std::io;
    use std::sync::{Arc, Mutex};

    /// Keeps the written lines.
    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl io::Write for Capture {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().expect("lock").extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Capture {
        fn lines(&self) -> Vec<String> {
            let bytes = self.0.lock().expect("lock").clone();
            String::from_utf8(bytes)
                .expect("UTF-8")
                .lines()
                .map(str::to_owned)
                .collect()
        }
    }

    fn subscriber(
        filter: EnvFilter,
        journal: bool,
    ) -> (impl Subscriber + Send + Sync + 'static, Capture) {
        let capture = Capture::default();
        let writer = capture.clone();
        let subscriber = Registry::default()
            .with(filter)
            .with(fmt_layer(journal, move || writer.clone()));
        (subscriber, capture)
    }

    /// Splits a line into the time and the rest. Checks the time layout.
    fn split_time(line: &str) -> (&str, &str) {
        let rest = line.strip_prefix('[').expect("starts with [");
        let (time, rest) = rest.split_once(' ').expect("time");
        assert_eq!(24, time.len(), "{time}");
        assert!(time.ends_with('Z') && time.as_bytes()[19] == b'.', "{time}");
        assert!(time.parse::<Timestamp>().is_ok(), "{time}");
        (time, rest)
    }

    #[test]
    fn line_layout_for_remote_sim() {
        let (sub, capture) = subscriber(option_filter(true), false);
        tracing::subscriber::with_default(sub, || {
            tracing::info!(target: "uc_intg_appletv::device::actor", "[A] connected in 20 ms");
            tracing::warn!(target: "uc_intg_appletv", "careful");
            tracing::error!(target: "uc_intg_appletv", "broken");
            tracing::debug!(target: "uc_intg_appletv", "detail");
        });
        let lines = capture.lines();
        let rests: Vec<&str> = lines.iter().map(|l| split_time(l).1).collect();
        // The remote-sim tool splits `[<time> <level> <target>]` at spaces to read the level and
        // the target, so spaces must separate them.
        assert_eq!(
            vec![
                "INFO  uc_intg_appletv::device::actor] [A] connected in 20 ms",
                "WARN  uc_intg_appletv] careful",
                "ERROR uc_intg_appletv] broken",
                "DEBUG uc_intg_appletv] detail",
            ],
            rests
        );
    }

    #[test]
    fn span_context_follows_the_message() {
        let (sub, capture) = subscriber(option_filter(true), false);
        tracing::subscriber::with_default(sub, || {
            let addr = "10.0.0.1:49152";
            let id = 1_u32;
            let span =
                tracing::error_span!(target: "ratv", "session", protocol = "mrp", id, addr = %addr);
            let _entered = span.enter();
            tracing::debug!(target: "ratv::mrp", addr = %addr, "connected");
            let inner = tracing::error_span!(target: "ratv", "inner");
            inner.in_scope(|| tracing::info!(target: "ratv", "deep"));
        });
        let lines = capture.lines();
        let rests: Vec<&str> = lines.iter().map(|l| split_time(l).1).collect();
        // No line for the span itself.
        assert_eq!(
            vec![
                "DEBUG ratv::mrp] connected addr=10.0.0.1:49152 \
                 session{protocol=\"mrp\" id=1 addr=10.0.0.1:49152}",
                "INFO  ratv] deep session{protocol=\"mrp\" id=1 addr=10.0.0.1:49152}:inner",
            ],
            rests
        );
    }

    #[test]
    fn log_records_keep_their_target() {
        let emit = || {
            let tracer = LogTracer::new();
            for (level, target, text) in [
                (log::Level::Info, "actix_server::builder", "actix info"),
                (log::Level::Debug, "actix_server::builder", "actix detail"),
                (
                    log::Level::Debug,
                    "uc_intg_appletv::controller",
                    "driver detail",
                ),
            ] {
                tracer.log(
                    &log::Record::builder()
                        .level(level)
                        .target(target)
                        .module_path_static(Some(target))
                        .args(format_args!("{text}"))
                        .build(),
                );
            }
        };
        let (sub, normal) = subscriber(option_filter(false), false);
        tracing::subscriber::with_default(sub, emit);
        let (sub, debug) = subscriber(option_filter(true), false);
        tracing::subscriber::with_default(sub, emit);
        let rests = |c: &Capture| -> Vec<String> {
            c.lines()
                .iter()
                .map(|l| split_time(l).1.to_owned())
                .collect()
        };
        assert_eq!(
            vec!["INFO  actix_server::builder] actix info"],
            rests(&normal)
        );
        assert_eq!(
            vec![
                "INFO  actix_server::builder] actix info",
                "DEBUG uc_intg_appletv::controller] driver detail",
            ],
            rests(&debug)
        );
    }

    #[test]
    fn journal_prefix_only_with_the_flag() {
        let (sub, capture) = subscriber(option_filter(true), true);
        tracing::subscriber::with_default(sub, || {
            tracing::warn!(target: "uc_intg_appletv", "careful");
        });
        let lines = capture.lines();
        let line = lines[0].strip_prefix("<4>").expect("prefix");
        assert_eq!("WARN  uc_intg_appletv] careful", split_time(line).1);

        for level in [
            Level::ERROR,
            Level::WARN,
            Level::INFO,
            Level::DEBUG,
            Level::TRACE,
        ] {
            assert_eq!("", priority_prefix(level, false));
        }
        assert_eq!("<3>", priority_prefix(Level::ERROR, true));
        assert_eq!("<4>", priority_prefix(Level::WARN, true));
        assert_eq!("<6>", priority_prefix(Level::INFO, true));
        assert_eq!("<7>", priority_prefix(Level::DEBUG, true));
        assert_eq!("<7>", priority_prefix(Level::TRACE, true));
    }

    #[test]
    fn journal_stream_must_match_stderr() {
        assert!(same_stream("2049:123456", 2049, 123_456));
        assert!(!same_stream("2049:123456", 2049, 123_457));
        assert!(!same_stream("2049:123456", 2048, 123_456));
        assert!(!same_stream("2049:abc", 2049, 0));
        assert!(!same_stream("", 0, 0));
        assert!(!same_stream("2049", 2049, 0));
        assert!(!same_stream("2049:123456:1", 2049, 123_456));
    }

    #[test]
    fn filters_select_the_lines() {
        let emit = || {
            tracing::debug!(target: "ratv_companion::connection", "ratv detail");
            tracing::debug!(target: "actix_server", "actix detail");
            tracing::info!(target: "actix_server", "actix info");
        };
        let (sub, debug) = subscriber(option_filter(true), false);
        tracing::subscriber::with_default(sub, emit);
        let (sub, normal) = subscriber(option_filter(false), false);
        tracing::subscriber::with_default(sub, emit);
        let (sub, env) = subscriber(env_filter_of("actix_server=debug,bad=level"), false);
        tracing::subscriber::with_default(sub, emit);
        let (sub, empty) = subscriber(env_filter_of(""), false);
        tracing::subscriber::with_default(sub, || {
            emit();
            tracing::error!(target: "x", "failure");
        });
        let texts = |c: &Capture| -> Vec<String> {
            c.lines()
                .iter()
                .map(|l| l.rsplit_once("] ").expect("message").1.to_owned())
                .collect()
        };
        assert_eq!(vec!["ratv detail", "actix info"], texts(&debug));
        assert_eq!(vec!["actix info"], texts(&normal));
        assert_eq!(vec!["actix detail", "actix info"], texts(&env));
        assert_eq!(vec!["failure"], texts(&empty));
    }

    /// ratv logs with `tracing`. The driver needs these lines in its log, with the span.
    #[tokio::test]
    async fn ratv_log_lines_reach_the_log() {
        let (sub, capture) = subscriber(option_filter(true), false);
        let _guard = tracing::subscriber::set_default(sub);
        // Nothing listens on this port, so the connect fails fast (on Windows after about 2 s) and
        // ratv logs it. A fixed port can be in use on a test machine.
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
        let lines = capture.lines();
        assert!(
            lines
                .iter()
                .any(|l| l.contains("connection failed") && l.contains(" session{")),
            "{lines:?}"
        );
    }
}
