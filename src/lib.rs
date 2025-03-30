#[macro_use]
extern crate tracing;

use std::{collections::VecDeque, fmt::Display, net::SocketAddr};

use anyhow::{bail, Context, Error, Result};
use derive_more::{Deref, DerefMut};
use hickory_resolver::Resolver;
use tokio::{
    io::{copy_bidirectional, AsyncReadExt, AsyncWriteExt},
    net::{lookup_host, TcpListener, TcpStream, ToSocketAddrs},
    time::{sleep, Duration},
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
        TcpListener::bind(addr).await.map(Self::from).map_err(Error::msg)
    }

    /// Start listening for new incoming client connections.
    ///
    /// # Errors
    /// Will return `Err` if `TcpListener::accept` fails.
    pub async fn start(&self) -> Result<()> {
        info!("Listening for new client connections on {}", self.local_addr()?);
        loop {
            let connection = self.accept().await?;
            let mut client = ProxyClient::from(connection);
            tokio::spawn(async move {
                if let Err(error) = client.start().await {
                    error!("Client connection error: {error}");
                }
            });
        }
    }
}

#[derive(Debug, Deref, DerefMut)]
pub struct ProxyClient {
    #[deref]
    #[deref_mut]
    pub stream: TcpStream,
    pub addr:   SocketAddr,
}

impl ProxyClient {
    pub fn from((stream, addr): (TcpStream, SocketAddr)) -> Self {
        Self { stream, addr }
    }

    /// Parse the Minecraft handshake, modify, and send it, then relay the packets.
    ///
    /// # Errors
    /// Will return `Err` for a lot of reasons, I'm not explaining them all.
    pub async fn start(&mut self) -> Result<()> {
        let client_addr = self.addr;
        info!("New client connection from {client_addr}");

        /* Read the packet length byte */
        let mut byte = [0u8; 1];
        self.read_exact(&mut byte).await?;

        /* Read the packet bytes */
        let length = read_varint(&mut &byte[..])?;
        let mut bytes = vec![0u8; usize::try_from(length)?];
        self.read_exact(&mut bytes).await?;

        /* Parse the handshake packet and modify the addr and port */
        let mut handshake = HandshakePacket::try_from(bytes)?;
        let mut delay = None;
        if let Some(subdomain) = handshake.addr.split(".proxy.").next() {
            let mut parts = subdomain.split('_').collect::<VecDeque<_>>();
            let addr = parts.pop_front().context("Missing addr")?.parse()?;
            let port = parts.pop_front().and_then(|port| port.parse().ok()).unwrap_or(25565);
            delay = parts.pop_front().and_then(|millis| millis.parse().ok());
            handshake.addr = addr;
            handshake.port = port;
        }

        /* Try to resolve the srv / host to get the server address */
        let resolver = Resolver::builder_tokio()?.build();
        let query = format!("_minecraft._tcp.{}", handshake.addr);
        let server_addr = if let Ok(srv) = resolver.srv_lookup(query).await {
            let record = srv.iter().next().context("Missing srv record")?;
            let lookup = resolver.lookup_ip(record.target().to_ascii()).await?;
            let addr = lookup.iter().next().context("Missing addr")?;
            SocketAddr::new(addr, record.port())
        } else {
            let host = handshake.to_string();
            let mut hosts = lookup_host(host).await?;
            hosts.next().context("Missing server address")?
        };

        if server_addr.ip() == self.addr.ip() || self.addr.to_string().starts_with("192.168") {
            bail!("Recursive Connection!")
        }

        /* Write the modified handshake packet to the server */
        let mut server = TcpStream::connect(server_addr).await?;
        let src = TryInto::<Vec<_>>::try_into(handshake)?;
        server.write_all(&src).await?;
        server.set_nodelay(true)?;
        self.set_nodelay(true)?;

        // TODO: Handle the status and login packets

        info!("Connection from {client_addr} -> {server_addr} opened");

        if let Some(millis) = delay {
            self.copy_bidirectional_delay(&mut server, millis).await?;
        } else {
            let (c2s, s2c) = copy_bidirectional(&mut self.stream, &mut server).await?;
            debug!("Transferred: C2S: {c2s} bytes, S2C: {s2c} bytes");
        }

        info!("Connection from {client_addr} -> {server_addr} closed");

        let _ = self.shutdown().await;
        let _ = server.shutdown().await;

        Ok(())
    }

    /// Copy the packets bidirectionally between the client and server.
    ///
    /// # Errors
    /// Will return `Err` if `TcpStream::read` or `TcpStream::write` fails.
    pub async fn copy_bidirectional_delay(&mut self, server: &mut TcpStream, millis: u64) -> Result<()> {
        let mut client_buf = vec![0u8; u16::MAX as usize];
        let mut server_buf = vec![0u8; u16::MAX as usize];

        loop {
            sleep(Duration::from_millis(millis)).await;

            tokio::select! {
                client_result = self.read(&mut client_buf) => {
                    match client_result {
                        Ok(0) => break, // Client closed connection
                        Ok(length) => server.write_all(&client_buf[..length]).await?,
                        Err(error) => bail!(error),
                    }
                }

                server_result = server.read(&mut server_buf) => {
                    match server_result {
                        Ok(0) => break, // Server closed connection
                        Ok(length) => self.write_all(&server_buf[..length]).await?,
                        Err(error) => bail!(error),
                    }
                }
            }
        }

        Ok(())
    }
}

pub type UShort = u16;
pub type VarInt = i32;
pub enum State {
    Status   = 1,
    Login    = 2,
    Transfer = 3,
}

pub struct HandshakePacket {
    pver: VarInt,
    addr: String,
    port: UShort,
    next: State,
}

impl Display for HandshakePacket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.addr, self.port)
    }
}

impl TryFrom<Vec<u8>> for HandshakePacket {
    type Error = Error;

    fn try_from(bytes: Vec<u8>) -> Result<Self, Self::Error> {
        let buf = &mut &bytes[..];
        let pid = read_varint(buf)?;

        if pid != 0x00 {
            bail!("Invalid Packet ID: {pid}")
        }

        Ok(Self {
            pver: read_varint(buf).context("pver")?,
            addr: read_string(buf).context("addr")?,
            port: read_ushort(buf).context("port")?,
            next: match read_varint(buf).context("next")? {
                1 => State::Status,
                2 => State::Login,
                3 => State::Transfer,
                state => bail!("Invalid State: {state}"),
            },
        })
    }
}

impl TryInto<Vec<u8>> for HandshakePacket {
    type Error = Error;

    fn try_into(self) -> Result<Vec<u8>, Self::Error> {
        let mut buf = Vec::new();
        buf.extend(write_varint(0x00)?);
        buf.extend(write_varint(self.pver)?);
        buf.extend(write_string(&self.addr)?);
        buf.extend(self.port.to_be_bytes());
        buf.extend(write_varint(self.next as VarInt)?);

        let len = VarInt::try_from(buf.len())?;
        let mut packet = write_varint(len)?;
        packet.extend(buf);

        Ok(packet)
    }
}

fn read_varint(buf: &mut &[u8]) -> Result<VarInt> {
    let mut result = 0;
    let mut shift = 0;

    loop {
        if buf.is_empty() {
            bail!("Unexpected end of buffer while reading VarInt");
        }

        let byte = buf[0];
        *buf = &buf[1..];

        let value = i32::from(byte & 0b0111_1111);
        result |= value << shift;

        if byte & 0b1000_0000 == 0 {
            break;
        }

        shift += 7;
        if shift > 35 {
            bail!("VarInt too big");
        }
    }
    Ok(result)
}

fn read_string(buf: &mut &[u8]) -> Result<String> {
    let len = read_varint(buf)?;
    if len < 0 {
        bail!("Invalid string length: {len}");
    }

    let len = usize::try_from(len)?;
    if buf.len() < len {
        bail!("Unexpected end of buffer while reading String");
    }

    let bytes = &buf[..len];
    *buf = &buf[len..];

    String::from_utf8(bytes.to_vec()).map_err(Error::from)
}

fn read_ushort(buf: &mut &[u8]) -> Result<UShort> {
    if buf.len() < 2 {
        bail!("Unexpected end of buffer while reading UShort");
    }

    let value = u16::from_be_bytes([buf[0], buf[1]]);
    *buf = &buf[2..];

    Ok(value)
}

fn write_varint(mut int: VarInt) -> Result<Vec<u8>> {
    let mut buf = Vec::new();

    loop {
        let mut byte = u8::try_from(int & 0b0111_1111)?;
        int >>= 7;

        if int != 0 {
            byte |= 0b1000_0000;
        }

        buf.push(byte);

        if int == 0 {
            break;
        }
    }

    Ok(buf)
}

fn write_string(str: &str) -> Result<Vec<u8>> {
    let len = str.len();
    let int = VarInt::try_from(len)?;
    let mut buf = write_varint(int)?;
    buf.extend_from_slice(str.as_bytes());

    Ok(buf)
}
