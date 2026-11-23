use std::time::Duration;

#[derive(Clone, Debug, Default)]
pub struct BurnReport {
    pub bytes_sent: usize,
    pub transfer_time: Duration,
    pub connection_time: Duration,
    pub reconnect_time: Duration,
    pub components_completed: usize,
    pub device_crcs_verified: usize,
    pub upgrade_ended: bool,
    pub reset: ResetOutcome,
}

#[derive(Clone, Debug, Default)]
pub enum ResetOutcome {
    #[default]
    NotAttempted,
    Skipped,
    Acknowledged,
    Unconfirmed(String),
    Failed(String),
}

#[derive(Clone, Debug)]
/// Progress reported synchronously during an upgrade.
pub enum BurnEvent {
    Log(String),
    Stage(String),
    ComponentStarted {
        name: String,
        partition: String,
        size: usize,
    },
    ComponentProgress {
        name: String,
        sent: usize,
        total: usize,
    },
    OverallProgress {
        sent: usize,
        total: usize,
    },
    ComponentFinished {
        name: String,
    },
    Finished,
}

impl BurnEvent {
    /// Encode one event using the CLI and GUI burn log schema.
    pub fn to_json(&self, elapsed: Duration) -> serde_json::Value {
        use serde_json::json;
        let elapsed_secs = elapsed.as_secs_f64();
        let rate = |sent: usize| sent as f64 / elapsed_secs.max(0.001);
        match self {
            Self::Log(line) => json!({ "type": "log", "line": line, "elapsed_secs": elapsed_secs }),
            Self::Stage(line) => {
                json!({ "type": "stage", "line": line, "elapsed_secs": elapsed_secs })
            }
            Self::ComponentStarted {
                name,
                partition,
                size,
            } => {
                json!({ "type": "component_started", "name": name, "partition": partition, "size": size, "elapsed_secs": elapsed_secs })
            }
            Self::ComponentProgress { name, sent, total } => {
                json!({ "type": "component_progress", "name": name, "sent": sent, "total": total, "elapsed_secs": elapsed_secs, "rate_bps": rate(*sent) })
            }
            Self::OverallProgress { sent, total } => {
                json!({ "type": "overall_progress", "sent": sent, "total": total, "elapsed_secs": elapsed_secs, "rate_bps": rate(*sent) })
            }
            Self::ComponentFinished { name } => {
                json!({ "type": "component_finished", "name": name, "elapsed_secs": elapsed_secs })
            }
            Self::Finished => json!({ "type": "finished", "elapsed_secs": elapsed_secs }),
        }
    }
}

/// Callback invoked on the thread executing the upgrade.
pub type BurnCallback<'a> = dyn FnMut(BurnEvent) + Send + 'a;

pub(super) fn emit(callback: &mut Option<&mut BurnCallback<'_>>, event: BurnEvent) {
    if let Some(callback) = callback.as_deref_mut() {
        callback(event);
    }
}
