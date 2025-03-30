use anyhow::Result;
use shaysproxy::ProxyServer;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(false)
        .init();

    ProxyServer::bind("0.0.0.0:25565").await?.start().await
}
