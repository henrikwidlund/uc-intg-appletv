// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Reading the driver log. Only this module reads the log file. Checks read the lines of the
//! driver itself with [`driver_log`]. [`RawLog`] has the lines of all crates, for the `log` step
//! and for the debug lines of ratv.

use super::{Driver, LogMark};
use crate::Fail;

/// Target of the driver lines. The modules of the driver add `::<module>`.
const DRIVER_TARGET: &str = "uc_intg_appletv";

/// One log line: `[<time> <level> <target>] <message>`.
pub struct LogLine<'a> {
    /// The whole line.
    pub line: &'a str,
    pub level: &'a str,
    pub target: &'a str,
    pub message: &'a str,
}

/// The parts of a log line. `None` for a line without the `[<time> <level> <target>]` start, for
/// example the second line of a message.
fn parse_line(line: &str) -> Option<LogLine<'_>> {
    let (head, message) = line.strip_prefix('[')?.split_once(']')?;
    // The level name is padded to 5 characters, so more than one space can come after it.
    let mut words = head.split(' ').filter(|w| !w.is_empty());
    let (_time, level, target) = (words.next()?, words.next()?, words.next()?);
    let known = ["ERROR", "WARN", "INFO", "DEBUG", "TRACE"].contains(&level);
    (known && words.next().is_none()).then(|| LogLine {
        line,
        level,
        target,
        message: message.strip_prefix(' ').unwrap_or(message),
    })
}

/// True for the target of the driver or of one of its modules.
fn is_driver_target(target: &str) -> bool {
    target == DRIVER_TARGET
        || target
            .strip_prefix(DRIVER_TARGET)
            .is_some_and(|rest| rest.starts_with("::"))
}

/// The parts of a line of the driver itself. `None` for a line of ratv or another crate.
fn driver_line(line: &str) -> Option<LogLine<'_>> {
    parse_line(line).filter(|l| is_driver_target(l.target))
}

/// True if `text` contains one of `patterns`.
pub fn has_any(text: &str, patterns: &[&str]) -> bool {
    patterns.iter().any(|p| text.contains(p))
}

/// The log after `mark` as raw text. Empty if it cannot be read. Bytes that are not UTF-8, for
/// example a character cut at the mark, become U+FFFD.
fn read_since(driver: &Driver, mark: LogMark) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = std::fs::File::open(&driver.log) else {
        return String::new();
    };
    let mut bytes = Vec::new();
    if file.seek(SeekFrom::Start(mark.0)).is_err() || file.read_to_end(&mut bytes).is_err() {
        return String::new();
    }
    String::from_utf8(bytes).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// The lines of the driver itself in a part of the driver log. Lines of ratv and other crates,
/// and lines without the `[<time> <level> <target>]` start, are left out. Methods that search
/// match only the message part of a line.
pub struct DriverLog {
    /// The kept lines, each with a new line at the end.
    text: String,
}

impl DriverLog {
    /// The driver lines of `raw`.
    pub fn from_raw(raw: &str) -> Self {
        Self::from_lines(raw.lines().filter(|l| driver_line(l).is_some()))
    }

    fn from_lines<'a>(lines: impl Iterator<Item = &'a str>) -> Self {
        let mut text = String::new();
        for line in lines {
            text.push_str(line);
            text.push('\n');
        }
        Self { text }
    }

    /// All lines, in log order.
    pub fn entries(&self) -> impl Iterator<Item = LogLine<'_>> {
        self.text.lines().filter_map(parse_line)
    }

    /// The lines whose message contains one of `patterns`.
    pub fn matches<'a, 'p>(
        &'a self,
        patterns: &'p [&'p str],
    ) -> impl Iterator<Item = LogLine<'a>> + use<'a, 'p> {
        self.entries().filter(move |l| has_any(l.message, patterns))
    }

    /// True if a message contains `pattern`.
    pub fn contains(&self, pattern: &str) -> bool {
        self.has_any(&[pattern])
    }

    /// True if a message contains one of `patterns`.
    pub fn has_any(&self, patterns: &[&str]) -> bool {
        self.matches(patterns).next().is_some()
    }

    /// The number of lines whose message contains one of `patterns`.
    pub fn count(&self, patterns: &[&str]) -> usize {
        self.matches(patterns).count()
    }

    /// The whole lines whose message contains one of `patterns`.
    pub fn lines_with(&self, patterns: &[&str]) -> Vec<&str> {
        self.matches(patterns).map(|l| l.line).collect()
    }

    /// The index of the first line whose message contains one of `patterns`.
    pub fn first_with(&self, patterns: &[&str]) -> Option<usize> {
        self.entries().position(|l| has_any(l.message, patterns))
    }

    /// The index of the last line whose message contains one of `patterns`.
    pub fn last_with(&self, patterns: &[&str]) -> Option<usize> {
        self.entries()
            .enumerate()
            .filter(|(_, l)| has_any(l.message, patterns))
            .last()
            .map(|(i, _)| i)
    }

    /// The lines before the line at `index`.
    pub fn lines_before(&self, index: usize) -> Self {
        Self::from_lines(self.text.lines().take(index))
    }

    /// The lines from the line at `index`.
    pub fn lines_from(&self, index: usize) -> Self {
        Self::from_lines(self.text.lines().skip(index))
    }
}

/// The driver lines of the driver log after `mark`.
pub fn driver_log(driver: &Driver, mark: LogMark) -> DriverLog {
    DriverLog::from_raw(&read_since(driver, mark))
}

/// A part of the driver log with the lines of all crates. Only for the `log` step and for the
/// debug lines of ratv. Other checks use [`DriverLog`].
pub struct RawLog(String);

impl RawLog {
    /// The whole log.
    pub fn whole(driver: &Driver) -> Self {
        Self::since(driver, LogMark::START)
    }

    /// The log after `mark`.
    pub fn since(driver: &Driver, mark: LogMark) -> Self {
        Self(read_since(driver, mark))
    }

    /// True if `text` is anywhere in the log, also in a line of another crate or in a line
    /// without a start. For secrets and panics.
    pub fn contains_anywhere(&self, text: &str) -> bool {
        self.0.contains(text)
    }

    /// The lines with the log level `level`, from all crates.
    pub fn level_lines(&self, level: &str) -> Vec<&str> {
        self.0
            .lines()
            .filter(|l| parse_line(l).is_some_and(|p| p.level == level))
            .collect()
    }

    /// Numbers of debug lines of the driver and of ratv. A line starts with
    /// `[<time> <level> <target>]`, so a text in a message does not count.
    pub fn debug_lines(&self) -> (usize, usize) {
        let (mut driver, mut ratv) = (0, 0);
        for line in self.0.lines().filter_map(parse_line) {
            if line.level != "DEBUG" {
                continue;
            }
            match line.target {
                target if is_driver_target(target) => driver += 1,
                // The prefix also covers the other ratv crates, such as ratv_mrp, as in the log
                // filter of the driver.
                target if target.starts_with("ratv") => ratv += 1,
                _ => {}
            }
        }
        (driver, ratv)
    }
}

/// The driver log from the first driver line after `mark` whose message has `pattern`. The
/// lines of ratv stay in it.
pub fn log_after(driver: &Driver, mark: LogMark, pattern: &str) -> Result<RawLog, Fail> {
    let mut log = read_since(driver, mark);
    let mut at = 0;
    for line in log.split_inclusive('\n') {
        if driver_line(line).is_some_and(|l| l.message.contains(pattern)) {
            log.drain(..at);
            return Ok(RawLog(log));
        }
        at += line.len();
    }
    Err(Fail::new(format!(
        "no driver line with '{pattern}' after the mark"
    )))
}

/// A driver log line with `message` and the driver target, with a new line at the end.
#[cfg(test)]
pub fn test_line(message: &str) -> String {
    format!("[2026-11-28T10:00:00.000Z INFO  uc_intg_appletv::device::actor] {message}\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_line as line;

    fn log_of(raw: &str) -> DriverLog {
        DriverLog::from_raw(raw)
    }

    /// A driver with a new empty folder in the temp folder for one test.
    fn test_driver(name: &str) -> Driver {
        let dir =
            std::env::temp_dir().join(format!("remote-sim-log-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("test folder");
        Driver::new(std::path::Path::new("none"), &dir, 0)
    }

    #[test]
    fn only_driver_lines_count() {
        let parsed = parse_line("[2026-11-28T10:00:00.000Z WARN  uc_intg_appletv] [A] x y")
            .expect("a log line");
        assert_eq!(
            ("WARN", "uc_intg_appletv", "[A] x y"),
            (parsed.level, parsed.target, parsed.message)
        );
        assert!(
            parse_line("[A] connection closed").is_none(),
            "no level and target"
        );
        assert!(
            parse_line("[t NOTE uc_intg_appletv] x").is_none(),
            "unknown level"
        );
        assert!(
            parse_line("[t INFO a b] x").is_none(),
            "more than one target"
        );
        // A message that holds text like a driver line start.
        let ratv = "[t DEBUG ratv::mrp] got [t INFO  uc_intg_appletv] [A] connection closed\n\
                    [t DEBUG ratv::apple_tv] connection failed protocol=mrp\n\
                    trace [t INFO  uc_intg_appletv] [A] connection closed\n";
        let closed = ["connection closed"];
        assert_eq!(0, log_of(ratv).count(&closed), "ratv lines");
        assert_eq!(0, log_of(ratv).count(&["connection failed"]), "ratv lines");
        assert!(log_of(ratv).lines_with(&closed).is_empty());
        let log = format!("{ratv}{}", line("[A] connection closed"));
        assert_eq!(
            vec![line("[A] connection closed").trim_end()],
            log_of(&log).lines_with(&closed)
        );
        assert_eq!(1, log_of(&log).count(&closed));
    }

    #[test]
    fn real_lines_are_parsed() {
        let real = "[2026-11-26T22:04:31.442Z DEBUG ratv::mrp] connected \
                    addr=192.168.1.20:49152 session{protocol=\"mrp\" id=3}";
        let parsed = parse_line(real).expect("a log line");
        assert_eq!(real, parsed.line);
        assert_eq!(
            (
                "DEBUG",
                "ratv::mrp",
                "connected addr=192.168.1.20:49152 session{protocol=\"mrp\" id=3}"
            ),
            (parsed.level, parsed.target, parsed.message)
        );
        let error = parse_line("[2026-11-26T22:04:31.442Z ERROR uc_intg_appletv] broken")
            .expect("ERROR line");
        assert_eq!(("ERROR", "broken"), (error.level, error.message));
        let trace = parse_line("[t TRACE uc_intg_appletv::device] x").expect("TRACE line");
        assert_eq!("TRACE", trace.level);
        let empty = parse_line("[t INFO  uc_intg_appletv]").expect("empty message");
        assert_eq!("", empty.message);
        assert!(driver_line("[t INFO  uc_intg_appletv] x").is_some());
        assert!(driver_line("[t INFO  uc_intg_appletv::device::actor] x").is_some());
        assert!(
            driver_line("[t INFO  uc_intg_appletv_x] x").is_none(),
            "another crate"
        );
        assert!(driver_line("[t INFO  uc_intg_appletvx::a] x").is_none());
    }

    #[test]
    fn driver_log_keeps_only_driver_lines() {
        let raw = format!(
            "{}[t DEBUG ratv::mrp] [A] b\n  continuation of a message\n{}{}",
            line("[A] a"),
            line("[A] c"),
            line("[A] a")
        );
        let log = log_of(&raw);
        assert_eq!(
            format!("{}{}{}", line("[A] a"), line("[A] c"), line("[A] a")),
            log.text
        );
        assert!(!log.contains("continuation"));
        assert!(!log.contains("] b"));
        assert!(log.contains("[A] c"));
        assert!(!log.contains("INFO"), "the header is not matched");
        assert_eq!(2, log.count(&["[A] a"]));
        assert_eq!(Some(1), log.first_with(&["[A] c"]));
        assert_eq!(Some(2), log.last_with(&["[A] a"]));
        assert_eq!(None, log.last_with(&["[A] d"]));
        assert_eq!(line("[A] a"), log.lines_before(1).text);
        assert_eq!(
            format!("{}{}", line("[A] c"), line("[A] a")),
            log.lines_from(1).text
        );
    }

    #[test]
    fn bytes_that_are_not_utf8_do_not_stop_the_next_lines() {
        let driver = test_driver("utf8");
        let first = line("[A] \u{e4}");
        let mut bytes = first.clone().into_bytes();
        bytes.extend_from_slice(b"\xff\xfe broken\n");
        bytes.extend_from_slice(line("[A] next").as_bytes());
        std::fs::write(&driver.log, &bytes).expect("write");
        // The mark is inside the two bytes of `ä` in the first line.
        let mark = LogMark::at(first.len() as u64 - 2);
        let raw = read_since(&driver, mark);
        assert_eq!(
            format!("\u{fffd}\n\u{fffd}\u{fffd} broken\n{}", line("[A] next")),
            raw
        );
        let parsed = raw.lines().filter_map(parse_line).collect::<Vec<_>>();
        assert_eq!(1, parsed.len());
        assert_eq!("[A] next", parsed[0].message);
        assert!(driver_log(&driver, LogMark::START).contains("[A] next"));
        assert_eq!("", read_since(&driver, LogMark::at(1000)), "after the end");
        let _ = std::fs::remove_dir_all(&driver.dir);
        assert_eq!("", read_since(&driver, LogMark::START), "no file");
    }

    #[test]
    fn log_levels_come_from_the_line_start() {
        let log = RawLog(
            "[t ERROR uc_intg_appletv] a\n\
             [t INFO  uc_intg_appletv] text with ERROR and WARN in it\n\
             [t WARN  ratv::mrp] b\n\
             ERROR at the start\n"
                .to_owned(),
        );
        assert_eq!(
            vec!["[t ERROR uc_intg_appletv] a"],
            log.level_lines("ERROR")
        );
        assert_eq!(vec!["[t WARN  ratv::mrp] b"], log.level_lines("WARN"));
        assert!(log.contains_anywhere("ERROR at the start"));
    }

    #[test]
    fn log_after_starts_at_the_pattern() {
        let driver = test_driver("after");
        std::fs::write(&driver.log, line("[w] debug log on")).expect("write");
        let mark = driver.log_len();
        let ratv = "[t DEBUG ratv::mrp] x] debug log on\n";
        let on = line("[w] debug log on");
        let mut log = std::fs::read_to_string(&driver.log).expect("read");
        log.push_str(&format!("{ratv}{on}{ratv}"));
        std::fs::write(&driver.log, log).expect("write");
        let Ok(after) = log_after(&driver, mark, "] debug log on") else {
            panic!("no line after the mark");
        };
        assert_eq!(
            format!("{on}{ratv}"),
            after.0,
            "from the driver line, with ratv lines"
        );
        assert_eq!((0, 1), after.debug_lines());
        let Err(e) = log_after(&driver, mark, "] debug log off") else {
            panic!("no off line");
        };
        assert_eq!(
            "no driver line with '] debug log off' after the mark",
            e.msg
        );
        let _ = std::fs::remove_dir_all(&driver.dir);
    }

    #[test]
    fn debug_lines_count_only_the_level_and_target() {
        let log = RawLog(
            "[2026-11-28T10:00:00.123Z DEBUG uc_intg_appletv::device::actor] [A] x\n\
             [2026-11-28T10:00:00.124Z INFO  uc_intg_appletv] title: A DEBUG song\n\
             DEBUG at the start\n\
             [2026-11-28T10:00:00.125Z DEBUG ratv::apple_tv] connected\n\
             [2026-11-28T10:00:00.126Z DEBUG ratv] other\n\
             [2026-11-28T10:00:00.127Z DEBUG actix_web] other crate\n\
             [2026-11-28T10:00:00.128Z TRACE uc_intg_appletv] trace\n\
             [2026-11-28T10:00:00.129Z TRACE ratv::mrp] trace\n"
                .to_owned(),
        );
        assert_eq!((1, 2), log.debug_lines());
        assert_eq!((0, 0), RawLog(String::new()).debug_lines());
    }
}
