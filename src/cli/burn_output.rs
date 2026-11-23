use std::fs::{File, OpenOptions};
use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use artinchip_flash_app::device::{BurnEvent, BurnReport, ResetOutcome};
use artinchip_flash_app::flash_job::JobEvent;
use serde_json::{json, Value};

use super::ProgressMode;

pub(super) struct BurnOutput {
    started: Instant,
    json: bool,
    mode: ProgressMode,
    tty: bool,
    verbose: bool,
    log: Option<File>,
    last_sample: Option<(usize, Duration)>,
    rate: Option<f64>,
    displayed: Option<Duration>,
    milestone: usize,
    line_width: usize,
}

impl BurnOutput {
    pub(super) fn new(json: bool, mode: ProgressMode, verbose: bool, log: Option<&Path>) -> Self {
        let log =
            log.and_then(
                |path| match OpenOptions::new().write(true).create_new(true).open(path) {
                    Ok(file) => Some(file),
                    Err(error) => {
                        eprintln!(
                            "Warning: could not open burn log '{}': {error}",
                            path.display()
                        );
                        None
                    }
                },
            );
        Self {
            started: Instant::now(),
            json,
            mode,
            tty: !json
                && matches!(mode, ProgressMode::Auto)
                && io::stderr().is_terminal()
                && std::env::var("TERM").as_deref() != Ok("dumb")
                && !verbose,
            verbose,
            log,
            last_sample: None,
            rate: None,
            displayed: None,
            milestone: 0,
            line_width: 0,
        }
    }

    pub(super) fn event(&mut self, event: JobEvent) {
        self.event_at(event, self.started.elapsed());
    }

    fn event_at(&mut self, event: JobEvent, elapsed: Duration) {
        let event = match event {
            JobEvent::Progress(event) => event,
            JobEvent::Output(line) => {
                self.log(&json!({"type":"log","line":line,"elapsed_secs":elapsed.as_secs_f64()}));
                self.line(&line);
                return;
            }
        };
        let progress_event = matches!(
            event,
            BurnEvent::ComponentProgress { .. } | BurnEvent::OverallProgress { .. }
        );
        let mut value =
            (self.json || (self.log.is_some() && !progress_event)).then(|| event.to_json(elapsed));
        let line = match event {
            BurnEvent::Stage(line) => Some(line),
            BurnEvent::Log(line) => {
                if self.verbose || line.starts_with("Warning:") {
                    Some(line)
                } else {
                    None
                }
            }
            BurnEvent::ComponentStarted {
                name,
                partition,
                size,
            } => {
                self.last_sample = None;
                self.rate = None;
                if matches!(self.mode, ProgressMode::Off) {
                    None
                } else {
                    Some(format!(
                        "Component: {name} partition={partition} ({size} bytes)"
                    ))
                }
            }
            BurnEvent::ComponentProgress { sent, .. } => {
                if let Some((previous, time)) = self.last_sample {
                    self.rate = rate(sent.saturating_sub(previous), elapsed - time);
                }
                self.last_sample = Some((sent, elapsed));
                None
            }
            BurnEvent::OverallProgress { sent, total } => {
                self.progress(sent, total, elapsed);
                None
            }
            BurnEvent::ComponentFinished { name } => {
                (!matches!(self.mode, ProgressMode::Off)).then(|| format!("Component done: {name}"))
            }
            BurnEvent::Finished => Some("Burn finished".into()),
        };
        if progress_event {
            if let Some(value) = &mut value {
                value["rate_bps"] = json!(self.rate);
            }
        } else if let Some(value) = &value {
            self.log(value);
        }
        if self.json {
            println!("{}", value.expect("JSON event"));
        }
        if let Some(line) = line.filter(|line| !self.json || line.starts_with("Warning:")) {
            self.line(&line);
        }
    }

    fn progress(&mut self, sent: usize, total: usize, elapsed: Duration) {
        if self.json || matches!(self.mode, ProgressMode::Off) || total == 0 {
            return;
        }
        let percent = ((sent as f64 / total as f64) * 100.0).min(100.0);
        let milestone = percent as usize / 25;
        let display = if self.tty {
            self.displayed
                .is_none_or(|last| elapsed - last >= Duration::from_millis(250))
                || sent == total
        } else {
            (self.verbose && matches!(self.mode, ProgressMode::Plain)) || milestone > self.milestone
        };
        if display {
            self.displayed = Some(elapsed);
            self.milestone = milestone;
            let line = format!(
                "Overall: {sent}/{total} bytes ({percent:.0}%) {}",
                format_rate(self.rate)
            );
            if self.tty {
                eprint!(
                    "\r{line}{:width$}",
                    "",
                    width = self.line_width.saturating_sub(line.len())
                );
                self.line_width = line.len();
            } else {
                self.line(&line);
            }
        }
    }

    fn line(&mut self, line: &str) {
        if self.line_width != 0 {
            eprint!("\r{:width$}\r", "", width = self.line_width);
            self.line_width = 0;
        }
        eprintln!("{line}");
    }

    fn log(&mut self, value: &Value) {
        if let Some(error) = self
            .log
            .as_mut()
            .and_then(|log| writeln!(log, "{value}").err())
        {
            self.log = None;
            eprintln!("Warning: could not write burn log: {error}");
        }
    }

    pub(super) fn finish(&mut self, report: &BurnReport, code: i32, error: Option<&str>) {
        let elapsed = self.started.elapsed();
        let (reset, reset_error) = match &report.reset {
            ResetOutcome::NotAttempted => ("not_attempted", None),
            ResetOutcome::Skipped => ("skipped", None),
            ResetOutcome::Acknowledged => ("acknowledged", None),
            ResetOutcome::Unconfirmed(error) => ("unconfirmed", Some(error)),
            ResetOutcome::Failed(error) => ("failed", Some(error)),
        };
        let average = rate(report.bytes_sent, report.transfer_time);
        let non_transfer = elapsed.saturating_sub(report.transfer_time);
        let status = match code {
            0 => "success",
            130 => "cancelled",
            _ => "failed",
        };
        let result = json!({
            "type":"result", "status":status, "exit_code":code, "error":error,
            "elapsed_secs":elapsed.as_secs_f64(), "transfer_secs":report.transfer_time.as_secs_f64(),
            "non_transfer_secs":non_transfer.as_secs_f64(), "bytes_sent":report.bytes_sent,
            "connection_secs":report.connection_time.as_secs_f64(), "reconnect_secs":report.reconnect_time.as_secs_f64(),
            "avg_rate_bps":average, "components_completed":report.components_completed,
            "device_crcs_verified":report.device_crcs_verified, "upgrade_ended":report.upgrade_ended,
            "reset":reset, "reset_error":reset_error,
        });
        self.log(&result);
        if self.json {
            println!("{result}");
        }
        if let Some(error) = error {
            self.line(error);
        } else {
            if !self.json {
                println!("Burn completed successfully!");
            }
            self.line(&format!(
                "Sent {} bytes in {:.2}s of {:.2}s (avg {}). Connection {:.2}s, reconnect {:.2}s. Upgrade end acknowledged, {} component(s) completed, {} device CRC(s) verified. Reset: {reset}.",
                report.bytes_sent, report.transfer_time.as_secs_f64(), elapsed.as_secs_f64(),
                format_rate(average), report.connection_time.as_secs_f64(), report.reconnect_time.as_secs_f64(),
                report.components_completed, report.device_crcs_verified,
            ));
        }
    }
}

fn rate(bytes: usize, time: Duration) -> Option<f64> {
    (bytes > 0 && !time.is_zero()).then(|| bytes as f64 / time.as_secs_f64())
}

fn format_rate(bps: Option<f64>) -> String {
    match bps {
        Some(bps) if bps >= 1024.0 * 1024.0 => format!("{:.2} MiB/s", bps / (1024.0 * 1024.0)),
        Some(bps) if bps >= 1024.0 => format!("{:.1} KiB/s", bps / 1024.0),
        Some(bps) => format!("{bps:.0} B/s"),
        None => "--".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_rate_excludes_wait_before_first_chunk_and_resets_at_component_boundary() {
        let mut output = BurnOutput::new(false, ProgressMode::Off, false, None);
        for (sent, seconds, expected) in [(512, 30, None), (1024, 31, Some(512.0))] {
            output.event_at(
                JobEvent::Progress(BurnEvent::ComponentProgress {
                    name: "first".into(),
                    sent,
                    total: 1024,
                }),
                Duration::from_secs(seconds),
            );
            assert_eq!(output.rate, expected);
        }
        output.event_at(
            JobEvent::Progress(BurnEvent::ComponentStarted {
                name: "second".into(),
                partition: "os".into(),
                size: 1024,
            }),
            Duration::from_secs(32),
        );
        assert_eq!(output.rate, None);
        assert!(output.last_sample.is_none());
    }
}
