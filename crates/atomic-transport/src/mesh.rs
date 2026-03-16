use crate::protocol::MeshMessage;
use quinn::{Endpoint, ServerConfig};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use tracing::{error, info, warn};

/// Peer connection tracking.
#[derive(Debug)]
#[allow(dead_code)]
struct Peer {
    addr: SocketAddr,
    node_id: [u8; 32],
    connection: quinn::Connection,
}

/// QUIC-based mesh transport layer.
/// Each node runs a QUIC server and connects to peers as a client.
pub struct MeshTransport {
    #[allow(dead_code)]
    node_id: [u8; 32],
    bind_addr: SocketAddr,
    endpoint: Option<Endpoint>,
    peers: Arc<RwLock<HashMap<[u8; 32], Peer>>>,
    incoming_tx: mpsc::Sender<MeshMessage>,
    incoming_rx: Option<mpsc::Receiver<MeshMessage>>,
}

impl MeshTransport {
    pub fn new(node_id: [u8; 32], bind_addr: SocketAddr, buffer_size: usize) -> Self {
        let (tx, rx) = mpsc::channel(buffer_size);
        Self {
            node_id,
            bind_addr,
            endpoint: None,
            peers: Arc::new(RwLock::new(HashMap::new())),
            incoming_tx: tx,
            incoming_rx: Some(rx),
        }
    }

    /// Start the QUIC server listener.
    pub async fn start(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let (server_config, _cert) = self.generate_self_signed_config()?;
        let endpoint = Endpoint::server(server_config, self.bind_addr)?;
        info!("Mesh transport listening on {}", self.bind_addr);

        let incoming_tx = self.incoming_tx.clone();
        let peers = self.peers.clone();
        let ep = endpoint.clone();

        // Accept incoming connections
        tokio::spawn(async move {
            while let Some(incoming) = ep.accept().await {
                let tx = incoming_tx.clone();
                let _peers = peers.clone();
                tokio::spawn(async move {
                    match incoming.await {
                        Ok(conn) => {
                            info!("Peer connected from {}", conn.remote_address());
                            Self::handle_connection(conn, tx).await;
                        }
                        Err(e) => {
                            warn!("Failed to accept connection: {}", e);
                        }
                    }
                });
            }
        });

        self.endpoint = Some(endpoint);
        Ok(())
    }

    /// Connect to a peer node.
    pub async fn connect_to_peer(
        &self,
        peer_id: [u8; 32],
        addr: SocketAddr,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let endpoint = self
            .endpoint
            .as_ref()
            .ok_or("transport not started")?;

        let conn = endpoint.connect(addr, "atomic-mesh")?.await?;
        info!("Connected to peer {} at {}", hex::encode(&peer_id[..8]), addr);

        let peer = Peer {
            addr,
            node_id: peer_id,
            connection: conn.clone(),
        };
        self.peers.write().await.insert(peer_id, peer);

        // Spawn reader for this connection
        let tx = self.incoming_tx.clone();
        tokio::spawn(async move {
            Self::handle_connection(conn, tx).await;
        });

        Ok(())
    }

    /// Send a message to a specific peer.
    pub async fn send_to(
        &self,
        peer_id: &[u8; 32],
        msg: &MeshMessage,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let peers = self.peers.read().await;
        let peer = peers.get(peer_id).ok_or("peer not found")?;

        let data = msg.encode()?;
        let mut send = peer.connection.open_uni().await?;
        let len = (data.len() as u32).to_le_bytes();
        send.write_all(&len).await?;
        send.write_all(&data).await?;
        send.finish()?;

        Ok(())
    }

    /// Broadcast a message to all connected peers.
    pub async fn broadcast(&self, msg: &MeshMessage) {
        let peers = self.peers.read().await;
        let data = match msg.encode() {
            Ok(d) => d,
            Err(e) => {
                error!("Failed to encode message: {}", e);
                return;
            }
        };

        for (id, peer) in peers.iter() {
            match peer.connection.open_uni().await {
                Ok(mut send) => {
                    let len = (data.len() as u32).to_le_bytes();
                    if let Err(e) = send.write_all(&len).await {
                        warn!("Failed to send to {}: {}", hex::encode(&id[..8]), e);
                        continue;
                    }
                    if let Err(e) = send.write_all(&data).await {
                        warn!("Failed to send to {}: {}", hex::encode(&id[..8]), e);
                        continue;
                    }
                    let _ = send.finish();
                }
                Err(e) => {
                    warn!("Failed to open stream to {}: {}", hex::encode(&id[..8]), e);
                }
            }
        }
    }

    /// Take the incoming message receiver (can only be called once).
    pub fn take_receiver(&mut self) -> Option<mpsc::Receiver<MeshMessage>> {
        self.incoming_rx.take()
    }

    pub async fn peer_count(&self) -> usize {
        self.peers.read().await.len()
    }

    async fn handle_connection(conn: quinn::Connection, tx: mpsc::Sender<MeshMessage>) {
        loop {
            match conn.accept_uni().await {
                Ok(mut recv) => {
                    let tx = tx.clone();
                    tokio::spawn(async move {
                        let mut len_buf = [0u8; 4];
                        if recv.read_exact(&mut len_buf).await.is_err() {
                            return;
                        }
                        let len = u32::from_le_bytes(len_buf) as usize;
                        // Limit message size to 16 MB
                        if len > 16 * 1024 * 1024 {
                            warn!("Message too large: {} bytes", len);
                            return;
                        }
                        let mut data = vec![0u8; len];
                        if recv.read_exact(&mut data).await.is_err() {
                            return;
                        }

                        match MeshMessage::decode(&data) {
                            Ok(msg) => {
                                let _ = tx.send(msg).await;
                            }
                            Err(e) => {
                                warn!("Failed to decode mesh message: {}", e);
                            }
                        }
                    });
                }
                Err(_) => break,
            }
        }
    }

    fn generate_self_signed_config(
        &self,
    ) -> Result<(ServerConfig, Vec<u8>), Box<dyn std::error::Error + Send + Sync>> {
        let cert = rcgen::generate_simple_self_signed(vec!["atomic-mesh".to_string()])?;
        let cert_der = cert.cert.der().to_vec();
        let key_der = cert.key_pair.serialize_der();

        let cert_chain = vec![rustls::pki_types::CertificateDer::from(cert_der.clone())];
        let key = rustls::pki_types::PrivateKeyDer::try_from(key_der)?;

        let server_crypto = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(cert_chain, key)?;

        let server_config = ServerConfig::with_crypto(Arc::new(
            quinn::crypto::rustls::QuicServerConfig::try_from(server_crypto)?,
        ));

        Ok((server_config, cert_der))
    }
}
