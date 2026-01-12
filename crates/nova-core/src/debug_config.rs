use serde::{Deserialize, Serialize};

/// Debug adapter launch configuration.
///
/// This is intentionally small for now — it is enough for `nova-dap` to attach
/// to an already-running JVM that has JDWP enabled.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct LaunchConfig {
    /// Host to connect the JDWP client to (IP address or hostname). Defaults to `127.0.0.1`.
    pub host: Option<String>,
    /// JDWP port.
    pub port: Option<u16>,
}

/// Debug adapter attach configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachConfig {
    /// Host to connect the JDWP client to (IP address or hostname). Defaults to `127.0.0.1`.
    pub host: Option<String>,
    /// JDWP port.
    pub port: u16,
}
