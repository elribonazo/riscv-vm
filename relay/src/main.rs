//! libp2p QUIC Relay Service with NAT Gateway
//!
//! A relay server that enables NAT traversal and peer connectivity for:
//! - Server to server (direct QUIC)
//! - Browser to server (WebSocket to QUIC bridge)
//! - Browser to browser (via relay + DCUtR hole punching)
//! - NAT traversal (circuit relay v2 + DCUtR)
//! - **NAT Gateway for external network access (ping, DNS)**
//!
//! Usage:
//!   cargo run --release -- --port 4001
//!
//! Features:
//! - QUIC transport for efficient, secure connections
//! - TCP + WebSocket for browser compatibility
//! - Circuit Relay v2 for NAT traversal
//! - DCUtR for direct connection upgrade (hole punching)
//! - Kademlia DHT for peer discovery
//! - AutoNAT for NAT detection
//! - Gossipsub for pub/sub messaging
//! - **NAT Gateway**: Routes external traffic (ICMP, UDP/DNS) to the internet

use anyhow::Result;
use clap::Parser;
use futures::{SinkExt, StreamExt};
use libp2p::{
    autonat,
    dcutr,
    gossipsub::{self, IdentTopic, MessageAuthenticity},
    identify,
    identity::Keypair,
    kad::{self, store::MemoryStore, Mode as KadMode},
    noise,
    ping,
    relay,
    request_response::{self, ProtocolSupport},
    swarm::{NetworkBehaviour, SwarmEvent},
    tcp, yamux, Multiaddr, PeerId, StreamProtocol, Swarm, SwarmBuilder,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    error::Error,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::Arc,
    time::Duration,
};
use tokio::{
    net::{TcpListener, TcpStream, UdpSocket},
    sync::{broadcast, mpsc, Mutex as TokioMutex},
};
use tokio_tungstenite::{accept_async, tungstenite::Message};
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about = "libp2p QUIC Relay Server for NAT traversal and peer connectivity"
)]
struct Args {
    /// QUIC port to listen on
    #[arg(short, long, default_value_t = 4001)]
    port: u16,

    /// TCP port for connections
    #[arg(long, default_value_t = 4002)]
    tcp_port: u16,

    /// WebSocket port for browser connections
    #[arg(long, default_value_t = 8765)]
    ws_port: u16,

    /// Bind address
    #[arg(short, long, default_value = "0.0.0.0")]
    bind: String,

    /// External address to announce (for servers behind NAT/load balancer)
    #[arg(long)]
    external_addr: Option<String>,

    /// Enable verbose logging
    #[arg(short, long)]
    verbose: bool,

    /// Relay reservation duration in seconds
    #[arg(long, default_value_t = 3600)]
    reservation_duration: u64,

    /// Maximum number of relay reservations
    #[arg(long, default_value_t = 1024)]
    max_reservations: usize,

    /// Maximum circuits per peer
    #[arg(long, default_value_t = 16)]
    max_circuits_per_peer: usize,
}

/// Custom protocol for application-level messages
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RelayMessage {
    /// Broadcast data to all peers in a topic
    Broadcast { topic: String, data: Vec<u8> },
    /// Direct message to a specific peer
    Direct { data: Vec<u8> },
    /// Peer discovery request
    DiscoverPeers,
    /// Peer list response
    PeerList { peers: Vec<String> },
}

/// Combined network behaviour for the relay
#[derive(NetworkBehaviour)]
struct RelayServerBehaviour {
    /// Circuit relay v2 (server mode) for NAT traversal
    relay: relay::Behaviour,

    /// DCUtR for direct connection upgrade after relay
    dcutr: dcutr::Behaviour,

    /// Identify protocol for peer info exchange
    identify: identify::Behaviour,

    /// Ping for connection liveness
    ping: ping::Behaviour,

    /// Kademlia DHT for peer discovery
    kademlia: kad::Behaviour<MemoryStore>,

    /// AutoNAT for NAT detection
    autonat: autonat::Behaviour,

    /// Gossipsub for pub/sub messaging
    gossipsub: gossipsub::Behaviour,

    /// Request-response for direct messaging
    request_response: request_response::cbor::Behaviour<RelayMessage, RelayMessage>,
}

/// Relay server state
struct RelayServer {
    /// The libp2p swarm
    swarm: Swarm<RelayServerBehaviour>,

    /// Connected peers
    connected_peers: HashSet<PeerId>,

    /// Peer addresses for routing
    peer_addresses: HashMap<PeerId, Vec<Multiaddr>>,

    /// Active relay reservations
    reservations: HashSet<PeerId>,

    /// Topics we're subscribed to
    topics: HashSet<String>,

    /// Channel to receive packets from WebSocket clients to publish to gossipsub
    ws_to_gossip_rx: mpsc::Receiver<Vec<u8>>,

    /// Channel to send packets from gossipsub to WebSocket clients
    gossip_to_ws_tx: broadcast::Sender<Vec<u8>>,
}

impl RelayServer {
    /// Create a new relay server
    async fn new(
        args: &Args,
        ws_to_gossip_rx: mpsc::Receiver<Vec<u8>>,
        gossip_to_ws_tx: broadcast::Sender<Vec<u8>>,
    ) -> Result<Self> {
        // Generate a persistent keypair (in production, load from file)
        let local_key = Keypair::generate_ed25519();
        let local_peer_id = PeerId::from(local_key.public());
        info!("Local peer ID: {}", local_peer_id);

        // Build the swarm with TCP and QUIC transports
        let swarm = SwarmBuilder::with_existing_identity(local_key.clone())
            .with_tokio()
            .with_tcp(
                tcp::Config::default(),
                noise::Config::new,
                yamux::Config::default,
            )?
            .with_quic()
            .with_dns()?
            .with_behaviour(|keypair| {
                // Relay behaviour configuration
                let relay_config = relay::Config {
                    reservation_duration: Duration::from_secs(args.reservation_duration),
                    max_reservations: args.max_reservations,
                    max_circuits_per_peer: args.max_circuits_per_peer,
                    ..Default::default()
                };

                // Identify configuration
                let identify = identify::Behaviour::new(identify::Config::new(
                    "/riscv-relay/1.0.0".to_string(),
                    keypair.public(),
                ));

                // Kademlia configuration for DHT
                let store = MemoryStore::new(keypair.public().to_peer_id());
                let mut kademlia_config = kad::Config::new(StreamProtocol::new("/riscv-kad/1.0.0"));
                kademlia_config.set_query_timeout(Duration::from_secs(60));
                let mut kademlia = kad::Behaviour::with_config(
                    keypair.public().to_peer_id(),
                    store,
                    kademlia_config,
                );
                kademlia.set_mode(Some(KadMode::Server));

                // Gossipsub configuration - use Permissive mode to accept messages from WS bridge
                let gossipsub_config = gossipsub::ConfigBuilder::default()
                    .heartbeat_interval(Duration::from_secs(10))
                    .validation_mode(gossipsub::ValidationMode::Permissive)
                    .message_id_fn(|msg| {
                        // Use content hash + timestamp for message ID (allows same content to be sent multiple times)
                        let mut hasher = std::collections::hash_map::DefaultHasher::new();
                        std::hash::Hash::hash(&msg.data, &mut hasher);
                        std::hash::Hash::hash(&msg.source, &mut hasher);
                        // Include sequence_number if available for uniqueness
                        std::hash::Hash::hash(&msg.sequence_number, &mut hasher);
                        gossipsub::MessageId::from(
                            std::hash::Hasher::finish(&hasher).to_string(),
                        )
                    })
                    .build()
                    .expect("Valid gossipsub config");

                let gossipsub = gossipsub::Behaviour::new(
                    MessageAuthenticity::Signed(keypair.clone()),
                    gossipsub_config,
                )
                .expect("Valid gossipsub behaviour");

                // Request-response for direct messaging
                let request_response = request_response::cbor::Behaviour::new(
                    [(StreamProtocol::new("/riscv-relay/msg/1.0.0"), ProtocolSupport::Full)],
                    request_response::Config::default(),
                );

                // AutoNAT configuration
                let autonat = autonat::Behaviour::new(
                    keypair.public().to_peer_id(),
                    autonat::Config {
                        only_global_ips: false,
                        ..Default::default()
                    },
                );

                RelayServerBehaviour {
                    relay: relay::Behaviour::new(keypair.public().to_peer_id(), relay_config),
                    dcutr: dcutr::Behaviour::new(keypair.public().to_peer_id()),
                    identify,
                    ping: ping::Behaviour::new(ping::Config::new()),
                    kademlia,
                    autonat,
                    gossipsub,
                    request_response,
                }
            })?
            .with_swarm_config(|cfg| {
                cfg.with_idle_connection_timeout(Duration::from_secs(60))
            })
            .build();

        Ok(Self {
            swarm,
            connected_peers: HashSet::new(),
            peer_addresses: HashMap::new(),
            reservations: HashSet::new(),
            topics: HashSet::new(),
            ws_to_gossip_rx,
            gossip_to_ws_tx,
        })
    }

    /// Get the local peer ID
    fn local_peer_id(&self) -> PeerId {
        *self.swarm.local_peer_id()
    }

    /// Start listening on configured addresses
    fn start_listening(&mut self, args: &Args) -> Result<()> {
        // Listen on TCP
        let tcp_addr: Multiaddr = format!("/ip4/{}/tcp/{}", args.bind, args.tcp_port).parse()?;
        self.swarm.listen_on(tcp_addr.clone())?;
        info!("Listening on TCP: {}", tcp_addr);

        // Listen on QUIC
        let quic_addr: Multiaddr = format!(
            "/ip4/{}/udp/{}/quic-v1",
            args.bind,
            args.port
        )
        .parse()?;
        self.swarm.listen_on(quic_addr.clone())?;
        info!("Listening on QUIC: {}", quic_addr);

        // Add external address if specified
        if let Some(ref external) = args.external_addr {
            let external_addr: Multiaddr = external.parse()?;
            self.swarm.add_external_address(external_addr.clone());
            info!("Announced external address: {}", external_addr);
        }

        Ok(())
    }

    /// Subscribe to a gossipsub topic
    fn subscribe(&mut self, topic: &str) -> Result<()> {
        let topic = IdentTopic::new(topic);
        self.swarm.behaviour_mut().gossipsub.subscribe(&topic)?;
        self.topics.insert(topic.to_string());
        info!("Subscribed to topic: {}", topic);
        Ok(())
    }

    /// Publish a message to a gossipsub topic
    fn publish(&mut self, topic: &str, data: Vec<u8>) -> Result<()> {
        let topic = IdentTopic::new(topic);
        self.swarm
            .behaviour_mut()
            .gossipsub
            .publish(topic, data)?;
        Ok(())
    }

    /// Run the relay server event loop
    async fn run(mut self) -> Result<()> {
        // Subscribe to default topics
        self.subscribe("riscv-vm")?;
        self.subscribe("relay-announce")?;

        let topic = IdentTopic::new("riscv-vm");

        loop {
            tokio::select! {
                // Handle swarm events
                event = self.swarm.select_next_some() => {
                    match event {
                        SwarmEvent::NewListenAddr { address, .. } => {
                            let peer_id = *self.swarm.local_peer_id();
                            let full_addr = format!("{}/p2p/{}", address, peer_id);
                            info!("New listening address: {}", full_addr);
                        }

                        SwarmEvent::ConnectionEstablished {
                            peer_id,
                            endpoint,
                            num_established,
                            ..
                        } => {
                            info!(
                                "Connection established with {} via {:?} (total: {})",
                                peer_id,
                                endpoint.get_remote_address(),
                                num_established
                            );
                            self.connected_peers.insert(peer_id);
                            self.peer_addresses
                                .entry(peer_id)
                                .or_default()
                                .push(endpoint.get_remote_address().clone());
                        }

                        SwarmEvent::ConnectionClosed {
                            peer_id,
                            num_established,
                            cause,
                            ..
                        } => {
                            if num_established == 0 {
                                info!("Connection closed with {} (cause: {:?})", peer_id, cause);
                                self.connected_peers.remove(&peer_id);
                                self.peer_addresses.remove(&peer_id);
                            }
                        }

                        SwarmEvent::Behaviour(event) => {
                            self.handle_behaviour_event(event).await;
                        }

                        SwarmEvent::IncomingConnection { local_addr, .. } => {
                            debug!("Incoming connection on {}", local_addr);
                        }

                        SwarmEvent::OutgoingConnectionError { peer_id, error, .. } => {
                            if let Some(peer) = peer_id {
                                warn!("Outgoing connection error to {}: {}", peer, error);
                            }
                        }

                        SwarmEvent::IncomingConnectionError { local_addr, error, .. } => {
                            warn!("Incoming connection error on {}: {}", local_addr, error);
                        }

                        event => {
                            debug!("Unhandled swarm event: {:?}", event);
                        }
                    }
                }

                // Handle messages from WebSocket clients to publish to gossipsub
                Some(data) = self.ws_to_gossip_rx.recv() => {
                    debug!("[WS Bridge] Publishing {} bytes from WebSocket to gossipsub", data.len());
                    if let Err(e) = self.swarm.behaviour_mut().gossipsub.publish(topic.clone(), data) {
                        warn!("[WS Bridge] Failed to publish to gossipsub: {}", e);
                    }
                }
            }
        }
    }

    /// Handle behaviour-specific events
    async fn handle_behaviour_event(&mut self, event: RelayServerBehaviourEvent) {
        match event {
            RelayServerBehaviourEvent::Relay(relay_event) => {
                self.handle_relay_event(relay_event);
            }

            RelayServerBehaviourEvent::Dcutr(dcutr_event) => {
                self.handle_dcutr_event(dcutr_event);
            }

            RelayServerBehaviourEvent::Identify(identify_event) => {
                self.handle_identify_event(identify_event);
            }

            RelayServerBehaviourEvent::Ping(ping_event) => {
                if let ping::Event { peer, result: Ok(rtt), .. } = ping_event {
                    debug!("Ping to {} succeeded: {:?}", peer, rtt);
                }
            }

            RelayServerBehaviourEvent::Kademlia(kad_event) => {
                self.handle_kademlia_event(kad_event);
            }

            RelayServerBehaviourEvent::Autonat(autonat_event) => {
                self.handle_autonat_event(autonat_event);
            }

            RelayServerBehaviourEvent::Gossipsub(gossipsub_event) => {
                self.handle_gossipsub_event(gossipsub_event);
            }

            RelayServerBehaviourEvent::RequestResponse(rr_event) => {
                self.handle_request_response_event(rr_event);
            }
        }
    }

    fn handle_relay_event(&mut self, event: relay::Event) {
        match event {
            relay::Event::ReservationReqAccepted { src_peer_id, .. } => {
                info!("Relay reservation accepted for {}", src_peer_id);
                self.reservations.insert(src_peer_id);
            }

            relay::Event::ReservationReqDenied { src_peer_id } => {
                warn!("Relay reservation denied for {}", src_peer_id);
            }

            relay::Event::ReservationTimedOut { src_peer_id } => {
                info!("Relay reservation timed out for {}", src_peer_id);
                self.reservations.remove(&src_peer_id);
            }

            relay::Event::CircuitReqAccepted { src_peer_id, dst_peer_id, .. } => {
                info!(
                    "Circuit established: {} -> {} (via relay)",
                    src_peer_id, dst_peer_id
                );
            }

            relay::Event::CircuitReqDenied { src_peer_id, dst_peer_id } => {
                warn!(
                    "Circuit request denied: {} -> {}",
                    src_peer_id, dst_peer_id
                );
            }

            relay::Event::CircuitClosed { src_peer_id, dst_peer_id, .. } => {
                debug!(
                    "Circuit closed: {} -> {}",
                    src_peer_id, dst_peer_id
                );
            }

            _ => {}
        }
    }

    fn handle_dcutr_event(&mut self, event: dcutr::Event) {
        // dcutr::Event is a struct with remote_peer_id and result
        let dcutr::Event { remote_peer_id, result } = event;
        match result {
            Ok(connection_id) => {
                info!(
                    "DCUtR: Direct connection established with {} (connection: {:?}, hole punch success!)",
                    remote_peer_id, connection_id
                );
            }
            Err(error) => {
                warn!(
                    "DCUtR: Failed to establish direct connection with {}: {:?}",
                    remote_peer_id, error
                );
            }
        }
    }

    fn handle_identify_event(&mut self, event: identify::Event) {
        match event {
            identify::Event::Received { peer_id, info, .. } => {
                debug!(
                    "Identified peer {}: {} {:?}",
                    peer_id, info.protocol_version, info.listen_addrs
                );

                // Add peer's addresses to Kademlia
                for addr in info.listen_addrs {
                    self.swarm
                        .behaviour_mut()
                        .kademlia
                        .add_address(&peer_id, addr);
                }
            }

            identify::Event::Sent { peer_id, .. } => {
                debug!("Sent identify info to {}", peer_id);
            }

            _ => {}
        }
    }

    fn handle_kademlia_event(&mut self, event: kad::Event) {
        match event {
            kad::Event::RoutingUpdated {
                peer, is_new_peer, ..
            } => {
                if is_new_peer {
                    debug!("Kademlia: Added new peer {} to routing table", peer);
                }
            }

            kad::Event::OutboundQueryProgressed { result, .. } => {
                if let kad::QueryResult::GetClosestPeers(Ok(ok)) = result {
                    debug!("Kademlia: Found {} closest peers", ok.peers.len());
                }
            }

            _ => {}
        }
    }

    fn handle_autonat_event(&mut self, event: autonat::Event) {
        match event {
            autonat::Event::StatusChanged { old, new } => {
                info!("AutoNAT status changed: {:?} -> {:?}", old, new);
                match new {
                    autonat::NatStatus::Public(addr) => {
                        info!("NAT Status: Public at {}", addr);
                    }
                    autonat::NatStatus::Private => {
                        info!("NAT Status: Private (behind NAT)");
                    }
                    autonat::NatStatus::Unknown => {
                        info!("NAT Status: Unknown");
                    }
                }
            }

            autonat::Event::InboundProbe(probe) => {
                debug!("AutoNAT inbound probe: {:?}", probe);
            }

            autonat::Event::OutboundProbe(probe) => {
                debug!("AutoNAT outbound probe: {:?}", probe);
            }
        }
    }

    fn handle_gossipsub_event(&mut self, event: gossipsub::Event) {
        match event {
            gossipsub::Event::Message {
                propagation_source,
                message_id,
                message,
            } => {
                debug!(
                    "Gossipsub message from {}: topic={}, id={}, {} bytes",
                    propagation_source,
                    message.topic,
                    message_id,
                    message.data.len()
                );

                // Forward to WebSocket clients (if it's on the riscv-vm topic)
                if message.topic.as_str() == "riscv-vm" {
                    debug!("[WS Bridge] Forwarding {} bytes from gossipsub to WebSocket clients", message.data.len());
                    // broadcast::send returns the number of receivers that got the message
                    let _ = self.gossip_to_ws_tx.send(message.data);
                }
            }

            gossipsub::Event::Subscribed { peer_id, topic } => {
                info!("Peer {} subscribed to {}", peer_id, topic);
            }

            gossipsub::Event::Unsubscribed { peer_id, topic } => {
                info!("Peer {} unsubscribed from {}", peer_id, topic);
            }

            _ => {}
        }
    }

    fn handle_request_response_event(
        &mut self,
        event: request_response::Event<RelayMessage, RelayMessage>,
    ) {
        match event {
            request_response::Event::Message { peer, message } => match message {
                request_response::Message::Request {
                    request, channel, ..
                } => {
                    info!("Request from {}: {:?}", peer, request);

                    // Handle the request
                    let response = match request {
                        RelayMessage::DiscoverPeers => RelayMessage::PeerList {
                            peers: self.connected_peers.iter().map(|p| p.to_string()).collect(),
                        },
                        RelayMessage::Broadcast { topic, data } => {
                            // Publish to gossipsub
                            if let Err(e) = self.publish(&topic, data) {
                                error!("Failed to publish: {}", e);
                            }
                            RelayMessage::Direct {
                                data: b"OK".to_vec(),
                            }
                        }
                        _ => RelayMessage::Direct {
                            data: b"OK".to_vec(),
                        },
                    };

                    // Send response
                    if let Err(e) = self
                        .swarm
                        .behaviour_mut()
                        .request_response
                        .send_response(channel, response)
                    {
                        error!("Failed to send response: {:?}", e);
                    }
                }

                request_response::Message::Response { response, .. } => {
                    debug!("Response from {}: {:?}", peer, response);
                }
            },

            request_response::Event::OutboundFailure { peer, error, .. } => {
                warn!("Outbound request to {} failed: {:?}", peer, error);
            }

            request_response::Event::InboundFailure { peer, error, .. } => {
                warn!("Inbound request from {} failed: {:?}", peer, error);
            }

            _ => {}
        }
    }
}

/// Virtual gateway configuration
const GATEWAY_IP: [u8; 4] = [10, 0, 2, 2];
const GATEWAY_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];

/// VM IP address (for NAT source tracking)
#[allow(dead_code)]
const VM_IP: [u8; 4] = [10, 0, 2, 15];

/// NAT session for tracking UDP connections
#[derive(Clone, Debug)]
struct NatUdpSession {
    /// Original source IP (VM's IP)
    src_ip: [u8; 4],
    /// Original source port
    src_port: u16,
    /// External destination IP
    dst_ip: [u8; 4],
    /// External destination port
    dst_port: u16,
    /// Original source MAC
    src_mac: [u8; 6],
    /// Creation time
    created: std::time::Instant,
}

/// NAT session for tracking ICMP ping requests
#[derive(Clone, Debug)]
#[allow(dead_code)]
struct NatIcmpSession {
    /// Original source IP (VM's IP)
    src_ip: [u8; 4],
    /// Original source MAC
    src_mac: [u8; 6],
    /// ICMP identifier
    ident: u16,
    /// ICMP sequence number
    seq: u16,
    /// External destination IP
    dst_ip: [u8; 4],
    /// Creation time
    created: std::time::Instant,
}

/// NAT Gateway state
struct NatGateway {
    /// UDP sessions indexed by (external_dst_ip, external_dst_port, src_port)
    udp_sessions: HashMap<(Ipv4Addr, u16, u16), NatUdpSession>,
    /// ICMP sessions indexed by (dst_ip, ident, seq)
    icmp_sessions: HashMap<(Ipv4Addr, u16, u16), NatIcmpSession>,
    /// UDP socket for external DNS/UDP traffic
    udp_socket: Option<Arc<UdpSocket>>,
    /// Channel to send NAT responses back to WS clients
    response_tx: broadcast::Sender<Vec<u8>>,
}

impl NatGateway {
    fn new(response_tx: broadcast::Sender<Vec<u8>>) -> Self {
        Self {
            udp_sessions: HashMap::new(),
            icmp_sessions: HashMap::new(),
            udp_socket: None,
            response_tx,
        }
    }

    /// Initialize the UDP socket for external traffic
    async fn init(&mut self) -> Result<()> {
        let socket = UdpSocket::bind("0.0.0.0:0").await?;
        info!("[NAT] UDP socket bound to {}", socket.local_addr()?);
        self.udp_socket = Some(Arc::new(socket));
        Ok(())
    }

    /// Clean up expired sessions (older than 30 seconds)
    fn cleanup_expired(&mut self) {
        let timeout = Duration::from_secs(30);
        let now = std::time::Instant::now();
        
        self.udp_sessions.retain(|_, session| {
            now.duration_since(session.created) < timeout
        });
        
        self.icmp_sessions.retain(|_, session| {
            now.duration_since(session.created) < timeout
        });
    }

    /// Check if an IP is external (not in 10.0.0.0/8 private range)
    fn is_external_ip(ip: &[u8; 4]) -> bool {
        // Internal: 10.x.x.x, 127.x.x.x
        ip[0] != 10 && ip[0] != 127
    }

    /// Process an outbound UDP packet and perform NAT
    async fn process_udp_outbound(&mut self, frame: &[u8]) -> Option<()> {
        if frame.len() < 42 {
            return None;
        }

        // Extract IP addresses
        let src_ip: [u8; 4] = frame[26..30].try_into().ok()?;
        let dst_ip: [u8; 4] = frame[30..34].try_into().ok()?;
        
        // Only NAT external traffic
        if !Self::is_external_ip(&dst_ip) {
            return None;
        }

        // Get IP header length
        let ihl = ((frame[14] & 0x0f) * 4) as usize;
        let udp_start = 14 + ihl;
        
        if frame.len() < udp_start + 8 {
            return None;
        }

        // Extract UDP ports
        let src_port = u16::from_be_bytes([frame[udp_start], frame[udp_start + 1]]);
        let dst_port = u16::from_be_bytes([frame[udp_start + 2], frame[udp_start + 3]]);
        let udp_len = u16::from_be_bytes([frame[udp_start + 4], frame[udp_start + 5]]) as usize;

        // Extract source MAC
        let src_mac: [u8; 6] = frame[6..12].try_into().ok()?;

        // Create NAT session
        let dst_addr = Ipv4Addr::new(dst_ip[0], dst_ip[1], dst_ip[2], dst_ip[3]);
        let session = NatUdpSession {
            src_ip,
            src_port,
            dst_ip,
            dst_port,
            src_mac,
            created: std::time::Instant::now(),
        };

        // Store session
        self.udp_sessions.insert((dst_addr, dst_port, src_port), session);

        // Extract UDP payload (skip UDP header)
        let payload_start = udp_start + 8;
        let payload_end = std::cmp::min(udp_start + udp_len, frame.len());
        
        if payload_start >= payload_end {
            return None;
        }

        let payload = &frame[payload_start..payload_end];

        // Send to external destination
        if let Some(ref socket) = self.udp_socket {
            let dest = SocketAddrV4::new(dst_addr, dst_port);
            match socket.send_to(payload, dest).await {
                Ok(n) => {
                    info!("[NAT] Forwarded {} bytes UDP to {} (VM port {})", n, dest, src_port);
                }
                Err(e) => {
                    warn!("[NAT] Failed to send UDP to {}: {}", dest, e);
                }
            }
        }

        Some(())
    }

    /// Generate an Ethernet+IP+UDP frame for a NAT response
    fn generate_udp_response(&self, session: &NatUdpSession, payload: &[u8]) -> Vec<u8> {
        let udp_len = 8 + payload.len();
        let ip_len = 20 + udp_len;
        let frame_len = 14 + ip_len;
        
        let mut frame = vec![0u8; frame_len];
        
        // Ethernet header
        frame[0..6].copy_from_slice(&session.src_mac);  // dst = VM's MAC
        frame[6..12].copy_from_slice(&GATEWAY_MAC);      // src = gateway MAC
        frame[12..14].copy_from_slice(&[0x08, 0x00]);   // ethertype = IPv4
        
        // IP header
        frame[14] = 0x45;  // version + IHL
        frame[15] = 0;      // TOS
        frame[16..18].copy_from_slice(&(ip_len as u16).to_be_bytes());
        frame[18..20].copy_from_slice(&[0x00, 0x00]);  // identification
        frame[20..22].copy_from_slice(&[0x40, 0x00]);  // flags (DF) + fragment
        frame[22] = 64;     // TTL
        frame[23] = 17;     // protocol = UDP
        frame[24..26].copy_from_slice(&[0x00, 0x00]);  // checksum (fill later)
        frame[26..30].copy_from_slice(&session.dst_ip);  // src IP = external server
        frame[30..34].copy_from_slice(&session.src_ip);  // dst IP = VM's IP
        
        // IP checksum
        let ip_checksum = compute_checksum(&frame[14..34]);
        frame[24] = (ip_checksum >> 8) as u8;
        frame[25] = (ip_checksum & 0xff) as u8;
        
        // UDP header
        let udp_start = 34;
        frame[udp_start..udp_start+2].copy_from_slice(&session.dst_port.to_be_bytes());  // src port = external
        frame[udp_start+2..udp_start+4].copy_from_slice(&session.src_port.to_be_bytes()); // dst port = VM's
        frame[udp_start+4..udp_start+6].copy_from_slice(&(udp_len as u16).to_be_bytes());
        frame[udp_start+6..udp_start+8].copy_from_slice(&[0x00, 0x00]);  // checksum (optional)
        
        // UDP payload
        frame[udp_start+8..].copy_from_slice(payload);
        
        frame
    }

    /// Process an outbound ICMP ping and perform NAT
    async fn process_icmp_outbound(&mut self, frame: &[u8]) -> Option<()> {
        if frame.len() < 42 {
            return None;
        }

        // Extract IP addresses
        let src_ip: [u8; 4] = frame[26..30].try_into().ok()?;
        let dst_ip: [u8; 4] = frame[30..34].try_into().ok()?;
        
        // Only NAT external traffic
        if !Self::is_external_ip(&dst_ip) {
            return None;
        }

        // Check ICMP type is echo request (8)
        if frame[34] != 8 {
            return None;
        }

        // Extract ICMP ident and seq
        let ident = u16::from_be_bytes([frame[38], frame[39]]);
        let seq = u16::from_be_bytes([frame[40], frame[41]]);
        
        // Extract source MAC
        let src_mac: [u8; 6] = frame[6..12].try_into().ok()?;

        let dst_addr = Ipv4Addr::new(dst_ip[0], dst_ip[1], dst_ip[2], dst_ip[3]);

        // Store ICMP session
        let session = NatIcmpSession {
            src_ip,
            src_mac,
            ident,
            seq,
            dst_ip,
            created: std::time::Instant::now(),
        };
        self.icmp_sessions.insert((dst_addr, ident, seq), session);

        info!("[NAT] ICMP echo request to {} (ident={}, seq={})", dst_addr, ident, seq);

        // We can't send raw ICMP without root, so we'll use a workaround:
        // Send a UDP packet to port 7 (echo) or use surge-ping crate
        // For now, let's simulate by sending the request to an external ping service
        // or we spawn a subprocess
        
        // Alternative: Use tokio-icmp or pnet if available
        // For now, we'll try to ping using system ping command in background
        let response_tx = self.response_tx.clone();
        let src_mac_clone = src_mac;
        let src_ip_clone = src_ip;
        
        tokio::spawn(async move {
            // Try to ping using external process
            let output = tokio::process::Command::new("ping")
                .args(["-c", "1", "-W", "3", &dst_addr.to_string()])
                .output()
                .await;
            
            match output {
                Ok(out) if out.status.success() => {
                    // Generate ICMP echo reply frame
                    let reply = Self::generate_icmp_reply_for_nat(
                        &src_mac_clone, &src_ip_clone, &dst_ip, ident, seq
                    );
                    let _ = response_tx.send(reply);
                    info!("[NAT] ICMP echo reply from {} (ident={}, seq={})", dst_addr, ident, seq);
                }
                Ok(out) => {
                    debug!("[NAT] Ping to {} failed: {:?}", dst_addr, out.status);
                }
                Err(e) => {
                    debug!("[NAT] Failed to execute ping to {}: {}", dst_addr, e);
                }
            }
        });

        Some(())
    }

    /// Generate ICMP echo reply frame for NAT response
    fn generate_icmp_reply_for_nat(
        dst_mac: &[u8; 6],
        dst_ip: &[u8; 4],
        src_ip: &[u8; 4],
        ident: u16,
        seq: u16,
    ) -> Vec<u8> {
        let icmp_data = b"RISCV_PING";  // Match kernel's ping data
        let icmp_len = 8 + icmp_data.len();
        let ip_len = 20 + icmp_len;
        let frame_len = 14 + ip_len;
        
        let mut frame = vec![0u8; frame_len];
        
        // Ethernet header
        frame[0..6].copy_from_slice(dst_mac);           // dst = VM's MAC
        frame[6..12].copy_from_slice(&GATEWAY_MAC);    // src = gateway MAC
        frame[12..14].copy_from_slice(&[0x08, 0x00]); // ethertype = IPv4
        
        // IP header
        frame[14] = 0x45;  // version + IHL
        frame[15] = 0;      // TOS
        frame[16..18].copy_from_slice(&(ip_len as u16).to_be_bytes());
        frame[18..20].copy_from_slice(&ident.to_be_bytes());  // identification
        frame[20..22].copy_from_slice(&[0x00, 0x00]);  // flags + fragment
        frame[22] = 64;     // TTL
        frame[23] = 1;      // protocol = ICMP
        frame[24..26].copy_from_slice(&[0x00, 0x00]);  // checksum (fill later)
        frame[26..30].copy_from_slice(src_ip);         // src IP = external server
        frame[30..34].copy_from_slice(dst_ip);         // dst IP = VM's IP
        
        // IP checksum
        let ip_checksum = compute_checksum(&frame[14..34]);
        frame[24] = (ip_checksum >> 8) as u8;
        frame[25] = (ip_checksum & 0xff) as u8;
        
        // ICMP header
        frame[34] = 0;      // type = echo reply
        frame[35] = 0;      // code
        frame[36..38].copy_from_slice(&[0x00, 0x00]);  // checksum (fill later)
        frame[38..40].copy_from_slice(&ident.to_be_bytes());
        frame[40..42].copy_from_slice(&seq.to_be_bytes());
        frame[42..].copy_from_slice(icmp_data);
        
        // ICMP checksum
        let icmp_checksum = compute_checksum(&frame[34..]);
        frame[36] = (icmp_checksum >> 8) as u8;
        frame[37] = (icmp_checksum & 0xff) as u8;
        
        frame
    }
}

/// Check if an Ethernet frame is an ARP request for the gateway IP
fn is_arp_request_for_gateway(frame: &[u8]) -> bool {
    if frame.len() < 42 {
        return false;
    }
    // Check ethertype is ARP (0x0806)
    if frame[12] != 0x08 || frame[13] != 0x06 {
        return false;
    }
    // Check ARP operation is request (1)
    if frame[20] != 0x00 || frame[21] != 0x01 {
        return false;
    }
    // Check target protocol address is gateway IP
    frame[38..42] == GATEWAY_IP
}

/// Generate an ARP reply for the gateway
fn generate_arp_reply(request: &[u8]) -> Vec<u8> {
    let mut reply = vec![0u8; 42];
    
    // Ethernet header
    reply[0..6].copy_from_slice(&request[6..12]); // dst = sender's MAC
    reply[6..12].copy_from_slice(&GATEWAY_MAC);    // src = gateway MAC
    reply[12..14].copy_from_slice(&[0x08, 0x06]); // ethertype = ARP
    
    // ARP header
    reply[14..16].copy_from_slice(&[0x00, 0x01]); // hardware type = ethernet
    reply[16..18].copy_from_slice(&[0x08, 0x00]); // protocol type = IPv4
    reply[18] = 6;                                 // hardware addr len
    reply[19] = 4;                                 // protocol addr len
    reply[20..22].copy_from_slice(&[0x00, 0x02]); // operation = reply
    reply[22..28].copy_from_slice(&GATEWAY_MAC);   // sender hardware addr = gateway MAC
    reply[28..32].copy_from_slice(&GATEWAY_IP);    // sender protocol addr = gateway IP
    reply[32..38].copy_from_slice(&request[22..28]); // target hardware addr = requestor's MAC
    reply[38..42].copy_from_slice(&request[28..32]); // target protocol addr = requestor's IP
    
    reply
}

/// Check if a frame is an ICMP echo request (ping) to the gateway
fn is_icmp_echo_request_to_gateway(frame: &[u8]) -> bool {
    if frame.len() < 42 {
        return false;
    }
    // Check ethertype is IPv4 (0x0800)
    if frame[12] != 0x08 || frame[13] != 0x00 {
        return false;
    }
    // Check IP protocol is ICMP (1)
    if frame[23] != 1 {
        return false;
    }
    // Check destination IP is gateway
    if frame[30..34] != GATEWAY_IP {
        return false;
    }
    // Check ICMP type is echo request (8)
    frame[34] == 8
}

/// Compute Internet checksum (one's complement sum)
fn compute_checksum(data: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    let mut i = 0;
    while i + 1 < data.len() {
        sum += u16::from_be_bytes([data[i], data[i + 1]]) as u32;
        i += 2;
    }
    if i < data.len() {
        sum += (data[i] as u32) << 8;
    }
    while sum > 0xFFFF {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

/// Generate an ICMP echo reply from the gateway
fn generate_icmp_reply(request: &[u8]) -> Vec<u8> {
    let mut reply = request.to_vec();
    
    // Swap Ethernet addresses
    reply[0..6].copy_from_slice(&request[6..12]); // dst = sender's MAC
    reply[6..12].copy_from_slice(&GATEWAY_MAC);    // src = gateway MAC
    
    // Swap IP addresses
    let orig_src_ip: [u8; 4] = request[26..30].try_into().unwrap();
    let orig_dst_ip: [u8; 4] = request[30..34].try_into().unwrap();
    reply[26..30].copy_from_slice(&orig_dst_ip);
    reply[30..34].copy_from_slice(&orig_src_ip);
    
    // Recalculate IP header checksum
    reply[24] = 0;
    reply[25] = 0;
    let ip_checksum = compute_checksum(&reply[14..34]);
    reply[24] = (ip_checksum >> 8) as u8;
    reply[25] = (ip_checksum & 0xff) as u8;
    
    // Change ICMP type to echo reply (0)
    reply[34] = 0;
    
    // Recalculate ICMP checksum
    reply[36] = 0;
    reply[37] = 0;
    let icmp_data = &reply[34..];
    let checksum = compute_checksum(icmp_data);
    reply[36] = (checksum >> 8) as u8;
    reply[37] = (checksum & 0xff) as u8;
    
    reply
}

/// Check if a frame is an IPv4 packet destined for an external IP
fn is_external_ipv4_packet(frame: &[u8]) -> bool {
    if frame.len() < 34 {
        return false;
    }
    // Check ethertype is IPv4 (0x0800)
    if frame[12] != 0x08 || frame[13] != 0x00 {
        return false;
    }
    // Check destination IP is not internal (not 10.x.x.x or 127.x.x.x)
    let dst_ip = &frame[30..34];
    dst_ip[0] != 10 && dst_ip[0] != 127
}

/// Check if a frame is a UDP packet
fn is_udp_packet(frame: &[u8]) -> bool {
    if frame.len() < 34 {
        return false;
    }
    // Check ethertype is IPv4
    if frame[12] != 0x08 || frame[13] != 0x00 {
        return false;
    }
    // Check IP protocol is UDP (17)
    frame[23] == 17
}

/// Check if a frame is an ICMP packet
fn is_icmp_packet(frame: &[u8]) -> bool {
    if frame.len() < 34 {
        return false;
    }
    // Check ethertype is IPv4
    if frame[12] != 0x08 || frame[13] != 0x00 {
        return false;
    }
    // Check IP protocol is ICMP (1)
    frame[23] == 1
}

/// Handle a single WebSocket client connection
async fn handle_ws_client(
    stream: TcpStream,
    addr: SocketAddr,
    ws_to_gossip_tx: mpsc::Sender<Vec<u8>>,
    mut gossip_to_ws_rx: broadcast::Receiver<Vec<u8>>,
    nat_gateway: Arc<TokioMutex<NatGateway>>,
    mut nat_response_rx: broadcast::Receiver<Vec<u8>>,
) {
    info!("[WS Bridge] New WebSocket connection from {}", addr);

    let ws_stream = match accept_async(stream).await {
        Ok(ws) => ws,
        Err(e) => {
            error!("[WS Bridge] Failed to accept WebSocket from {}: {}", addr, e);
            return;
        }
    };

    let (ws_sender, mut ws_receiver) = ws_stream.split();

    // Use Arc<Mutex> to share ws_sender between tasks
    let ws_sender = Arc::new(tokio::sync::Mutex::new(ws_sender));
    let ws_sender_clone = ws_sender.clone();
    let ws_sender_nat = ws_sender.clone();

    // Spawn task to forward gossipsub messages to this WebSocket client
    let addr_clone = addr;
    let forward_task = tokio::spawn(async move {
        loop {
            match gossip_to_ws_rx.recv().await {
                Ok(data) => {
                    let mut sender = ws_sender_clone.lock().await;
                    if let Err(e) = sender.send(Message::Binary(data.into())).await {
                        debug!("[WS Bridge] Failed to send to {}: {}", addr_clone, e);
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => {
                    break;
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!("[WS Bridge] Client {} lagged by {} messages", addr_clone, n);
                }
            }
        }
    });

    // Spawn task to forward NAT responses to this WebSocket client
    let addr_nat = addr;
    let nat_forward_task = tokio::spawn(async move {
        loop {
            match nat_response_rx.recv().await {
                Ok(data) => {
                    debug!("[NAT] Forwarding {} byte response to {}", data.len(), addr_nat);
                    let mut sender = ws_sender_nat.lock().await;
                    if let Err(e) = sender.send(Message::Binary(data.into())).await {
                        debug!("[NAT] Failed to send NAT response to {}: {}", addr_nat, e);
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => {
                    break;
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!("[NAT] Client {} lagged by {} NAT responses", addr_nat, n);
                }
            }
        }
    });

    // Handle incoming messages from WebSocket client
    while let Some(msg) = ws_receiver.next().await {
        match msg {
            Ok(Message::Binary(data)) => {
                debug!("[WS Bridge] Received {} bytes from {}", data.len(), addr);
                
                // Check if this is an ARP request for gateway - respond locally
                if is_arp_request_for_gateway(&data) {
                    info!("[WS Bridge] Responding to ARP request for gateway from {}", addr);
                    let reply = generate_arp_reply(&data);
                    let mut sender = ws_sender.lock().await;
                    if let Err(e) = sender.send(Message::Binary(reply.into())).await {
                        warn!("[WS Bridge] Failed to send ARP reply to {}: {}", addr, e);
                    }
                    continue;
                }
                
                // Check if this is an ICMP ping to gateway - respond locally
                if is_icmp_echo_request_to_gateway(&data) {
                    info!("[WS Bridge] Responding to ICMP ping to gateway from {}", addr);
                    let reply = generate_icmp_reply(&data);
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    let mut sender = ws_sender.lock().await;
                    if let Err(e) = sender.send(Message::Binary(reply.into())).await {
                        warn!("[WS Bridge] Failed to send ICMP reply to {}: {}", addr, e);
                    }
                    continue;
                }
                
                // Check if this is external traffic that needs NAT
                if is_external_ipv4_packet(&data) {
                    if is_icmp_packet(&data) {
                        // External ICMP - route through NAT
                        let mut nat = nat_gateway.lock().await;
                        if nat.process_icmp_outbound(&data).await.is_some() {
                            continue; // NAT handled it
                        }
                    } else if is_udp_packet(&data) {
                        // External UDP (likely DNS) - route through NAT
                        let mut nat = nat_gateway.lock().await;
                        if nat.process_udp_outbound(&data).await.is_some() {
                            continue; // NAT handled it
                        }
                    }
                }
                
                // Forward other packets to gossipsub (internal VM-to-VM traffic)
                if let Err(e) = ws_to_gossip_tx.send(data.into()).await {
                    error!("[WS Bridge] Failed to forward to gossipsub: {}", e);
                    break;
                }
            }
            Ok(Message::Close(_)) => {
                info!("[WS Bridge] Client {} disconnected", addr);
                break;
            }
            Ok(Message::Ping(data)) => {
                debug!("[WS Bridge] Ping from {}", addr);
                let _ = data;
            }
            Ok(_) => {}
            Err(e) => {
                warn!("[WS Bridge] Error from {}: {}", addr, e);
                break;
            }
        }
    }

    forward_task.abort();
    nat_forward_task.abort();
    info!("[WS Bridge] Connection closed for {}", addr);
}

/// Start the WebSocket bridge server with NAT gateway
async fn start_ws_server(
    bind: String,
    ws_port: u16,
    ws_to_gossip_tx: mpsc::Sender<Vec<u8>>,
    gossip_to_ws_tx: broadcast::Sender<Vec<u8>>,
    nat_gateway: Arc<TokioMutex<NatGateway>>,
    nat_response_tx: broadcast::Sender<Vec<u8>>,
) -> Result<()> {
    let addr = format!("{}:{}", bind, ws_port);
    let listener = TcpListener::bind(&addr).await?;
    info!("[WS Bridge] WebSocket server listening on ws://{}", addr);

    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                let tx = ws_to_gossip_tx.clone();
                let rx = gossip_to_ws_tx.subscribe();
                let nat = nat_gateway.clone();
                let nat_rx = nat_response_tx.subscribe();
                tokio::spawn(handle_ws_client(stream, addr, tx, rx, nat, nat_rx));
            }
            Err(e) => {
                error!("[WS Bridge] Failed to accept connection: {}", e);
            }
        }
    }
}

/// Run the NAT UDP response receiver loop
async fn run_nat_udp_receiver(
    nat_gateway: Arc<TokioMutex<NatGateway>>,
    nat_response_tx: broadcast::Sender<Vec<u8>>,
) {
    // Wait for NAT gateway to be initialized with socket
    loop {
        let socket = {
            let nat = nat_gateway.lock().await;
            nat.udp_socket.clone()
        };
        
        if let Some(socket) = socket {
            info!("[NAT] UDP receiver started");
            let mut buf = [0u8; 2048];
            
            loop {
                match socket.recv_from(&mut buf).await {
                    Ok((n, src_addr)) => {
                        debug!("[NAT] Received {} bytes from {}", n, src_addr);
                        
                        // Find matching NAT session
                        let frame = {
                            let mut nat = nat_gateway.lock().await;
                            
                            // Clean up expired sessions periodically
                            nat.cleanup_expired();
                            
                            // Look for matching UDP session
                            let src_ip = match src_addr.ip() {
                                std::net::IpAddr::V4(ip) => ip,
                                _ => continue,
                            };
                            let src_port = src_addr.port();
                            
                            // Find session by external port (and optionally IP)
                            // DNS servers may respond from different IPs (anycast)
                            // so we match primarily on the external port
                            let mut found_session = None;
                            for (_key, session) in &nat.udp_sessions {
                                // For well-known ports like DNS (53), match on port only
                                // This handles DNS anycast responses from different IPs
                                if session.dst_port == src_port {
                                    // Prefer exact IP match, but accept any for port 53 (DNS)
                                    let ip_match = session.dst_ip == src_ip.octets();
                                    let is_dns = src_port == 53;
                                    
                                    if ip_match || is_dns {
                                        found_session = Some(session.clone());
                                        break;
                                    }
                                }
                            }
                            
                            if let Some(session) = found_session {
                                info!("[NAT] UDP response from {} -> VM port {}", src_addr, session.src_port);
                                Some(nat.generate_udp_response(&session, &buf[..n]))
                            } else {
                                debug!("[NAT] No session found for UDP from {}", src_addr);
                                None
                            }
                        };
                        
                        if let Some(frame) = frame {
                            let _ = nat_response_tx.send(frame);
                        }
                    }
                    Err(e) => {
                        warn!("[NAT] UDP recv error: {}", e);
                    }
                }
            }
        }
        
        // Wait before checking again
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    info!("Starting libp2p QUIC Relay Server with NAT Gateway...");
    info!("QUIC port: {}", args.port);
    info!("TCP port: {}", args.tcp_port);
    info!("WebSocket port: {}", args.ws_port);

    // Create channels for WebSocket bridge
    let (ws_to_gossip_tx, ws_to_gossip_rx) = mpsc::channel::<Vec<u8>>(1024);
    let (gossip_to_ws_tx, _) = broadcast::channel::<Vec<u8>>(1024);

    // Create NAT response channel
    let (nat_response_tx, _) = broadcast::channel::<Vec<u8>>(1024);

    // Create NAT gateway
    let mut nat_gateway = NatGateway::new(nat_response_tx.clone());
    if let Err(e) = nat_gateway.init().await {
        warn!("[NAT] Failed to initialize NAT gateway: {}", e);
        warn!("[NAT] External network access (ping 8.8.8.8, DNS) will not work");
    }
    let nat_gateway = Arc::new(TokioMutex::new(nat_gateway));

    // Create and configure the relay server
    let mut server = RelayServer::new(&args, ws_to_gossip_rx, gossip_to_ws_tx.clone()).await?;
    server.start_listening(&args)?;

    // Get peer ID for connection strings
    let peer_id = server.local_peer_id();

    // Print connection info
    println!();
    println!("╔════════════════════════════════════════════════════════════════════════════════╗");
    println!("║               libp2p QUIC Relay Server + NAT Gateway for RISC-V VM             ║");
    println!("╠════════════════════════════════════════════════════════════════════════════════╣");
    println!("║  Peer ID: {}  ║", peer_id);
    println!("╠════════════════════════════════════════════════════════════════════════════════╣");
    println!("║  Connect via QUIC (recommended):                                               ║");
    println!("║    /ip4/127.0.0.1/udp/{}/quic-v1/p2p/{}", args.port, peer_id);
    println!("║                                                                                ║");
    println!("║  Connect via TCP:                                                              ║");
    println!("║    /ip4/127.0.0.1/tcp/{}/p2p/{}", args.tcp_port, peer_id);
    println!("║                                                                                ║");
    println!("║  Connect via WebSocket (browser):                                              ║");
    println!("║    ws://127.0.0.1:{}", args.ws_port);
    println!("╠════════════════════════════════════════════════════════════════════════════════╣");
    println!("║  Features: Circuit Relay v2 • DCUtR • Kademlia DHT • Gossipsub • AutoNAT       ║");
    println!("║            WebSocket Bridge • NAT Gateway (external ping/DNS)                  ║");
    println!("╚════════════════════════════════════════════════════════════════════════════════╝");
    println!();

    // Start NAT UDP receiver in background
    let nat_gateway_udp = nat_gateway.clone();
    let nat_response_tx_udp = nat_response_tx.clone();
    tokio::spawn(async move {
        run_nat_udp_receiver(nat_gateway_udp, nat_response_tx_udp).await;
    });

    // Start WebSocket server in background
    let ws_bind = args.bind.clone();
    let ws_port = args.ws_port;
    tokio::spawn(async move {
        if let Err(e) = start_ws_server(
            ws_bind, 
            ws_port, 
            ws_to_gossip_tx, 
            gossip_to_ws_tx,
            nat_gateway,
            nat_response_tx,
        ).await {
            error!("[WS Bridge] WebSocket server error: {}", e);
        }
    });

    // Run the relay server
    server.run().await?;

    Ok(())
}
