//! Background delegation jobs, as clients see them.

use serde::{Deserialize, Serialize};

/// Why the server started a turn on its own.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TurnOrigin {
    /// `background`: finished background delegations are being reported.
    pub kind: String,
    /// The background jobs this turn reports.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub jobs: Vec<String>,
}

/// `TurnOrigin::kind` of a turn reporting finished background delegations.
pub const ORIGIN_BACKGROUND: &str = "background";

/// Background delegation jobs a `tool.completed` output shows starting or
/// settling. `agent_*` calls with `background: true` return
/// `{"job", "status":"running", "background":true}`; `agent_wait` and
/// `agent_status` return job objects (or `{"jobs":[...]}`) whose status is no
/// longer `running` once they finish. Clients use this to keep a session open
/// while its jobs run, so the jobs are not cancelled with it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BackgroundJobUpdate {
    /// Jobs that started running in the background.
    pub started: Vec<String>,
    /// Jobs that finished, failed, or were cancelled.
    pub settled: Vec<String>,
}

/// The jobs `output` shows starting or settling; empty for other output.
pub fn background_job_update(output: &str) -> BackgroundJobUpdate {
    let mut update = BackgroundJobUpdate::default();
    let Ok(serde_json::Value::Object(value)) = serde_json::from_str::<serde_json::Value>(output)
    else {
        return update;
    };
    let mut visit = |job: &serde_json::Map<String, serde_json::Value>| {
        let (Some(id), Some(status)) = (
            job.get("job").and_then(serde_json::Value::as_str),
            job.get("status").and_then(serde_json::Value::as_str),
        ) else {
            return;
        };
        if status == "running" {
            if job.get("background").and_then(serde_json::Value::as_bool) == Some(true) {
                update.started.push(id.to_owned());
            }
        } else {
            update.settled.push(id.to_owned());
        }
    };
    visit(&value);
    for job in value
        .get("jobs")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
    {
        if let serde_json::Value::Object(job) = job {
            visit(job);
        }
    }
    update
}
