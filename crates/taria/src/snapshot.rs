use serde::{Deserialize, Serialize};

use crate::Node;

/// One published state of the app's semantic tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub protocol_version: u32,
    /// Monotonic sequence number so agents can detect staleness.
    pub seq: u64,
    pub root: Node,
}
