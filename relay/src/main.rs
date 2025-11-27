//! P2P WebTransport Relay Server
//!
//! A central hub relay server that enables:
//! - Browser <-> Browser connectivity via WebTransport
//! - Browser <-> Server connectivity
//! - Server <-> Server connectivity
//! - Virtual network with DHCP-like IP assignment (10.0.2.x)
//! - External traffic proxy (DNS, ICMP) for VMs

mod hub;
mod peer;
mod protocol;
mod proxy;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use tokio::sync::mpsc;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;
use wtransport::{Endpoint, Identity, ServerConfig};

use crate::hub::{Hub, PeerMessage};
use crate::peer::PeerId;
use crate::protocol::{encode_data_frame, ControlMessage, MSG_TYPE_CONTROL};

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about = "P2P WebTransport Relay Server for RISC-V VM networking"
)]
struct Args {
    /// Port to listen on (UDP/QUIC)
    #[arg(short, long, default_value_t = 4433)]
    port: u16,

    /// Bind address
    #[arg(short, long, default_value = "0.0.0.0")]
    bind: String,

    /// Heartbeat interval in seconds
    #[arg(long, default_value_t = 30)]
    heartbeat_interval: u64,

    /// Peer timeout in seconds
    #[arg(long, default_value_t = 90)]
    peer_timeout: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    info!("Starting P2P WebTransport Relay Server...");
    info!("Virtual Network: 10.0.2.0/24, Gateway: 10.0.2.2");

    // Generate self-signed certificate
    let identity = Identity::self_signed(["localhost", "127.0.0.1", "::1"])?;
    let cert_hash = identity
        .certificate_chain()
        .as_slice()
        .first()
        .unwrap()
        .hash();
    // Format hash without colons for easy copy-paste
    let cert_hash_hex = format!("{}", cert_hash).replace(":", "");
    info!("Certificate Hash: {}", cert_hash_hex);
    info!("Use this hash with --net-cert-hash when connecting");

    // Create the central hub
    let hub = Arc::new(Hub::new());

    // Initialize the external proxy
    if let Err(e) = hub.proxy().init().await {
        warn!("Failed to initialize external proxy: {}", e);
    }

    // Spawn the UDP response receiver for external proxy
    let hub_clone = hub.clone();
    tokio::spawn(async move {
        run_proxy_receiver(hub_clone).await;
    });

    // Spawn the peer cleanup task
    let hub_clone = hub.clone();
    let timeout = args.peer_timeout;
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(timeout / 3));
        loop {
            interval.tick().await;
            hub_clone.cleanup_expired_peers().await;
            hub_clone.log_stats().await;
        }
    });

    // Setup WebTransport server
    let socket_addr = format!("{}:{}", args.bind, args.port).parse()?;

    let config = ServerConfig::builder()
        .with_bind_address(socket_addr)
        .with_identity(identity)
        .build();

    let endpoint = Endpoint::server(config)?;

    info!("Listening on https://{}:{}", args.bind, args.port);

    // Accept incoming sessions
    loop {
        let incoming_session = endpoint.accept().await;
        let hub = hub.clone();

        tokio::spawn(async move {
            if let Err(e) = handle_connection(incoming_session, hub).await {
                warn!("Connection error: {}", e);
            }
        });
    }
}

/// Handle a single WebTransport connection
async fn handle_connection(
    incoming: wtransport::endpoint::IncomingSession,
    hub: Arc<Hub>,
) -> Result<()> {
    let request = incoming.await?;
    info!("New connection from {:?}", request.remote_address());

    let connection = request.accept().await?;
    info!("Session established with {:?}", connection.remote_address());

    // Create channel for sending to this peer
    let (tx, mut rx) = mpsc::channel::<PeerMessage>(256);

    // Wait for registration message
    let peer_id: PeerId;
    let assigned_ip: [u8; 4];

    loop {
        tokio::select! {
            result = connection.receive_datagram() => {
                match result {
                    Ok(datagram) => {
                        let data = datagram.to_vec();
                        if !data.is_empty() && data[0] == MSG_TYPE_CONTROL {
                            if let Ok(ControlMessage::Register { mac }) = ControlMessage::decode(&data) {
                                // Register the peer
                                match hub.register_peer(mac, tx.clone()).await {
                                    Some((id, ip)) => {
                                        peer_id = id;
                                        assigned_ip = ip;
                                        info!(
                                            "Peer {} registered: MAC={}, IP={}",
                                            peer_id,
                                            protocol::format_mac(&mac),
                                            protocol::format_ip(&ip)
                                        );
                                        break;
                                    }
                                    None => {
                                        let err = ControlMessage::Error {
                                            message: "IP pool exhausted".to_string(),
                                        };
                                        let _ = connection.send_datagram(err.encode());
                                        return Ok(());
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        warn!("Connection closed during registration: {}", e);
                        return Ok(());
                    }
                }
            }
            _ = tokio::time::sleep(Duration::from_secs(30)) => {
                warn!("Registration timeout");
                return Ok(());
            }
        }
    }

    // Subscribe to broadcast channel
    let mut broadcast_rx = hub.subscribe();

    // Main message loop
    loop {
        tokio::select! {
            // Receive from client
            result = connection.receive_datagram() => {
                match result {
                    Ok(datagram) => {
                        let data = datagram.to_vec();
                        hub.touch_peer(peer_id).await;
                        hub.route_frame(peer_id, data).await;
                    }
                    Err(e) => {
                        info!("Peer {} disconnected: {}", peer_id, e);
                        break;
                    }
                }
            }

            // Send to client (from hub routing)
            Some(msg) = rx.recv() => {
                match msg {
                    PeerMessage::Send(data) => {
                        if let Err(e) = connection.send_datagram(data) {
                            warn!("Failed to send to peer {}: {}", peer_id, e);
                            break;
                        }
                    }
                    PeerMessage::Disconnect => {
                        info!("Peer {} kicked by hub", peer_id);
                        break;
                    }
                }
            }

            // Broadcast messages (from other peers)
            Ok((from_peer, data)) = broadcast_rx.recv() => {
                if from_peer != peer_id {
                    if let Err(e) = connection.send_datagram(data) {
                        warn!("Failed to broadcast to peer {}: {}", peer_id, e);
                        break;
                    }
                }
            }
        }
    }

    // Cleanup
    hub.unregister_peer(peer_id).await;
    info!(
        "Peer {} unregistered (IP {})",
        peer_id,
        protocol::format_ip(&assigned_ip)
    );

    Ok(())
}

/// Run the external proxy UDP receiver loop
async fn run_proxy_receiver(hub: Arc<Hub>) {
    loop {
        let socket = hub.proxy().udp_socket().await;

        if let Some(socket) = socket {
            let mut buf = [0u8; 2048];
            loop {
                match socket.recv_from(&mut buf).await {
                    Ok((n, src_addr)) => {
                        if let Some(response_frame) =
                            hub.proxy().handle_incoming_udp(&buf, src_addr, n).await
                        {
                            // Need to send this response to the right peer
                            // The response frame contains the destination MAC/IP
                            // which we can use to route it
                            broadcast_response(&hub, &response_frame).await;
                        }
                    }
                    Err(e) => {
                        warn!("Proxy UDP recv error: {}", e);
                        break;
                    }
                }
            }
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Broadcast a proxy response to the appropriate peer
async fn broadcast_response(hub: &Hub, ethernet_frame: &[u8]) {
    if ethernet_frame.len() < 14 {
        return;
    }

    let dst_mac: [u8; 6] = ethernet_frame[0..6].try_into().unwrap();

    // Find the peer with this MAC
    let peers_arc = hub.peers();
    let peers = peers_arc.read().await;
    if let Some(peer) = peers.find_by_mac(&dst_mac) {
        let peer_id = peer.id;
        drop(peers);
        hub.send_to_peer(peer_id, encode_data_frame(ethernet_frame))
            .await;
    }
}
