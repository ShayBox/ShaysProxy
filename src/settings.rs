use derive_config::DeriveTomlConfig;
use serde::{Deserialize, Serialize};

#[derive(
    Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, DeriveTomlConfig, Deserialize, Serialize, SmartDefault,
)]
pub struct Settings {
    /// The interface and port the proxy will bind to.
    #[default("127.0.0.1:25566")]
    pub bind_addr: String,

    /// Minecraft server the proxy will connect to.
    #[default("127.0.0.1:25565")]
    pub conn_addr: String,
}
