//! libp2p network backend for connecting to the QUIC relay.
//!
//! This backend tunnels Ethernet frames over libp2p gossipsub,
//! enabling peer-to-peer networking with NAT traversal support.
//!
//! Includes:
//! - Virtual gateway that responds to ARP requests for 10.0.2.2
//! - NAT for external traffic (ICMP, UDP/DNS) to enable ping 8.8.8.8 and nslookup

use crate::net::NetworkBackend;
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use super::*;
    use futures::StreamExt;
    use libp2p::{
        gossipsub::{self, IdentTopic, MessageAuthenticity},
        identify,
        identity::Keypair,
        noise,
        ping,
        relay,
        swarm::{NetworkBehaviour, SwarmEvent},
        tcp, yamux, Multiaddr, PeerId, SwarmBuilder,
    };
    use std::collections::{HashMap, VecDeque};
    use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket as StdUdpSocket};
    use std::thread;
    use std::time::Duration;

    /// Gossipsub topic for VM Ethernet frames
    const VM_TOPIC: &str = "riscv-vm";

    /// Maximum number of packets to buffer
    const MAX_RX_QUEUE_SIZE: usize = 256;

    /// Virtual gateway configuration
    /// The gateway responds to ARP requests and ICMP pings, simulating a router
    const GATEWAY_IP: [u8; 4] = [10, 0, 2, 2];
    const GATEWAY_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56]; // Virtual gateway MAC

    /// NAT session for tracking UDP connections
    #[derive(Clone, Debug)]
    struct NatUdpSession {
        src_ip: [u8; 4],
        src_port: u16,
        dst_ip: [u8; 4],
        dst_port: u16,
        src_mac: [u8; 6],
        created: std::time::Instant,
    }

    /// NAT gateway for external network access
    struct NatGateway {
        /// UDP socket for external traffic (DNS, etc.)
        udp_socket: Option<StdUdpSocket>,
        /// UDP sessions indexed by (external_dst_ip, external_dst_port, src_port)
        udp_sessions: HashMap<(Ipv4Addr, u16, u16), NatUdpSession>,
    }

    impl NatGateway {
        fn new() -> Self {
            // Try to bind UDP socket for NAT
            let udp_socket = match StdUdpSocket::bind("0.0.0.0:0") {
                Ok(socket) => {
                    // Set non-blocking mode
                    let _ = socket.set_nonblocking(true);
                    log::info!("[NAT] UDP socket bound to {:?}", socket.local_addr());
                    Some(socket)
                }
                Err(e) => {
                    log::warn!("[NAT] Failed to bind UDP socket: {}. External DNS won't work.", e);
                    None
                }
            };

            Self {
                udp_socket,
                udp_sessions: HashMap::new(),
            }
        }

        /// Check if an IP is external (not internal 10.x.x.x or 127.x.x.x)
        fn is_external_ip(ip: &[u8; 4]) -> bool {
            ip[0] != 10 && ip[0] != 127
        }

        /// Clean up expired sessions (older than 30 seconds)
        fn cleanup_expired(&mut self) {
            let timeout = Duration::from_secs(30);
            let now = std::time::Instant::now();
            self.udp_sessions.retain(|_, session| {
                now.duration_since(session.created) < timeout
            });
        }

        /// Process an outbound UDP packet and perform NAT
        fn process_udp_outbound(&mut self, frame: &[u8]) -> bool {
            if frame.len() < 42 {
                return false;
            }

            // Extract IP addresses
            let src_ip: [u8; 4] = match frame[26..30].try_into() {
                Ok(ip) => ip,
                Err(_) => return false,
            };
            let dst_ip: [u8; 4] = match frame[30..34].try_into() {
                Ok(ip) => ip,
                Err(_) => return false,
            };

            // Only NAT external traffic
            if !Self::is_external_ip(&dst_ip) {
                return false;
            }

            // Get IP header length
            let ihl = ((frame[14] & 0x0f) * 4) as usize;
            let udp_start = 14 + ihl;

            if frame.len() < udp_start + 8 {
                return false;
            }

            // Extract UDP ports
            let src_port = u16::from_be_bytes([frame[udp_start], frame[udp_start + 1]]);
            let dst_port = u16::from_be_bytes([frame[udp_start + 2], frame[udp_start + 3]]);
            let udp_len = u16::from_be_bytes([frame[udp_start + 4], frame[udp_start + 5]]) as usize;

            // Extract source MAC
            let src_mac: [u8; 6] = match frame[6..12].try_into() {
                Ok(mac) => mac,
                Err(_) => return false,
            };

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
                return false;
            }

            let payload = &frame[payload_start..payload_end];

            // Send to external destination
            if let Some(ref socket) = self.udp_socket {
                let dest = SocketAddrV4::new(dst_addr, dst_port);
                match socket.send_to(payload, dest) {
                    Ok(n) => {
                        log::info!("[NAT] Forwarded {} bytes UDP to {} (VM port {})", n, dest, src_port);
                        return true;
                    }
                    Err(e) => {
                        log::warn!("[NAT] Failed to send UDP to {}: {}", dest, e);
                    }
                }
            }

            false
        }

        /// Check for incoming UDP responses and generate reply frames
        fn check_udp_responses(&mut self, gateway_mac: &[u8; 6]) -> Option<Vec<u8>> {
            let socket = self.udp_socket.as_ref()?;
            let mut buf = [0u8; 2048];

            match socket.recv_from(&mut buf) {
                Ok((n, src_addr)) => {
                    log::debug!("[NAT] Received {} bytes from {}", n, src_addr);

                    // Clean up expired sessions
                    self.cleanup_expired();

                    // Look for matching UDP session
                    let src_ip = match src_addr.ip() {
                        std::net::IpAddr::V4(ip) => ip,
                        _ => return None,
                    };
                    let src_port = src_addr.port();

                    // Find session by external port (and optionally IP)
                    // DNS servers may respond from different IPs (anycast)
                    // so we match primarily on the external port
                    let session = self.udp_sessions.iter()
                        .find(|(_key, session)| {
                            if session.dst_port == src_port {
                                // Prefer exact IP match, but accept any for port 53 (DNS)
                                let ip_match = session.dst_ip == src_ip.octets();
                                let is_dns = src_port == 53;
                                ip_match || is_dns
                            } else {
                                false
                            }
                        })
                        .map(|(_, session)| session.clone())?;

                    log::info!("[NAT] UDP response from {} -> VM port {}", src_addr, session.src_port);
                    Some(self.generate_udp_response(&session, &buf[..n], gateway_mac))
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => None,
                Err(e) => {
                    log::debug!("[NAT] UDP recv error: {}", e);
                    None
                }
            }
        }

        /// Generate an Ethernet+IP+UDP frame for a NAT response
        fn generate_udp_response(&self, session: &NatUdpSession, payload: &[u8], gateway_mac: &[u8; 6]) -> Vec<u8> {
            let udp_len = 8 + payload.len();
            let ip_len = 20 + udp_len;
            let frame_len = 14 + ip_len;

            let mut frame = vec![0u8; frame_len];

            // Ethernet header
            frame[0..6].copy_from_slice(&session.src_mac);  // dst = VM's MAC
            frame[6..12].copy_from_slice(gateway_mac);       // src = gateway MAC
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

        /// Process external ICMP ping by spawning a system ping command
        fn process_icmp_outbound(&mut self, frame: &[u8], response_queue: &Arc<Mutex<VecDeque<PendingReply>>>) -> bool {
            if frame.len() < 42 {
                return false;
            }

            // Extract destination IP
            let dst_ip: [u8; 4] = match frame[30..34].try_into() {
                Ok(ip) => ip,
                Err(_) => return false,
            };

            // Only NAT external traffic
            if !Self::is_external_ip(&dst_ip) {
                return false;
            }

            // Check ICMP type is echo request (8)
            if frame[34] != 8 {
                return false;
            }

            // Extract source MAC
            let src_mac: [u8; 6] = match frame[6..12].try_into() {
                Ok(mac) => mac,
                Err(_) => return false,
            };

            // Extract source IP
            let src_ip: [u8; 4] = match frame[26..30].try_into() {
                Ok(ip) => ip,
                Err(_) => return false,
            };

            // Extract ICMP ident and seq
            let ident = u16::from_be_bytes([frame[38], frame[39]]);
            let seq = u16::from_be_bytes([frame[40], frame[41]]);

            let dst_addr = Ipv4Addr::new(dst_ip[0], dst_ip[1], dst_ip[2], dst_ip[3]);
            log::info!("[NAT] ICMP echo request to {} (ident={}, seq={})", dst_addr, ident, seq);

            // Spawn a thread to ping and queue the response
            let queue = response_queue.clone();
            std::thread::spawn(move || {
                // Try to ping using external process
                let output = std::process::Command::new("ping")
                    .args(["-c", "1", "-W", "3", &dst_addr.to_string()])
                    .output();

                match output {
                    Ok(out) if out.status.success() => {
                        // Generate ICMP echo reply frame
                        let reply = generate_icmp_reply_for_external(&src_mac, &src_ip, &dst_ip, ident, seq);
                        let pending = PendingReply {
                            data: reply,
                            deliver_at: std::time::Instant::now() + Duration::from_millis(10),
                        };
                        queue.lock().unwrap().push_back(pending);
                        log::info!("[NAT] ICMP echo reply from {} (ident={}, seq={})", dst_addr, ident, seq);
                    }
                    Ok(_out) => {
                        log::debug!("[NAT] Ping to {} failed (host unreachable)", dst_addr);
                    }
                    Err(e) => {
                        log::debug!("[NAT] Failed to execute ping to {}: {}", dst_addr, e);
                    }
                }
            });

            true
        }
    }

    /// Generate ICMP echo reply frame for external NAT response
    fn generate_icmp_reply_for_external(
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

    /// Check if a frame is an external UDP packet
    fn is_external_udp_packet(frame: &[u8]) -> bool {
        if frame.len() < 34 {
            return false;
        }
        // Check ethertype is IPv4
        if frame[12] != 0x08 || frame[13] != 0x00 {
            return false;
        }
        // Check IP protocol is UDP (17)
        if frame[23] != 17 {
            return false;
        }
        // Check destination IP is external
        let dst_ip: [u8; 4] = match frame[30..34].try_into() {
            Ok(ip) => ip,
            Err(_) => return false,
        };
        NatGateway::is_external_ip(&dst_ip)
    }

    /// Check if a frame is an external ICMP packet
    fn is_external_icmp_packet(frame: &[u8]) -> bool {
        if frame.len() < 34 {
            return false;
        }
        // Check ethertype is IPv4
        if frame[12] != 0x08 || frame[13] != 0x00 {
            return false;
        }
        // Check IP protocol is ICMP (1)
        if frame[23] != 1 {
            return false;
        }
        // Check destination IP is external
        let dst_ip: [u8; 4] = match frame[30..34].try_into() {
            Ok(ip) => ip,
            Err(_) => return false,
        };
        NatGateway::is_external_ip(&dst_ip)
    }

    /// Network behaviour for the VM client
    #[derive(NetworkBehaviour)]
    struct VmClientBehaviour {
        /// Gossipsub for pub/sub messaging (Ethernet frames)
        gossipsub: gossipsub::Behaviour,

        /// Circuit relay client for NAT traversal
        relay_client: relay::client::Behaviour,

        /// Identify protocol
        identify: identify::Behaviour,

        /// Ping for liveness
        ping: ping::Behaviour,
    }

    /// Commands sent to the libp2p event loop
    enum Command {
        Send(Vec<u8>),
        Shutdown,
    }

    /// Pending reply with delivery time
    struct PendingReply {
        data: Vec<u8>,
        deliver_at: std::time::Instant,
    }

    /// Check if an Ethernet frame is an ARP request for the gateway IP
    fn is_arp_request_for_gateway(frame: &[u8]) -> bool {
        // Minimum ARP frame: 14 (eth) + 28 (arp) = 42 bytes
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
        // Minimum: 14 (eth) + 20 (ip) + 8 (icmp) = 42 bytes
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

    /// Generate an ICMP echo reply from the gateway
    fn generate_icmp_reply(request: &[u8]) -> Vec<u8> {
        let mut reply = request.to_vec();
        
        // Swap Ethernet addresses
        reply[0..6].copy_from_slice(&request[6..12]); // dst = sender's MAC
        reply[6..12].copy_from_slice(&GATEWAY_MAC);    // src = gateway MAC
        
        // Swap IP addresses (src is at offset 26, dst at offset 30)
        let orig_src_ip: [u8; 4] = request[26..30].try_into().unwrap();
        let orig_dst_ip: [u8; 4] = request[30..34].try_into().unwrap();
        reply[26..30].copy_from_slice(&orig_dst_ip); // src IP = gateway (was dst)
        reply[30..34].copy_from_slice(&orig_src_ip); // dst IP = original sender
        
        // Recalculate IP header checksum (bytes 24-25 in frame = offset 10-11 in IP header)
        // First clear the old checksum
        reply[24] = 0;
        reply[25] = 0;
        // Calculate checksum over IP header (20 bytes starting at offset 14)
        let ip_checksum = compute_checksum(&reply[14..34]);
        reply[24] = (ip_checksum >> 8) as u8;
        reply[25] = (ip_checksum & 0xff) as u8;
        
        // Change ICMP type to echo reply (0)
        reply[34] = 0;
        
        // Recalculate ICMP checksum
        // Clear old checksum
        reply[36] = 0;
        reply[37] = 0;
        
        // Calculate new checksum over ICMP message
        let icmp_start = 34;
        let icmp_data = &reply[icmp_start..];
        let checksum = compute_checksum(icmp_data);
        reply[36] = (checksum >> 8) as u8;
        reply[37] = (checksum & 0xff) as u8;
        
        reply
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

    /// libp2p network backend for native platforms.
    pub struct Libp2pBackend {
        /// Relay multiaddress (e.g., /ip4/127.0.0.1/udp/4001/quic-v1/p2p/PEER_ID)
        relay_addr: String,
        /// MAC address for this VM
        mac: [u8; 6],
        /// Channel to send commands to the event loop
        cmd_tx: Option<Sender<Command>>,
        /// Channel to receive packets from the event loop
        rx_from_swarm: Option<Receiver<Vec<u8>>>,
        /// Connection state
        connected: Arc<Mutex<bool>>,
        /// Error message if any
        error_message: Arc<Mutex<Option<String>>>,
        /// Local peer ID
        local_peer_id: Arc<Mutex<Option<PeerId>>>,
        /// Queue for locally-generated gateway responses (ARP replies, ICMP replies)
        /// Uses Arc<Mutex> because `send` takes &self, not &mut self
        /// Contains PendingReply with delivery time to simulate network latency
        local_reply_queue: Arc<Mutex<VecDeque<PendingReply>>>,
        /// NAT gateway for external network access
        nat_gateway: Arc<Mutex<NatGateway>>,
    }

    impl Libp2pBackend {
        pub fn new(relay_addr: &str) -> Self {
            // Generate a random MAC based on relay address hash
            let mut mac = [0x52, 0x54, 0x00, 0x00, 0x00, 0x00];
            let hash: u32 = relay_addr
                .bytes()
                .fold(0u32, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u32));
            mac[3] = ((hash >> 16) & 0xff) as u8;
            mac[4] = ((hash >> 8) & 0xff) as u8;
            mac[5] = (hash & 0xff) as u8;

            Self {
                relay_addr: relay_addr.to_string(),
                mac,
                cmd_tx: None,
                rx_from_swarm: None,
                connected: Arc::new(Mutex::new(false)),
                error_message: Arc::new(Mutex::new(None)),
                local_peer_id: Arc::new(Mutex::new(None)),
                local_reply_queue: Arc::new(Mutex::new(VecDeque::new())),
                nat_gateway: Arc::new(Mutex::new(NatGateway::new())),
            }
        }

        /// Check if connected to the relay
        pub fn is_connected(&self) -> bool {
            *self.connected.lock().unwrap()
        }

        /// Get any error message
        pub fn error_message(&self) -> Option<String> {
            self.error_message.lock().unwrap().clone()
        }

        /// Get local peer ID
        pub fn local_peer_id(&self) -> Option<String> {
            self.local_peer_id.lock().unwrap().map(|p| p.to_string())
        }

        /// Run the libp2p event loop in a separate thread
        fn run_event_loop(
            relay_addr: String,
            cmd_rx: Receiver<Command>,
            packet_tx: Sender<Vec<u8>>,
            connected: Arc<Mutex<bool>>,
            error_message: Arc<Mutex<Option<String>>>,
            local_peer_id_store: Arc<Mutex<Option<PeerId>>>,
        ) {
            // Create a tokio runtime for this thread
            let rt = match tokio::runtime::Runtime::new() {
                Ok(rt) => rt,
                Err(e) => {
                    *error_message.lock().unwrap() = Some(format!("Failed to create runtime: {}", e));
                    return;
                }
            };

            rt.block_on(async move {
                // Generate keypair
                let local_key = Keypair::generate_ed25519();
                let local_peer_id = PeerId::from(local_key.public());
                *local_peer_id_store.lock().unwrap() = Some(local_peer_id);
                log::info!("[Libp2pBackend] Local peer ID: {}", local_peer_id);

                // Build the swarm step by step
                let tcp_builder = match SwarmBuilder::with_existing_identity(local_key.clone())
                    .with_tokio()
                    .with_tcp(
                        tcp::Config::default(),
                        noise::Config::new,
                        yamux::Config::default,
                    ) {
                    Ok(b) => b,
                    Err(e) => {
                        *error_message.lock().unwrap() = Some(format!("Failed to setup TCP: {}", e));
                        return;
                    }
                };

                let quic_builder = tcp_builder.with_quic();

                let relay_builder = match quic_builder.with_relay_client(noise::Config::new, yamux::Config::default) {
                    Ok(b) => b,
                    Err(e) => {
                        *error_message.lock().unwrap() = Some(format!("Failed to setup relay client: {}", e));
                        return;
                    }
                };

                let behaviour_builder = match relay_builder.with_behaviour(|keypair, relay_client| {
                    // Gossipsub configuration
                    let gossipsub_config = gossipsub::ConfigBuilder::default()
                        .heartbeat_interval(Duration::from_secs(10))
                        .validation_mode(gossipsub::ValidationMode::Permissive)
                        .message_id_fn(|msg| {
                            let mut hasher = std::collections::hash_map::DefaultHasher::new();
                            std::hash::Hash::hash(&msg.data, &mut hasher);
                            std::hash::Hash::hash(&msg.source, &mut hasher);
                            std::hash::Hash::hash(&std::time::Instant::now(), &mut hasher);
                            gossipsub::MessageId::from(std::hash::Hasher::finish(&hasher).to_string())
                        })
                        .build()
                        .expect("Valid gossipsub config");

                    let gossipsub = gossipsub::Behaviour::new(
                        MessageAuthenticity::Signed(keypair.clone()),
                        gossipsub_config,
                    )
                    .expect("Valid gossipsub behaviour");

                    // Identify
                    let identify = identify::Behaviour::new(identify::Config::new(
                        "/riscv-vm-client/1.0.0".to_string(),
                        keypair.public(),
                    ));

                    VmClientBehaviour {
                        gossipsub,
                        relay_client,
                        identify,
                        ping: ping::Behaviour::new(ping::Config::new()),
                    }
                }) {
                    Ok(b) => b,
                    Err(e) => {
                        *error_message.lock().unwrap() = Some(format!("Failed to setup behaviour: {}", e));
                        return;
                    }
                };

                let mut swarm = behaviour_builder
                    .with_swarm_config(|cfg| cfg.with_idle_connection_timeout(Duration::from_secs(60)))
                    .build();

                // Subscribe to VM topic
                let topic = IdentTopic::new(VM_TOPIC);
                if let Err(e) = swarm.behaviour_mut().gossipsub.subscribe(&topic) {
                    *error_message.lock().unwrap() = Some(format!("Failed to subscribe: {}", e));
                    return;
                }
                log::info!("[Libp2pBackend] Subscribed to topic: {}", VM_TOPIC);

                // Parse and dial relay address
                let relay_multiaddr: Multiaddr = match relay_addr.parse() {
                    Ok(addr) => addr,
                    Err(e) => {
                        *error_message.lock().unwrap() = Some(format!("Invalid relay address: {}", e));
                        return;
                    }
                };

                log::info!("[Libp2pBackend] Dialing relay: {}", relay_multiaddr);
                if let Err(e) = swarm.dial(relay_multiaddr.clone()) {
                    *error_message.lock().unwrap() = Some(format!("Failed to dial relay: {}", e));
                    return;
                }

                // Event loop
                let mut pending_packets: VecDeque<Vec<u8>> = VecDeque::new();

                loop {
                    // Check for commands (non-blocking)
                    match cmd_rx.try_recv() {
                        Ok(Command::Send(data)) => {
                            if *connected.lock().unwrap() {
                                // Publish to gossipsub
                                if let Err(e) = swarm.behaviour_mut().gossipsub.publish(topic.clone(), data) {
                                    log::warn!("[Libp2pBackend] Failed to publish: {}", e);
                                }
                            } else {
                                // Queue until connected
                                if pending_packets.len() < MAX_RX_QUEUE_SIZE {
                                    pending_packets.push_back(data);
                                }
                            }
                        }
                        Ok(Command::Shutdown) => {
                            log::info!("[Libp2pBackend] Shutting down");
                            break;
                        }
                        Err(TryRecvError::Disconnected) => {
                            log::info!("[Libp2pBackend] Command channel closed");
                            break;
                        }
                        Err(TryRecvError::Empty) => {}
                    }

                    // Process swarm events with timeout
                    tokio::select! {
                        event = swarm.select_next_some() => {
                            match event {
                                SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                                    log::info!("[Libp2pBackend] Connected to {}", peer_id);
                                    *connected.lock().unwrap() = true;
                                    *error_message.lock().unwrap() = None;

                                    // Flush pending packets
                                    while let Some(data) = pending_packets.pop_front() {
                                        if let Err(e) = swarm.behaviour_mut().gossipsub.publish(topic.clone(), data) {
                                            log::warn!("[Libp2pBackend] Failed to publish queued packet: {}", e);
                                        }
                                    }
                                }

                                SwarmEvent::ConnectionClosed { peer_id, cause, .. } => {
                                    log::info!("[Libp2pBackend] Disconnected from {}: {:?}", peer_id, cause);
                                    // Only mark as disconnected if we have no other connections
                                    if swarm.network_info().num_peers() == 0 {
                                        *connected.lock().unwrap() = false;
                                    }
                                }

                                SwarmEvent::Behaviour(VmClientBehaviourEvent::Gossipsub(
                                    gossipsub::Event::Message { message, propagation_source, .. }
                                )) => {
                                    // Don't process our own messages
                                    if message.source != Some(local_peer_id) {
                                        log::debug!(
                                            "[Libp2pBackend] Received {} bytes from {}",
                                            message.data.len(),
                                            propagation_source
                                        );
                                        let _ = packet_tx.send(message.data);
                                    }
                                }

                                SwarmEvent::Behaviour(VmClientBehaviourEvent::Gossipsub(
                                    gossipsub::Event::Subscribed { peer_id, topic }
                                )) => {
                                    log::info!("[Libp2pBackend] Peer {} subscribed to {}", peer_id, topic);
                                }

                                SwarmEvent::OutgoingConnectionError { peer_id, error, .. } => {
                                    if let Some(peer) = peer_id {
                                        log::warn!("[Libp2pBackend] Connection error to {}: {}", peer, error);
                                    }
                                    *error_message.lock().unwrap() = Some(format!("Connection error: {}", error));
                                }

                                _ => {}
                            }
                        }

                        _ = tokio::time::sleep(Duration::from_millis(10)) => {
                            // Timeout to allow checking commands
                        }
                    }
                }
            });
        }
    }

    impl NetworkBackend for Libp2pBackend {
        fn init(&mut self) -> Result<(), String> {
            log::info!("[Libp2pBackend] Connecting to {}", self.relay_addr);

            let (cmd_tx, cmd_rx) = channel();
            let (packet_tx, packet_rx) = channel();

            self.cmd_tx = Some(cmd_tx);
            self.rx_from_swarm = Some(packet_rx);

            let relay_addr = self.relay_addr.clone();
            let connected = self.connected.clone();
            let error_message = self.error_message.clone();
            let local_peer_id = self.local_peer_id.clone();

            // Spawn the event loop in a separate thread
            thread::spawn(move || {
                Self::run_event_loop(relay_addr, cmd_rx, packet_tx, connected, error_message, local_peer_id);
            });

            // Wait a bit for connection (non-blocking init)
            for _ in 0..50 {
                if *self.connected.lock().unwrap() {
                    log::info!("[Libp2pBackend] Connected successfully!");
                    return Ok(());
                }
                if let Some(ref err) = *self.error_message.lock().unwrap() {
                    return Err(err.clone());
                }
                std::thread::sleep(Duration::from_millis(100));
            }

            // Connection is async, so we return Ok even if not connected yet
            log::info!("[Libp2pBackend] Initialization started (connection pending)");
            Ok(())
        }

        fn recv(&mut self) -> Result<Option<Vec<u8>>, String> {
            // First, check for locally-generated gateway responses (ARP replies, ICMP replies)
            // Only return replies that are ready to be delivered (simulates network latency)
            {
                let mut queue = self.local_reply_queue.lock().unwrap();
                let now = std::time::Instant::now();
                // Check if the first pending reply is ready
                if let Some(pending) = queue.front() {
                    if now >= pending.deliver_at {
                        let reply = queue.pop_front().unwrap();
                        log::debug!("[Libp2pBackend] recv() returning {} bytes from local gateway reply", reply.data.len());
                        return Ok(Some(reply.data));
                    }
                }
            }
            
            // Second, check for NAT UDP responses from external servers
            {
                let mut nat = self.nat_gateway.lock().unwrap();
                if let Some(frame) = nat.check_udp_responses(&GATEWAY_MAC) {
                    log::debug!("[Libp2pBackend] recv() returning {} bytes from NAT UDP response", frame.len());
                    return Ok(Some(frame));
                }
            }
            
            // Then check for packets from the network (gossipsub)
            if let Some(ref rx) = self.rx_from_swarm {
                match rx.try_recv() {
                    Ok(data) => {
                        log::trace!("[Libp2pBackend] recv() returning {} bytes", data.len());
                        Ok(Some(data))
                    }
                    Err(TryRecvError::Empty) => Ok(None),
                    Err(TryRecvError::Disconnected) => Err("libp2p disconnected".to_string()),
                }
            } else {
                Ok(None)
            }
        }

        fn send(&self, buf: &[u8]) -> Result<(), String> {
            let now = std::time::Instant::now();
            
            // Check if this is an ARP request for the gateway - if so, generate a local reply
            if is_arp_request_for_gateway(buf) {
                let reply = generate_arp_reply(buf);
                log::info!("[Libp2pBackend] Intercepted ARP request for gateway, queueing ARP reply");
                // ARP replies are delivered quickly (1ms simulated latency)
                let pending = PendingReply {
                    data: reply,
                    deliver_at: now + Duration::from_millis(1),
                };
                self.local_reply_queue.lock().unwrap().push_back(pending);
                return Ok(());
            }
            
            // Check if this is an ICMP ping to the gateway - if so, generate a local reply
            if is_icmp_echo_request_to_gateway(buf) {
                let reply = generate_icmp_reply(buf);
                log::info!("[Libp2pBackend] Intercepted ICMP ping to gateway, queueing ICMP reply");
                let pending = PendingReply {
                    data: reply,
                    deliver_at: now + Duration::from_millis(10),
                };
                self.local_reply_queue.lock().unwrap().push_back(pending);
                return Ok(());
            }
            
            // Check if this is an external ICMP ping - route through NAT
            if is_external_icmp_packet(buf) {
                let mut nat = self.nat_gateway.lock().unwrap();
                if nat.process_icmp_outbound(buf, &self.local_reply_queue) {
                    // NAT handled the external ping
                    return Ok(());
                }
            }
            
            // Check if this is an external UDP packet (e.g., DNS) - route through NAT
            if is_external_udp_packet(buf) {
                let mut nat = self.nat_gateway.lock().unwrap();
                if nat.process_udp_outbound(buf) {
                    // NAT handled the external UDP
                    return Ok(());
                }
            }
            
            // For all other packets (internal VM-to-VM), send to the network via libp2p gossipsub
            if let Some(ref tx) = self.cmd_tx {
                tx.send(Command::Send(buf.to_vec()))
                    .map_err(|e| format!("Send failed: {}", e))
            } else {
                Err("libp2p not initialized".to_string())
            }
        }

        fn mac_address(&self) -> [u8; 6] {
            self.mac
        }
    }

    impl Drop for Libp2pBackend {
        fn drop(&mut self) {
            if let Some(ref tx) = self.cmd_tx {
                let _ = tx.send(Command::Shutdown);
            }
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub use native::Libp2pBackend;

// For WASM, we'll still use WebSocket since browser libp2p requires different setup
// The relay can bridge WebSocket clients to the libp2p network
#[cfg(target_arch = "wasm32")]
pub use crate::net_ws::WsBackend as Libp2pBackend;

