use std::io::ErrorKind;

use anyhow::Result;
use derive_config::{ConfigError, DeriveTomlConfig};
use shaysproxy::{settings::Settings, ProxyServer};

#[tokio::main]
async fn main() -> Result<()> {
    let settings = Settings::load().unwrap_or_else(|error| match error {
        ConfigError::Io(error) if error.kind() == ErrorKind::NotFound => Settings::default(),
        error => panic!("{error}"),
    });
    
    settings.save()?; /* Save the settings on first run */

    ProxyServer::builder()
        .bind_addr(settings.bind_addr)
        .conn_addr(settings.conn_addr)
        .build()
        .await?
        .start()
        .await
}
