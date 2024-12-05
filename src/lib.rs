#[macro_use]
extern crate smart_default;
#[macro_use]
extern crate tracing;

pub mod settings;

use std::{net::SocketAddr, sync::Arc};

use anyhow::{bail, Result};
use azalea_protocol::{
    connect::Connection,
    packets::{
        config::{ClientboundConfigPacket, ServerboundConfigPacket},
        game::{ClientboundGamePacket, ServerboundGamePacket},
        handshake::{ClientboundHandshakePacket, ServerboundHandshakePacket},
        login::{ClientboundLoginPacket, ServerboundLoginPacket},
        status::{ClientboundStatusPacket, ServerboundStatusPacket},
        ClientIntention,
        Packet,
    },
    read::ReadPacketError,
};
use tokio::net::{TcpListener, TcpStream};

#[derive(SmartDefault)]
pub struct ProxyServerBuilder {
    #[default("127.0.0.1:25566")]
    bind_addr: String,

    #[default("127.0.0.1:25565")]
    conn_addr: String,
}

impl ProxyServerBuilder {
    #[must_use]
    pub fn bind_addr(mut self, address: String) -> Self {
        self.bind_addr = address;
        self
    }

    #[must_use]
    pub fn conn_addr(mut self, address: String) -> Self {
        self.conn_addr = address;
        self
    }

    /// # Errors
    /// Will return `Err` if `TcpListener::bind` fails.
    pub async fn build(self) -> Result<ProxyServer> {
        let listener = TcpListener::bind(self.bind_addr).await?;
        let socket = str::parse(&self.conn_addr)?;

        Ok(ProxyServer::new(listener, socket))
    }
}

#[derive(Clone)]
pub struct ProxyServer {
    listener: Arc<TcpListener>,
    socket:   SocketAddr,
}

impl ProxyServer {
    #[must_use]
    pub fn builder() -> ProxyServerBuilder {
        ProxyServerBuilder::default()
    }

    pub fn new(listener: TcpListener, socket: SocketAddr) -> Self {
        Self {
            listener: Arc::new(listener),
            socket,
        }
    }

    /// # Errors
    /// Will return `Err` if `TcpListener::bind` fails.
    pub async fn start(&self) -> Result<()> {
        loop {
            let server = self.clone();
            let (stream, _addr) = self.listener.accept().await?;

            stream.set_nodelay(true)?;
            tokio::spawn(async move { Box::pin(server.handle_connection(stream)).await });
        }
    }

    /// # Errors
    /// Will return `Err` if transfer is initiated.
    pub async fn handle_connection(&self, stream: TcpStream) -> Result<()> {
        let mut c2s = Connection::new(&self.socket).await?;
        let mut s2c = Connection::<ServerboundHandshakePacket, ClientboundHandshakePacket>::wrap(stream);
        let ServerboundHandshakePacket::Intention(packet) = s2c.read().await?;
        c2s.write(ServerboundHandshakePacket::Intention(packet.clone())).await?;

        match packet.intention {
            ClientIntention::Status => self.handle_status(c2s.status(), s2c.status()).await,
            ClientIntention::Login => self.handle_login(c2s.login(), s2c.login()).await,
            ClientIntention::Transfer => bail!("Unsupported"),
        }
    }

    /// # Errors
    /// TODO
    pub async fn handle_status(
        &self,
        mut c2s: Connection<ClientboundStatusPacket, ServerboundStatusPacket>,
        mut s2c: Connection<ServerboundStatusPacket, ClientboundStatusPacket>,
    ) -> Result<()> {
        loop {
            tokio::select! {
                result = s2c.read() => {
                    match result {
                        Ok(packet) => {
                            info!("[Status] [C2S] {packet:#?}");

                            c2s.write(packet.into_variant()).await?
                        },
                        Err(error) => return match *error {
                            ReadPacketError::ConnectionClosed => {
                                info!("[Status] [C2S] ConnectionClosed");
                                Ok(())
                            },
                            error => Err(error.into())
                        },
                    }
                }
                result = c2s.read() => {
                    match result {
                        Ok(packet) => {
                            info!("[Status] [S2C] {packet:#?}");

                            s2c.write(packet.into_variant()).await?
                        },
                        Err(error) => return match *error {
                            ReadPacketError::ConnectionClosed => {
                                info!("[Status] [S2C] ConnectionClosed");
                                Ok(())
                            },
                            error => Err(error.into())
                        },
                    }
                }
            }
        }
    }

    /// # Errors
    /// TODO
    pub async fn handle_login(
        &self,
        mut c2s: Connection<ClientboundLoginPacket, ServerboundLoginPacket>,
        mut s2c: Connection<ServerboundLoginPacket, ClientboundLoginPacket>,
    ) -> Result<()> {
        #[allow(clippy::redundant_pub_crate)]
        loop {
            tokio::select! {
                result = s2c.read() => {
                    match result {
                        Ok(packet) => match packet {
                            packet => {
                                info!("[Login] [C2S] {packet:#?}");
                                c2s.write(packet.into_variant()).await?;
                            },
                        },
                        Err(error) => return match *error {
                            ReadPacketError::ConnectionClosed => {
                                info!("[Login] [C2S] ConnectionClosed");
                                Ok(())
                            },
                            error => Err(error.into())
                        },
                    }
                }
                result = c2s.read() => {
                    match result {
                        Ok(packet) => match packet {
                            ClientboundLoginPacket::LoginCompression(packet) => {
                                let threshold = packet.compression_threshold;
                                info!("[Login] [S2C] {packet:#?}");

                                s2c.write(packet).await?;
                                c2s.set_compression_threshold(threshold);
                                s2c.set_compression_threshold(threshold);
                            }
                            ClientboundLoginPacket::LoginFinished(packet) => {
                                info!("[Login] [S2C] LoginFinished (Login -> Config)");

                                s2c.write(packet).await?;
                                return Box::pin(self.handle_config(c2s.config(), s2c.config())).await;
                            }
                            packet => {
                                info!("[Login] [S2C] {packet:#?}");
                                s2c.write(packet.into_variant()).await?;
                            }
                        },
                        Err(error) => return match *error {
                            ReadPacketError::ConnectionClosed => {
                                info!("[Login] [S2C] ConnectionClosed");
                                Ok(())
                            },
                            error => Err(error.into())
                        },
                    }
                }
            }
        }
    }

    /// # Errors
    /// TODO
    pub async fn handle_config(
        &self,
        mut c2s: Connection<ClientboundConfigPacket, ServerboundConfigPacket>,
        mut s2c: Connection<ServerboundConfigPacket, ClientboundConfigPacket>,
    ) -> Result<()> {
        let mut ready = false;

        #[allow(clippy::redundant_pub_crate)]
        loop {
            tokio::select! {
                result = s2c.read() => {
                    match result {
                        Ok(packet) => match packet {
                            ServerboundConfigPacket::FinishConfiguration(packet) => {
                                info!("[Config] [C2S] FinishConfiguration (Config -> Game)");
                                c2s.write(packet).await?;

                                if ready {
                                    return self.handle_game(c2s.game(), s2c.game()).await;
                                }
                            }
                            packet => {
                                info!("[Config] [C2S] {packet:#?}");
                                c2s.write(packet.into_variant()).await?;
                            }
                        },
                        Err(error) => return match *error {
                            ReadPacketError::ConnectionClosed => {
                                info!("[Config] [C2S] ConnectionClosed");
                                Ok(())
                            },
                            error => Err(error.into())
                        },
                    }
                }
                result = c2s.read() => {
                    match result {
                        Ok(packet) => match packet {
                            ClientboundConfigPacket::FinishConfiguration(packet) => {
                                info!("[Config] [S2C] FinishConfiguration (Config -> Game)");
                                s2c.write(packet).await?;

                                if !ready {
                                    ready = true;
                                }
                            }
                            ClientboundConfigPacket::RegistryData(packet) => {
                                info!("[Config] [S2C] RegistryData (...)");
                                s2c.write(packet.into_variant()).await?;
                            },
                            ClientboundConfigPacket::UpdateTags(packet) => {
                                info!("[Config] [S2C] UpdateTags (...)");
                                s2c.write(packet.into_variant()).await?;
                            },
                            packet => {
                                info!("[Config] [S2C] {packet:#?}");
                                s2c.write(packet.into_variant()).await?;
                            },
                        },
                        Err(error) => return match *error {
                            ReadPacketError::ConnectionClosed => {
                                info!("[Config] [S2C] ConnectionClosed");
                                Ok(())
                            },
                            error => Err(error.into())
                        },
                    }
                }
            }
        }
    }

    /// # Errors
    /// TODO
    pub async fn handle_game(
        &self,
        mut c2s: Connection<ClientboundGamePacket, ServerboundGamePacket>,
        mut s2c: Connection<ServerboundGamePacket, ClientboundGamePacket>,
    ) -> Result<()> {
        #[allow(clippy::redundant_pub_crate)]
        loop {
            tokio::select! {
                result = s2c.read() => {
                    match result {
                        Ok(packet) => {
                                // info!("[Game] [C2S] {packet:#?}");
                                c2s.write(packet.into_variant()).await?;
                            },
                        Err(error) => return match *error {
                            ReadPacketError::ConnectionClosed => {
                                info!("[Game] [C2S] ConnectionClosed");
                                Ok(())
                            },
                            error => Err(error.into())
                        },
                    }
                }
                result = c2s.read() => {
                    match result {
                        Ok(packet) => match packet {
                            | ClientboundGamePacket::PlayerInfoUpdate(..)
                            | ClientboundGamePacket::SetEntityData(..)
                            | ClientboundGamePacket::UpdateAdvancements(..) => {}

                            ClientboundGamePacket::LevelChunkWithLight(packet) => {
                                info!("[Game] [S2C] LevelChunkWithLight (...)");
                                s2c.write(packet.into_variant()).await?;
                            }
                            ClientboundGamePacket::LightUpdate(packet) => {
                                info!("[Game] [S2C] LightUpdate (...)");
                                s2c.write(packet.into_variant()).await?;
                            }
                            ClientboundGamePacket::StartConfiguration(packet) => {
                                info!("[Game] [S2C] StartConfiguration (Game -> Config)");
                                s2c.write(packet).await?;
                                return Ok(());
                            }
                            packet => {
                                // info!("[Game] [S2C] {packet:#?}");
                                s2c.write(packet.into_variant()).await?;
                            },
                        },
                        Err(error) => return match *error {
                            ReadPacketError::ConnectionClosed => {
                                info!("[Game] [S2C] ConnectionClosed");
                                Ok(())
                            },
                            error => Err(error.into())
                        },
                    }
                }
            }
        }
    }
}
