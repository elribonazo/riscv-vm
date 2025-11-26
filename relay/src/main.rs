//! WebSocket relay server with gateway/router functionality for RISC-V VM networking.
//!
//! This server acts as:
//! 1. A hub for VMs to exchange Ethernet frames (VM-to-VM networking)
//! 2. A gateway router that responds to ARP and handles ICMP
//! 3. A NAT gateway to route ICMP traffic to the real internet
//!
//! Usage:
//!   cargo run --release -- --port 8765
//!
//! Then connect VMs with:
//!   riscv-vm --kernel kernel --net-ws ws://localhost:8765
//!
//! Or in browser, enable "Network" before booting the custom_kernel.

use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use log::{debug, info, warn, error};
use socket2::{Domain, Protocol, Socket, Type};
use std::collections::HashMap;
use std::mem::MaybeUninit;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, RwLock};
use tokio_tungstenite::{accept_async, tungstenite::{Error as WsError, Message}};

#[derive(Parser, Debug)]
#[command(author, version, about = "WebSocket relay server with gateway for RISC-V VM networking")]
struct Args {
    /// Port to listen on
    #[arg(short, long, default_value_t = 8765)]
    port: u16,

    /// Bind address
    #[arg(short, long, default_value = "0.0.0.0")]
    bind: String,
    
    /// Gateway IP address (the router that VMs see)
    #[arg(long, default_value = "10.0.2.2")]
    gateway_ip: String,
    
    /// Enable verbose packet logging
    #[arg(short, long)]
    verbose: bool,
}

/// Gateway configuration
#[derive(Clone)]
struct GatewayConfig {
    /// Gateway IP (what VMs send to for external traffic)
    ip: [u8; 4],
    /// Gateway MAC (virtual MAC for the gateway)
    mac: [u8; 6],
    /// Subnet mask
    #[allow(dead_code)]
    netmask: [u8; 4],
    /// Enable verbose logging
    verbose: bool,
}

impl GatewayConfig {
    fn new(ip_str: &str, verbose: bool) -> Self {
        let parts: Vec<u8> = ip_str
            .split('.')
            .filter_map(|s| s.parse().ok())
            .collect();
        
        let ip = if parts.len() == 4 {
            [parts[0], parts[1], parts[2], parts[3]]
        } else {
            [10, 0, 2, 2] // Default gateway IP
        };
        
        Self {
            ip,
            // Virtual MAC for the gateway - locally administered, unicast
            mac: [0x52, 0x54, 0x00, 0xaa, 0xbb, 0xcc],
            netmask: [255, 255, 255, 0],
            verbose,
        }
    }
}

type ClientId = u64;
type Tx = mpsc::UnboundedSender<Message>;
type ClientMap = Arc<RwLock<HashMap<ClientId, ClientInfo>>>;

/// Information about a connected client
struct ClientInfo {
    tx: Tx,
    mac: Option<[u8; 6]>,
    #[allow(dead_code)]
    ip: Option<[u8; 4]>,
}

/// Pending ICMP request for NAT tracking
#[derive(Clone)]
struct PendingIcmp {
    client_id: ClientId,
    src_mac: [u8; 6],
    src_ip: [u8; 4],
    dst_ip: [u8; 4],
    identifier: u16,
    sequence: u16,
    timestamp: Instant,
}

type IcmpTracker = Arc<RwLock<HashMap<(u16, u16), PendingIcmp>>>; // (identifier, sequence) -> pending

/// Pending UDP request for NAT tracking
#[derive(Clone)]
struct PendingUdp {
    client_id: ClientId,
    src_mac: [u8; 6],
    src_ip: [u8; 4],
    src_port: u16,
    dst_ip: [u8; 4],
    dst_port: u16,
    timestamp: Instant,
}

// (dst_ip, dst_port, src_port) -> pending UDP info for routing responses back
type UdpTracker = Arc<RwLock<HashMap<([u8; 4], u16, u16), PendingUdp>>>;

/// UDP packet parser
struct UdpPacket<'a> {
    src_port: u16,
    dst_port: u16,
    #[allow(dead_code)]
    length: u16,
    #[allow(dead_code)]
    checksum: u16,
    payload: &'a [u8],
}

impl<'a> UdpPacket<'a> {
    fn parse(data: &'a [u8]) -> Option<Self> {
        if data.len() < 8 {
            return None;
        }
        
        let src_port = u16::from_be_bytes([data[0], data[1]]);
        let dst_port = u16::from_be_bytes([data[2], data[3]]);
        let length = u16::from_be_bytes([data[4], data[5]]);
        let checksum = u16::from_be_bytes([data[6], data[7]]);
        let payload = &data[8..];
        
        Some(Self {
            src_port,
            dst_port,
            length,
            checksum,
            payload,
        })
    }
    
    /// Build a UDP packet
    fn build(src_port: u16, dst_port: u16, payload: &[u8]) -> Vec<u8> {
        let length = 8 + payload.len() as u16;
        let mut packet = Vec::with_capacity(8 + payload.len());
        packet.extend_from_slice(&src_port.to_be_bytes());
        packet.extend_from_slice(&dst_port.to_be_bytes());
        packet.extend_from_slice(&length.to_be_bytes());
        packet.extend_from_slice(&0u16.to_be_bytes()); // Checksum (0 = not computed)
        packet.extend_from_slice(payload);
        packet
    }
}

/// Ethernet frame parser/builder
struct EthernetFrame<'a> {
    dst_mac: [u8; 6],
    src_mac: [u8; 6],
    ethertype: u16,
    payload: &'a [u8],
}

impl<'a> EthernetFrame<'a> {
    fn parse(data: &'a [u8]) -> Option<Self> {
        if data.len() < 14 {
            return None;
        }
        
        let mut dst_mac = [0u8; 6];
        let mut src_mac = [0u8; 6];
        dst_mac.copy_from_slice(&data[0..6]);
        src_mac.copy_from_slice(&data[6..12]);
        let ethertype = u16::from_be_bytes([data[12], data[13]]);
        let payload = &data[14..];
        
        Some(Self { dst_mac, src_mac, ethertype, payload })
    }
    
    fn build(dst_mac: [u8; 6], src_mac: [u8; 6], ethertype: u16, payload: &[u8]) -> Vec<u8> {
        let mut frame = Vec::with_capacity(14 + payload.len());
        frame.extend_from_slice(&dst_mac);
        frame.extend_from_slice(&src_mac);
        frame.extend_from_slice(&ethertype.to_be_bytes());
        frame.extend_from_slice(payload);
        frame
    }
}

/// ARP packet parser/builder
struct ArpPacket {
    #[allow(dead_code)]
    hardware_type: u16,
    #[allow(dead_code)]
    protocol_type: u16,
    #[allow(dead_code)]
    hw_addr_len: u8,
    #[allow(dead_code)]
    proto_addr_len: u8,
    operation: u16,
    sender_hw_addr: [u8; 6],
    sender_proto_addr: [u8; 4],
    target_hw_addr: [u8; 6],
    target_proto_addr: [u8; 4],
}

impl ArpPacket {
    fn parse(data: &[u8]) -> Option<Self> {
        if data.len() < 28 {
            return None;
        }
        
        let hardware_type = u16::from_be_bytes([data[0], data[1]]);
        let protocol_type = u16::from_be_bytes([data[2], data[3]]);
        let hw_addr_len = data[4];
        let proto_addr_len = data[5];
        let operation = u16::from_be_bytes([data[6], data[7]]);
        
        let mut sender_hw_addr = [0u8; 6];
        let mut sender_proto_addr = [0u8; 4];
        let mut target_hw_addr = [0u8; 6];
        let mut target_proto_addr = [0u8; 4];
        
        sender_hw_addr.copy_from_slice(&data[8..14]);
        sender_proto_addr.copy_from_slice(&data[14..18]);
        target_hw_addr.copy_from_slice(&data[18..24]);
        target_proto_addr.copy_from_slice(&data[24..28]);
        
        Some(Self {
            hardware_type,
            protocol_type,
            hw_addr_len,
            proto_addr_len,
            operation,
            sender_hw_addr,
            sender_proto_addr,
            target_hw_addr,
            target_proto_addr,
        })
    }
    
    fn build_reply(request: &ArpPacket, responder_mac: [u8; 6]) -> Vec<u8> {
        let mut reply = vec![0u8; 28];
        
        // Hardware type: Ethernet (1)
        reply[0..2].copy_from_slice(&1u16.to_be_bytes());
        // Protocol type: IPv4 (0x0800)
        reply[2..4].copy_from_slice(&0x0800u16.to_be_bytes());
        // Hardware address length: 6
        reply[4] = 6;
        // Protocol address length: 4
        reply[5] = 4;
        // Operation: Reply (2)
        reply[6..8].copy_from_slice(&2u16.to_be_bytes());
        // Sender hardware address (our MAC)
        reply[8..14].copy_from_slice(&responder_mac);
        // Sender protocol address (the IP they asked about)
        reply[14..18].copy_from_slice(&request.target_proto_addr);
        // Target hardware address (requester's MAC)
        reply[18..24].copy_from_slice(&request.sender_hw_addr);
        // Target protocol address (requester's IP)
        reply[24..28].copy_from_slice(&request.sender_proto_addr);
        
        reply
    }
}

/// IPv4 packet header (minimal parsing)
struct Ipv4Header<'a> {
    version_ihl: u8,
    #[allow(dead_code)]
    tos: u8,
    #[allow(dead_code)]
    total_length: u16,
    identification: u16,
    #[allow(dead_code)]
    flags_fragment: u16,
    ttl: u8,
    protocol: u8,
    #[allow(dead_code)]
    checksum: u16,
    src_ip: [u8; 4],
    dst_ip: [u8; 4],
    payload: &'a [u8],
}

impl<'a> Ipv4Header<'a> {
    fn parse(data: &'a [u8]) -> Option<Self> {
        if data.len() < 20 {
            return None;
        }
        
        let version_ihl = data[0];
        let ihl = (version_ihl & 0x0F) as usize * 4;
        if ihl < 20 || data.len() < ihl {
            return None;
        }
        
        let total_length = u16::from_be_bytes([data[2], data[3]]);
        let identification = u16::from_be_bytes([data[4], data[5]]);
        let flags_fragment = u16::from_be_bytes([data[6], data[7]]);
        
        let mut src_ip = [0u8; 4];
        let mut dst_ip = [0u8; 4];
        src_ip.copy_from_slice(&data[12..16]);
        dst_ip.copy_from_slice(&data[16..20]);
        
        Some(Self {
            version_ihl,
            tos: data[1],
            total_length,
            identification,
            flags_fragment,
            ttl: data[8],
            protocol: data[9],
            checksum: u16::from_be_bytes([data[10], data[11]]),
            src_ip,
            dst_ip,
            payload: &data[ihl..],
        })
    }
}

/// ICMP packet
struct IcmpPacket<'a> {
    icmp_type: u8,
    code: u8,
    #[allow(dead_code)]
    checksum: u16,
    identifier: u16,
    sequence: u16,
    payload: &'a [u8],
}

impl<'a> IcmpPacket<'a> {
    fn parse(data: &'a [u8]) -> Option<Self> {
        if data.len() < 8 {
            return None;
        }
        
        Some(Self {
            icmp_type: data[0],
            code: data[1],
            checksum: u16::from_be_bytes([data[2], data[3]]),
            identifier: u16::from_be_bytes([data[4], data[5]]),
            sequence: u16::from_be_bytes([data[6], data[7]]),
            payload: &data[8..],
        })
    }
    
    /// Build an ICMP echo reply
    fn build_echo_reply(identifier: u16, sequence: u16, payload: &[u8]) -> Vec<u8> {
        let mut packet = Vec::with_capacity(8 + payload.len());
        packet.push(0); // Type: Echo Reply
        packet.push(0); // Code: 0
        packet.push(0); // Checksum placeholder
        packet.push(0);
        packet.extend_from_slice(&identifier.to_be_bytes());
        packet.extend_from_slice(&sequence.to_be_bytes());
        packet.extend_from_slice(payload);
        
        // Calculate checksum
        let checksum = Self::calculate_checksum(&packet);
        packet[2..4].copy_from_slice(&checksum.to_be_bytes());
        
        packet
    }
    
    /// Build an ICMP echo request
    fn build_echo_request(identifier: u16, sequence: u16, payload: &[u8]) -> Vec<u8> {
        let mut packet = Vec::with_capacity(8 + payload.len());
        packet.push(8); // Type: Echo Request
        packet.push(0); // Code: 0
        packet.push(0); // Checksum placeholder
        packet.push(0);
        packet.extend_from_slice(&identifier.to_be_bytes());
        packet.extend_from_slice(&sequence.to_be_bytes());
        packet.extend_from_slice(payload);
        
        // Calculate checksum
        let checksum = Self::calculate_checksum(&packet);
        packet[2..4].copy_from_slice(&checksum.to_be_bytes());
        
        packet
    }
    
    fn calculate_checksum(data: &[u8]) -> u16 {
        let mut sum: u32 = 0;
        let mut i = 0;
        while i < data.len() {
            let word = if i + 1 < data.len() {
                u16::from_be_bytes([data[i], data[i + 1]])
            } else {
                u16::from_be_bytes([data[i], 0])
            };
            sum += word as u32;
            i += 2;
        }
        while sum > 0xFFFF {
            sum = (sum & 0xFFFF) + (sum >> 16);
        }
        !sum as u16
    }
}

/// Build an IPv4 packet
fn build_ipv4_packet(src_ip: [u8; 4], dst_ip: [u8; 4], protocol: u8, ttl: u8, identification: u16, payload: &[u8]) -> Vec<u8> {
    let total_length = 20 + payload.len() as u16;
    let mut packet = vec![0u8; 20 + payload.len()];
    
    packet[0] = 0x45; // Version 4, IHL 5
    packet[1] = 0; // TOS
    packet[2..4].copy_from_slice(&total_length.to_be_bytes());
    packet[4..6].copy_from_slice(&identification.to_be_bytes());
    packet[6..8].copy_from_slice(&0u16.to_be_bytes()); // Flags + Fragment Offset
    packet[8] = ttl;
    packet[9] = protocol;
    // Checksum placeholder (bytes 10-11)
    packet[12..16].copy_from_slice(&src_ip);
    packet[16..20].copy_from_slice(&dst_ip);
    packet[20..].copy_from_slice(payload);
    
    // Calculate header checksum
    let checksum = IcmpPacket::calculate_checksum(&packet[0..20]);
    packet[10..12].copy_from_slice(&checksum.to_be_bytes());
    
    packet
}

/// Check if IP is in the local subnet (10.0.2.x)
fn is_local_subnet(ip: &[u8; 4]) -> bool {
    ip[0] == 10 && ip[1] == 0 && ip[2] == 2
}

/// ICMP socket for sending real pings
struct IcmpSocket {
    socket: Socket,
}

impl IcmpSocket {
    fn new() -> std::io::Result<Self> {
        // Create ICMP socket - on macOS SOCK_DGRAM works without root
        // On Linux, you may need CAP_NET_RAW or to enable ping_group_range
        let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::ICMPV4))?;
        socket.set_nonblocking(true)?;
        socket.set_read_timeout(Some(Duration::from_millis(100)))?;
        Ok(Self { socket })
    }
    
    #[allow(dead_code)]
    fn get_ref(&self) -> &Socket {
        &self.socket
    }
    
    fn send_ping(&self, dst_ip: [u8; 4], identifier: u16, sequence: u16, payload: &[u8]) -> std::io::Result<()> {
        let icmp_packet = IcmpPacket::build_echo_request(identifier, sequence, payload);
        let addr = SocketAddrV4::new(Ipv4Addr::new(dst_ip[0], dst_ip[1], dst_ip[2], dst_ip[3]), 0);
        info!("[NAT] Sending ICMP packet: {} bytes to {:?}, ident={}, seq={}", 
              icmp_packet.len(), addr, identifier, sequence);
        let bytes_sent = self.socket.send_to(&icmp_packet, &addr.into())?;
        info!("[NAT] Sent {} bytes", bytes_sent);
        Ok(())
    }
    
    fn recv_reply(&self) -> std::io::Result<Option<(Ipv4Addr, u16, u16, Vec<u8>)>> {
        let mut buf: [MaybeUninit<u8>; 1500] = unsafe { MaybeUninit::uninit().assume_init() };
        match self.socket.recv_from(&mut buf) {
            Ok((len, addr)) => {
                debug!("[NAT] Received {} bytes from {:?}", len, addr);
                
                // Convert MaybeUninit buffer to initialized slice
                let buf: &[u8] = unsafe { 
                    std::slice::from_raw_parts(buf.as_ptr() as *const u8, len)
                };
                
                // On macOS, SOCK_DGRAM + IPPROTO_ICMP returns IP header + ICMP
                // Check if first byte looks like IP header (0x45 = IPv4, IHL=5)
                let icmp_offset = if len > 20 && (buf[0] >> 4) == 4 {
                    // IP header present, calculate IHL (header length in 32-bit words)
                    let ihl = (buf[0] & 0x0F) as usize * 4;
                    debug!("[NAT] IP header detected, IHL={}", ihl);
                    ihl
                } else {
                    // No IP header, ICMP starts at offset 0
                    0
                };
                
                if len < icmp_offset + 8 {
                    debug!("[NAT] Packet too short for ICMP, ignoring");
                    return Ok(None);
                }
                
                let src_ip = match addr.as_socket_ipv4() {
                    Some(a) => *a.ip(),
                    None => {
                        debug!("[NAT] Not an IPv4 address, ignoring");
                        return Ok(None);
                    }
                };
                
                // Parse ICMP from correct offset
                let icmp = &buf[icmp_offset..];
                let icmp_type = icmp[0];
                let icmp_code = icmp[1];
                debug!("[NAT] ICMP type={}, code={}", icmp_type, icmp_code);
                
                if icmp_type != 0 { // Not echo reply
                    debug!("[NAT] Not an echo reply (type {}), ignoring", icmp_type);
                    return Ok(None);
                }
                
                let identifier = u16::from_be_bytes([icmp[4], icmp[5]]);
                let sequence = u16::from_be_bytes([icmp[6], icmp[7]]);
                let payload = icmp[8..].to_vec();
                
                info!("[NAT] Got echo reply from {}: ident={}, seq={}", src_ip, identifier, sequence);
                Ok(Some((src_ip, identifier, sequence, payload)))
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => Ok(None),
            Err(e) => {
                warn!("[NAT] recv_from error: {}", e);
                Err(e)
            }
        }
    }
}

/// UDP socket for NAT forwarding
struct UdpNatSocket {
    socket: std::net::UdpSocket,
}

impl UdpNatSocket {
    fn new() -> std::io::Result<Self> {
        // Bind to any available port
        let socket = std::net::UdpSocket::bind("0.0.0.0:0")?;
        socket.set_nonblocking(true)?;
        socket.set_read_timeout(Some(Duration::from_millis(100)))?;
        info!("[UDP-NAT] UDP socket bound to {:?}", socket.local_addr());
        Ok(Self { socket })
    }
    
    fn send_to(&self, data: &[u8], dst_ip: [u8; 4], dst_port: u16) -> std::io::Result<usize> {
        let addr = SocketAddrV4::new(
            Ipv4Addr::new(dst_ip[0], dst_ip[1], dst_ip[2], dst_ip[3]),
            dst_port,
        );
        self.socket.send_to(data, addr)
    }
    
    fn recv_from(&self) -> std::io::Result<Option<(Vec<u8>, [u8; 4], u16)>> {
        let mut buf = [0u8; 1500];
        match self.socket.recv_from(&mut buf) {
            Ok((len, addr)) => {
                if let SocketAddr::V4(addr_v4) = addr {
                    let ip = addr_v4.ip().octets();
                    let port = addr_v4.port();
                    Ok(Some((buf[..len].to_vec(), ip, port)))
                } else {
                    Ok(None)
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// NAT request types
enum NatRequest {
    Icmp {
        src_mac: [u8; 6],
        src_ip: [u8; 4],
        dst_ip: [u8; 4],
        identifier: u16,
        sequence: u16,
        payload: Vec<u8>,
    },
    Udp {
        src_mac: [u8; 6],
        src_ip: [u8; 4],
        src_port: u16,
        dst_ip: [u8; 4],
        dst_port: u16,
        payload: Vec<u8>,
    },
}

/// Process an incoming frame and generate gateway responses if needed
/// Returns (reply_frame, nat_request) where nat_request indicates if the packet should be NAT'd
fn process_frame_as_gateway(
    frame_data: &[u8],
    config: &GatewayConfig,
) -> (Option<Vec<u8>>, Option<NatRequest>) {
    let frame = match EthernetFrame::parse(frame_data) {
        Some(f) => f,
        None => return (None, None),
    };
    
    match frame.ethertype {
        0x0806 => {
            // ARP
            if let Some(arp) = ArpPacket::parse(frame.payload) {
                // Check if this is an ARP request
                if arp.operation == 1 {
                    // For the gateway IP, respond with gateway MAC
                    if arp.target_proto_addr == config.ip {
                        if config.verbose {
                            info!("[Gateway] ARP request for gateway IP {:?}, responding", config.ip);
                        }
                        
                        let arp_reply = ArpPacket::build_reply(&arp, config.mac);
                        let reply_frame = EthernetFrame::build(
                            arp.sender_hw_addr,
                            config.mac,
                            0x0806,
                            &arp_reply,
                        );
                        
                        return (Some(reply_frame), None);
                    }
                    
                    // For ANY external IP, also respond with gateway MAC (we are the router!)
                    // This allows the VM to send packets to external IPs through us
                    if !is_local_subnet(&arp.target_proto_addr) {
                        if config.verbose {
                            info!("[Gateway] ARP request for external IP {:?}, responding with gateway MAC", 
                                  format_ip(&arp.target_proto_addr));
                        }
                        
                        let arp_reply = ArpPacket::build_reply(&arp, config.mac);
                        let reply_frame = EthernetFrame::build(
                            arp.sender_hw_addr,
                            config.mac,
                            0x0806,
                            &arp_reply,
                        );
                        
                        return (Some(reply_frame), None);
                    }
                }
            }
        }
        0x0800 => {
            // IPv4
            if let Some(ip) = Ipv4Header::parse(frame.payload) {
                // Check if this packet is destined for our gateway IP
                if ip.dst_ip == config.ip {
                    match ip.protocol {
                        1 => {
                            // ICMP to gateway
                            if let Some(icmp) = IcmpPacket::parse(ip.payload) {
                                if icmp.icmp_type == 8 {
                                    // Echo request (ping) to gateway itself
                                    if config.verbose {
                                        info!("[Gateway] ICMP echo request from {:?}, seq={}", 
                                              format_ip(&ip.src_ip), icmp.sequence);
                                    }
                                    
                                    let icmp_reply = IcmpPacket::build_echo_reply(
                                        icmp.identifier,
                                        icmp.sequence,
                                        icmp.payload,
                                    );
                                    
                                    let ip_reply = build_ipv4_packet(
                                        config.ip,
                                        ip.src_ip,
                                        1, // ICMP
                                        64,
                                        ip.identification,
                                        &icmp_reply,
                                    );
                                    
                                    let reply_frame = EthernetFrame::build(
                                        frame.src_mac,
                                        config.mac,
                                        0x0800,
                                        &ip_reply,
                                    );
                                    
                                    return (Some(reply_frame), None);
                                }
                            }
                        }
                        _ => {}
                    }
                } else if !is_local_subnet(&ip.dst_ip) {
                    // Packet destined for external IP - need NAT
                    match ip.protocol {
                        1 => {
                            // ICMP to external IP
                            if let Some(icmp) = IcmpPacket::parse(ip.payload) {
                                if icmp.icmp_type == 8 {
                                    // Echo request to external IP - NAT it!
                                    info!("[NAT] ICMP echo request to {:?} seq={}", 
                                          format_ip(&ip.dst_ip), icmp.sequence);
                                    
                                    return (None, Some(NatRequest::Icmp {
                                        src_mac: frame.src_mac,
                                        src_ip: ip.src_ip,
                                        dst_ip: ip.dst_ip,
                                        identifier: icmp.identifier,
                                        sequence: icmp.sequence,
                                        payload: icmp.payload.to_vec(),
                                    }));
                                }
                            }
                        }
                        17 => {
                            // UDP to external IP - NAT it!
                            if let Some(udp) = UdpPacket::parse(ip.payload) {
                                info!("[UDP-NAT] UDP packet to {:?}:{} from port {}", 
                                      format_ip(&ip.dst_ip), udp.dst_port, udp.src_port);
                                
                                return (None, Some(NatRequest::Udp {
                                    src_mac: frame.src_mac,
                                    src_ip: ip.src_ip,
                                    src_port: udp.src_port,
                                    dst_ip: ip.dst_ip,
                                    dst_port: udp.dst_port,
                                    payload: udp.payload.to_vec(),
                                }));
                            }
                        }
                        _ => {
                            if config.verbose {
                                debug!("[Gateway] External IP protocol {} (not NATted)", ip.protocol);
                            }
                        }
                    }
                }
            }
        }
        _ => {}
    }
    
    (None, None)
}

/// Check if a frame should be broadcast (e.g., ARP, broadcast MAC)
fn should_broadcast(frame_data: &[u8]) -> bool {
    if frame_data.len() < 14 {
        return false;
    }
    
    // Check if destination is broadcast MAC
    let dst = &frame_data[0..6];
    dst == [0xff, 0xff, 0xff, 0xff, 0xff, 0xff]
}

/// Get destination MAC from frame
fn get_dst_mac(frame_data: &[u8]) -> Option<[u8; 6]> {
    if frame_data.len() < 6 {
        return None;
    }
    let mut mac = [0u8; 6];
    mac.copy_from_slice(&frame_data[0..6]);
    Some(mac)
}

/// Get source MAC from frame
fn get_src_mac(frame_data: &[u8]) -> Option<[u8; 6]> {
    if frame_data.len() < 12 {
        return None;
    }
    let mut mac = [0u8; 6];
    mac.copy_from_slice(&frame_data[6..12]);
    Some(mac)
}

fn format_mac(mac: &[u8; 6]) -> String {
    format!("{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            mac[0], mac[1], mac[2], mac[3], mac[4], mac[5])
}

fn format_ip(ip: &[u8; 4]) -> String {
    format!("{}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3])
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    
    let args = Args::parse();
    let addr = format!("{}:{}", args.bind, args.port);
    
    let config = GatewayConfig::new(&args.gateway_ip, args.verbose);
    
    // Create ICMP socket for NAT
    let icmp_socket = match IcmpSocket::new() {
        Ok(s) => {
            info!("[NAT] ICMP socket created successfully - external ping support enabled");
            Some(Arc::new(std::sync::Mutex::new(s)))
        }
        Err(e) => {
            warn!("[NAT] Failed to create ICMP socket: {} - external ping will NOT work", e);
            warn!("[NAT] On Linux, you may need: sudo sysctl -w net.ipv4.ping_group_range=\"0 65535\"");
            None
        }
    };
    
    // Create UDP socket for NAT (DNS and other UDP traffic)
    let udp_socket = match UdpNatSocket::new() {
        Ok(s) => {
            info!("[UDP-NAT] UDP socket created successfully - DNS/UDP NAT enabled");
            Some(Arc::new(std::sync::Mutex::new(s)))
        }
        Err(e) => {
            warn!("[UDP-NAT] Failed to create UDP socket: {} - DNS/UDP will NOT work", e);
            None
        }
    };
    
    let listener = TcpListener::bind(&addr).await?;
    info!("╔══════════════════════════════════════════════════════════════════╗");
    info!("║           RISC-V VM Network Relay with NAT Gateway              ║");
    info!("╠══════════════════════════════════════════════════════════════════╣");
    info!("║  WebSocket URL:  ws://localhost:{}                            ║", args.port);
    info!("║  Gateway IP:     {}                                       ║", format_ip(&config.ip));
    info!("║  Gateway MAC:    {}                           ║", format_mac(&config.mac));
    info!("║  ICMP NAT:       {}                                          ║", if icmp_socket.is_some() { "YES" } else { "NO " });
    info!("║  UDP NAT:        {}                                          ║", if udp_socket.is_some() { "YES" } else { "NO " });
    info!("╠══════════════════════════════════════════════════════════════════╣");
    info!("║  Usage:                                                          ║");
    info!("║    Native:  riscv-vm --kernel kernel --net-ws ws://localhost:{} ║", args.port);
    info!("║    Browser: Enable 'Network' before booting, then use 'ping'     ║");
    info!("╠══════════════════════════════════════════════════════════════════╣");
    info!("║  The gateway responds to:                                        ║");
    info!("║    - ARP requests for {} and external IPs              ║", format_ip(&config.ip));
    info!("║    - ICMP echo (ping) to {} (gateway)                  ║", format_ip(&config.ip));
    info!("║    - NAT for ICMP to external IPs (e.g., ping 8.8.8.8)           ║");
    info!("║    - NAT for UDP/DNS (e.g., nslookup google.com)                 ║");
    info!("║    - Routes packets between connected VMs                        ║");
    info!("╚══════════════════════════════════════════════════════════════════╝");
    
    let clients: ClientMap = Arc::new(RwLock::new(HashMap::new()));
    let icmp_tracker: IcmpTracker = Arc::new(RwLock::new(HashMap::new()));
    let udp_tracker: UdpTracker = Arc::new(RwLock::new(HashMap::new()));
    let mut next_client_id: ClientId = 0;
    
    // Spawn ICMP reply receiver task
    if let Some(ref socket) = icmp_socket {
        let socket_clone = socket.clone();
        let tracker_clone = icmp_tracker.clone();
        let clients_clone = clients.clone();
        let config_clone = config.clone();
        
        tokio::spawn(async move {
            loop {
                // Poll for ICMP replies
                let reply = {
                    let socket = socket_clone.lock().unwrap();
                    socket.recv_reply()
                };
                
                match reply {
                    Ok(Some((src_ip, identifier, sequence, payload))) => {
                        // Look up the pending request
                        let pending = {
                            let mut tracker = tracker_clone.write().await;
                            tracker.remove(&(identifier, sequence))
                        };
                        
                        if let Some(pending) = pending {
                            info!("[NAT] ICMP reply from {} seq={} -> forwarding to VM", src_ip, sequence);
                            
                            // Build reply packet to send back to VM
                            let src_ip_bytes: [u8; 4] = src_ip.octets();
                            
                            // Build ICMP echo reply
                            let icmp_reply = IcmpPacket::build_echo_reply(identifier, sequence, &payload);
                            
                            // Build IP packet (from external IP to VM's IP)
                            let ip_reply = build_ipv4_packet(
                                src_ip_bytes,
                                pending.src_ip,
                                1, // ICMP
                                64,
                                0,
                                &icmp_reply,
                            );
                            
                            // Build Ethernet frame
                            let reply_frame = EthernetFrame::build(
                                pending.src_mac,
                                config_clone.mac,
                                0x0800,
                                &ip_reply,
                            );
                            
                            // Send to the VM
                            let clients_guard = clients_clone.read().await;
                            if let Some(client) = clients_guard.get(&pending.client_id) {
                                let _ = client.tx.send(Message::Binary(reply_frame));
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        if e.kind() != std::io::ErrorKind::WouldBlock {
                            debug!("[NAT] ICMP recv error: {}", e);
                        }
                    }
                }
                
                // Clean up old pending requests (older than 10 seconds)
                {
                    let mut tracker = tracker_clone.write().await;
                    let now = Instant::now();
                    tracker.retain(|_, v| now.duration_since(v.timestamp) < Duration::from_secs(10));
                }
                
                // Small delay to avoid busy-looping
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });
    }
    
    // Spawn UDP reply receiver task
    if let Some(ref socket) = udp_socket {
        let socket_clone = socket.clone();
        let tracker_clone = udp_tracker.clone();
        let clients_clone = clients.clone();
        let config_clone = config.clone();
        
        tokio::spawn(async move {
            loop {
                // Poll for UDP replies
                let reply = {
                    let socket = socket_clone.lock().unwrap();
                    socket.recv_from()
                };
                
                match reply {
                    Ok(Some((data, src_ip, src_port))) => {
                        // Look up the pending request using src_ip, src_port as dst info
                        // We track by (dst_ip, dst_port, vm_src_port)
                        let pending = {
                            let tracker = tracker_clone.read().await;
                            // Find matching entry - the reply comes from dst_ip:dst_port
                            let mut found = None;
                            for (key, entry) in tracker.iter() {
                                if key.0 == src_ip && key.1 == src_port {
                                    found = Some((key.clone(), entry.clone()));
                                    break;
                                }
                            }
                            found
                        };
                        
                        if let Some((key, pending)) = pending {
                            info!("[UDP-NAT] UDP reply from {}:{} ({} bytes) -> forwarding to VM port {}", 
                                  format_ip(&src_ip), src_port, data.len(), pending.src_port);
                            
                            // Build UDP packet (swap ports for reply)
                            let udp_reply = UdpPacket::build(src_port, pending.src_port, &data);
                            
                            // Build IP packet (from external IP to VM's IP)
                            let ip_reply = build_ipv4_packet(
                                src_ip,
                                pending.src_ip,
                                17, // UDP
                                64,
                                0,
                                &udp_reply,
                            );
                            
                            // Build Ethernet frame
                            let reply_frame = EthernetFrame::build(
                                pending.src_mac,
                                config_clone.mac,
                                0x0800,
                                &ip_reply,
                            );
                            
                            // Send to the VM
                            let clients_guard = clients_clone.read().await;
                            if let Some(client) = clients_guard.get(&pending.client_id) {
                                let _ = client.tx.send(Message::Binary(reply_frame));
                            }
                            
                            // Remove the tracking entry (DNS is single request/response)
                            {
                                let mut tracker = tracker_clone.write().await;
                                tracker.remove(&key);
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        if e.kind() != std::io::ErrorKind::WouldBlock {
                            debug!("[UDP-NAT] UDP recv error: {}", e);
                        }
                    }
                }
                
                // Clean up old pending requests (older than 30 seconds for UDP)
                {
                    let mut tracker = tracker_clone.write().await;
                    let now = Instant::now();
                    tracker.retain(|_, v| now.duration_since(v.timestamp) < Duration::from_secs(30));
                }
                
                // Small delay to avoid busy-looping
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });
    }
    
    while let Ok((stream, peer_addr)) = listener.accept().await {
        let client_id = next_client_id;
        next_client_id += 1;
        
        let clients = clients.clone();
        let config = config.clone();
        let icmp_socket = icmp_socket.clone();
        let icmp_tracker = icmp_tracker.clone();
        let udp_socket = udp_socket.clone();
        let udp_tracker = udp_tracker.clone();
        
        tokio::spawn(async move {
            if let Err(e) = handle_connection(
                stream, peer_addr, client_id, clients, config, 
                icmp_socket, icmp_tracker, udp_socket, udp_tracker
            ).await {
                // Filter out harmless errors from health checks/scanners (like "No Upgrade header")
                let is_common_protocol_error = if let Some(ws_err) = e.downcast_ref::<WsError>() {
                    match ws_err {
                        WsError::Protocol(ref proto_err) => {
                            let msg = proto_err.to_string();
                            msg.contains("No \"Upgrade: websocket\" header") || 
                            msg.contains("Connection header is missing")
                        },
                        _ => false,
                    }
                } else {
                    false
                };

                if is_common_protocol_error {
                    debug!("Ignored non-websocket connection from client {}: {}", client_id, e);
                } else {
                    error!("Connection error for client {}: {}", client_id, e);
                }
            }
        });
    }
    
    Ok(())
}

async fn handle_connection(
    stream: TcpStream,
    peer_addr: SocketAddr,
    client_id: ClientId,
    clients: ClientMap,
    config: GatewayConfig,
    icmp_socket: Option<Arc<std::sync::Mutex<IcmpSocket>>>,
    icmp_tracker: IcmpTracker,
    udp_socket: Option<Arc<std::sync::Mutex<UdpNatSocket>>>,
    udp_tracker: UdpTracker,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let ws_stream = accept_async(stream).await?;
    info!("Client {} connected from {}", client_id, peer_addr);
    
    let (mut ws_sender, mut ws_receiver) = ws_stream.split();
    let (tx, mut rx) = mpsc::unbounded_channel();
    
    // Register this client
    {
        let mut clients_guard = clients.write().await;
        clients_guard.insert(client_id, ClientInfo { tx, mac: None, ip: None });
        info!("Total clients: {}", clients_guard.len());
    }
    
    // Task to send messages to this client
    let send_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if ws_sender.send(msg).await.is_err() {
                break;
            }
        }
    });
    
    // Receive messages from this client
    while let Some(msg) = ws_receiver.next().await {
        match msg {
            Ok(Message::Binary(data)) => {
                // Log packet info
                if config.verbose {
                    if let (Some(src), Some(dst)) = (get_src_mac(&data), get_dst_mac(&data)) {
                        debug!("Client {} frame: {} -> {} ({} bytes)",
                               client_id, format_mac(&src), format_mac(&dst), data.len());
                    }
                }
                
                // Update client's MAC if we don't have it
                if let Some(src_mac) = get_src_mac(&data) {
                    let mut clients_guard = clients.write().await;
                    if let Some(client) = clients_guard.get_mut(&client_id) {
                        if client.mac.is_none() {
                            info!("Client {} MAC learned: {}", client_id, format_mac(&src_mac));
                            client.mac = Some(src_mac);
                        }
                    }
                }
                
                // Check if gateway should respond or NAT
                let (reply, nat_request) = process_frame_as_gateway(&data, &config);
                
                // Send gateway reply back to this client
                if let Some(reply_frame) = reply {
                    let clients_guard = clients.read().await;
                    if let Some(client) = clients_guard.get(&client_id) {
                        let _ = client.tx.send(Message::Binary(reply_frame));
                    }
                }
                
                // Handle NAT requests
                if let Some(nat_req) = nat_request {
                    match nat_req {
                        NatRequest::Icmp { src_mac, src_ip, dst_ip, identifier, sequence, payload } => {
                            if let Some(ref socket) = icmp_socket {
                                // Track this request
                                {
                                    let mut tracker = icmp_tracker.write().await;
                                    tracker.insert((identifier, sequence), PendingIcmp {
                                        client_id,
                                        src_mac,
                                        src_ip,
                                        dst_ip,
                                        identifier,
                                        sequence,
                                        timestamp: Instant::now(),
                                    });
                                }
                                
                                // Send the real ICMP ping
                                let socket = socket.lock().unwrap();
                                if let Err(e) = socket.send_ping(dst_ip, identifier, sequence, &payload) {
                                    warn!("[NAT] Failed to send ICMP: {}", e);
                                } else {
                                    debug!("[NAT] Sent ICMP echo request to {:?}", format_ip(&dst_ip));
                                }
                            }
                        }
                        NatRequest::Udp { src_mac, src_ip, src_port, dst_ip, dst_port, payload } => {
                            if let Some(ref socket) = udp_socket {
                                // Track this request using (dst_ip, dst_port, src_port) as key
                                {
                                    let mut tracker = udp_tracker.write().await;
                                    tracker.insert((dst_ip, dst_port, src_port), PendingUdp {
                                        client_id,
                                        src_mac,
                                        src_ip,
                                        src_port,
                                        dst_ip,
                                        dst_port,
                                        timestamp: Instant::now(),
                                    });
                                }
                                
                                // Send the real UDP packet
                                let socket = socket.lock().unwrap();
                                match socket.send_to(&payload, dst_ip, dst_port) {
                                    Ok(sent) => {
                                        debug!("[UDP-NAT] Sent {} bytes to {}:{}", sent, format_ip(&dst_ip), dst_port);
                                    }
                                    Err(e) => {
                                        warn!("[UDP-NAT] Failed to send UDP: {}", e);
                                    }
                                }
                            }
                        }
                    }
                }
                
                // Broadcast to other clients (or unicast if we know the destination)
                let clients_guard = clients.read().await;
                let dst_mac = get_dst_mac(&data);
                let is_broadcast = should_broadcast(&data);
                
                for (&other_id, other_client) in clients_guard.iter() {
                    if other_id == client_id {
                        continue;
                    }
                    
                    // Send if broadcast or if MAC matches
                    if is_broadcast {
                        let _ = other_client.tx.send(Message::Binary(data.clone()));
                    } else if let Some(ref dst) = dst_mac {
                        if Some(*dst) == other_client.mac {
                            let _ = other_client.tx.send(Message::Binary(data.clone()));
                        }
                    }
                }
            }
            Ok(Message::Close(_)) => {
                info!("Client {} requested close", client_id);
                break;
            }
            Ok(Message::Ping(_)) => {
                // Pong is handled automatically by tungstenite
            }
            Ok(_) => {
                // Ignore text and other message types
            }
            Err(e) => {
                warn!("Error receiving from client {}: {}", client_id, e);
                break;
            }
        }
    }
    
    // Unregister client
    {
        let mut clients_guard = clients.write().await;
        clients_guard.remove(&client_id);
        info!("Client {} disconnected. Total clients: {}", client_id, clients_guard.len());
    }
    
    send_task.abort();
    
    Ok(())
}
