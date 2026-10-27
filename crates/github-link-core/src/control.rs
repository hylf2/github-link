use crate::{AccelerationResult, DaemonStatus, MirrorCandidate, RouteMode};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum ControlRequest {
    Status,
    Probe { host: Option<String> },
    RefreshMirrors,
    Accelerate { url: String },
    SetRoute { mode: RouteMode },
    Reload,
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlResponse {
    Status { status: DaemonStatus },
    Mirrors { candidates: Vec<MirrorCandidate> },
    Acceleration { result: AccelerationResult },
    Ok { message: String },
    Error { message: String },
}
