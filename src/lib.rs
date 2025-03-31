#[macro_use]
extern crate tracing;

use std::{collections::VecDeque, net::SocketAddr, time::Duration};

use anyhow::{bail, Context, Error, Result};
use azalea_protocol::{
    connect::Connection,
    packets::handshake::{ClientboundHandshakePacket, ServerboundHandshakePacket},
};
use derive_more::{Deref, DerefMut};
use hickory_resolver::Resolver;
use tokio::{
    io::{copy_bidirectional, AsyncReadExt, AsyncWriteExt},
    net::{lookup_host, TcpListener, TcpStream, ToSocketAddrs},
    time::sleep,
};

#[derive(Debug, Deref, DerefMut)]
pub struct ProxyServer(TcpListener);

impl ProxyServer {
    /// Creates a new `ProxyServer` from a listener which you have already bound.
    pub const fn from(listener: TcpListener) -> Self {
        Self(listener)
    }

    /// Creates a new `ProxyServer`, which will be bound to the specified address.
    ///
    /// # Errors
    /// Will return `Err` if `TcpListener::bind` fails.
    pub async fn bind(addr: impl ToSocketAddrs) -> Result<Self> {
        TcpListener::bind(addr).await.map(Self::from).map_err(Error::from)
    }

    /// Listen for new incoming client connections.
    ///
    /// # Errors
    /// Will return `Err` if `TcpListener::accept` fails.
    pub async fn listen(&self) -> Result<()> {
        info!("Listening on {}", self.0.local_addr()?);
        loop {
            debug!("Waiting for incoming connection");
            let client = self.accept().await.map(ProxyClient::from)?;
            tokio::spawn(async move { client.task().await.map_err(|error| error!("{error}")) });
        }
    }
}

#[derive(Debug, Deref, DerefMut)]
pub struct ProxyClient {
    #[deref]
    #[deref_mut]
    stream: TcpStream,
    socket: SocketAddr,
    millis: Option<u64>,
}

impl ProxyClient {
    pub fn from((stream, socket): (TcpStream, SocketAddr)) -> Self {
        Self {
            stream,
            socket,
            millis: None,
        }
    }

    /// Parse the Minecraft handshake, modify, and send it, then relay the packets.
    ///
    /// # Errors
    /// Will return `Err` for a lot of reasons, I'm not explaining them all.
    pub async fn task(mut self) -> Result<()> {
        info!("New connection from {}", self.socket);

        /* Wrap the connection and try to parse the handshake packet with Azalea */
        let mut c2s = Connection::<ServerboundHandshakePacket, ClientboundHandshakePacket>::wrap(self.stream);
        let ServerboundHandshakePacket::Intention(mut intention) = c2s.read().await?;

        /* Try to parse the global wildcard subdomain and modify the handshake */
        if let Some(subdomain) = intention.host.split(".proxy.").next() {
            let mut parts = subdomain.split('_').collect::<VecDeque<_>>();
            let host = parts.pop_front().context("None")?.parse()?;
            let port = parts.pop_front().and_then(|port| port.parse().ok()).unwrap_or(25565);
            if let Some(millis) = parts.pop_front() {
                self.millis = Some(millis.parse()?);
            }

            intention.host = host;
            intention.port = port;
        }

        /* Try to resolve the srv / host to get the server address */
        let resolver = Resolver::builder_tokio()?.build();
        let query = format!("_minecraft._tcp.{}", intention.host);
        let socket = if let Ok(srv) = resolver.srv_lookup(query).await {
            let record = srv.iter().next().context("Missing srv record")?;
            let lookup = resolver.lookup_ip(record.target().to_ascii()).await?;
            let addr = lookup.iter().next().context("Missing addr")?;
            SocketAddr::new(addr, record.port())
        } else {
            let host = intention.host.to_string();
            let mut hosts = lookup_host(host).await?;
            hosts.next().context("Missing server host")?
        };

        /* Prevent recursive connections from killing the process */
        if socket.ip() == self.socket.ip() || self.socket.to_string().starts_with("192.168") {
            bail!("Recursive Connection!")
        }

        /* Connect to the target server, wrap, and send the modified handshake packet */
        let server = TcpStream::connect(socket).await?;
        let mut s2c = Connection::<ClientboundHandshakePacket, ServerboundHandshakePacket>::wrap(server);
        s2c.write(ServerboundHandshakePacket::Intention(intention)).await?;

        /* Unwrap the client and server to raw tcp streams because Azalea can't handle it */
        let mut server = s2c.unwrap()?;
        let mut client = c2s.unwrap()?;

        info!("Opened connection from {} -> {socket}", self.socket);

        if let Some(millis) = self.millis {
            Self::copy_bidirectional_delay(&mut client, &mut server, millis).await?;
        } else {
            copy_bidirectional(&mut client, &mut server).await?;
        }

        info!("Closed connection from {} -> {socket}", self.socket);

        Ok(())
    }

    /// Copy the packets bidirectionally between the client and server.
    ///
    /// # Errors
    /// Will return `Err` if `TcpStream::read` or `TcpStream::write` fails.
    pub async fn copy_bidirectional_delay(client: &mut TcpStream, server: &mut TcpStream, millis: u64) -> Result<()> {
        let mut client_buf = vec![0u8; u16::MAX as usize];
        let mut server_buf = vec![0u8; u16::MAX as usize];

        loop {
            sleep(Duration::from_millis(millis)).await;

            tokio::select! {
                client_result = client.read(&mut client_buf) => {
                    match client_result {
                        Ok(0) => break, // Client closed connection
                        Ok(length) => server.write_all(&client_buf[..length]).await?,
                        Err(error) => bail!(error),
                    }
                }

                server_result = server.read(&mut server_buf) => {
                    match server_result {
                        Ok(0) => break, // Server closed connection
                        Ok(length) => client.write_all(&server_buf[..length]).await?,
                        Err(error) => bail!(error),
                    }
                }
            }
        }

        Ok(())
    }
}
