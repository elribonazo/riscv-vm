//! Network stack using smoltcp.
//!
//! This module provides the TCP/IP stack for the kernel using the smoltcp crate.

use crate::virtio_net::VirtioNet;
use alloc::vec;
use alloc::vec::Vec;
use alloc::collections::VecDeque;
use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet, SocketStorage};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::{icmp, udp};
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr, IpEndpoint, Ipv4Address};

/// Network configuration
/// Default IP address (used as fallback if no IP is assigned by relay)
pub const DEFAULT_IP_ADDR: Ipv4Address = Ipv4Address::new(10, 0, 2, 15);
pub const GATEWAY: Ipv4Address = Ipv4Address::new(10, 0, 2, 2);
pub const PREFIX_LEN: u8 = 24;

/// Dynamic IP address assigned by the relay/network controller
/// This is set during network initialization
pub static mut MY_IP_ADDR: Ipv4Address = Ipv4Address::new(10, 0, 2, 15);

/// Get the current IP address (safe wrapper)
pub fn get_my_ip() -> Ipv4Address {
    unsafe { MY_IP_ADDR }
}

/// DNS server (Google Public DNS)
pub const DNS_SERVER: Ipv4Address = Ipv4Address::new(8, 8, 8, 8);
/// DNS port
pub const DNS_PORT: u16 = 53;

/// Loopback address
pub const LOOPBACK: Ipv4Address = Ipv4Address::new(127, 0, 0, 1);

/// ICMP identifier for our ping socket
const ICMP_IDENT: u16 = 0x1234;

/// Local port for DNS queries
const DNS_LOCAL_PORT: u16 = 10053;

/// Pending loopback ping reply
struct LoopbackReply {
    from: Ipv4Address,
    ident: u16,
    seq: u16,
}

/// Static storage for sockets
static mut SOCKET_STORAGE: [SocketStorage<'static>; 4] = [SocketStorage::EMPTY; 4];

/// Static storage for ICMP buffers - need larger buffers for proper ICMP
static mut ICMP_RX_META: [icmp::PacketMetadata; 8] = [icmp::PacketMetadata::EMPTY; 8];
static mut ICMP_TX_META: [icmp::PacketMetadata; 8] = [icmp::PacketMetadata::EMPTY; 8];
static mut ICMP_RX_DATA: [u8; 512] = [0; 512];
static mut ICMP_TX_DATA: [u8; 512] = [0; 512];

/// Static storage for UDP buffers (for DNS queries)
static mut UDP_RX_META: [udp::PacketMetadata; 8] = [udp::PacketMetadata::EMPTY; 8];
static mut UDP_TX_META: [udp::PacketMetadata; 8] = [udp::PacketMetadata::EMPTY; 8];
static mut UDP_RX_DATA: [u8; 1024] = [0; 1024];
static mut UDP_TX_DATA: [u8; 1024] = [0; 1024];

/// Cached ARP entry
struct ArpCache {
    ip: [u8; 4],
    mac: [u8; 6],
}

/// Global network state
pub struct NetState {
    device: VirtioNet,
    iface: Interface,
    sockets: SocketSet<'static>,
    icmp_handle: SocketHandle,
    udp_handle: SocketHandle,
    arp_cache: Option<ArpCache>,
    /// Pending loopback ping replies (delivered on next poll)
    loopback_replies: VecDeque<LoopbackReply>,
}

impl NetState {
    /// Initialize the network stack
    /// Note: After storing this in a static, call finalize() to complete RX buffer setup!
    pub fn new(mut device: VirtioNet) -> Result<Self, &'static str> {
        // Initialize the VirtIO device (phase 1 - configures queues but doesn't populate RX)
        device.init()?;
        
        // --- IP ADDRESS DISCOVERY ---
        // Wait for an IP assignment from the host/relay
        // In WASM, async tasks need the JS event loop to run, so we use many
        // short iterations to give the browser more chances to process events
        let mut my_ip = DEFAULT_IP_ADDR; // Fallback default
        let mut got_ip = false;
        
        crate::uart::write_str("    \x1b[0;90m├─\x1b[0m Waiting for IP assignment");
        // 100 iterations with shorter delays = more chances for async tasks in WASM
        for i in 0..100 {
            if let Some(ip_bytes) = device.get_config_ip() {
                my_ip = Ipv4Address::from_bytes(&ip_bytes);
                got_ip = true;
                crate::uart::write_line(" \x1b[1;32m[OK]\x1b[0m");
                break;
            }
            // Print a dot every 5 iterations to show progress
            if i % 5 == 0 {
                crate::uart::write_str(".");
            }
            // Shorter delay to allow more frequent checks and JS event loop to run in WASM
            for _ in 0..200_000 { core::hint::spin_loop(); } 
        }
        
        // If we didn't get an IP, show fallback message
        if !got_ip {
            crate::uart::write_line(" \x1b[1;33m[using default]\x1b[0m");
        }
        
        // Save to global for other modules to use
        unsafe { MY_IP_ADDR = my_ip; }
        
        let mac = device.mac;
        let hw_addr = HardwareAddress::Ethernet(EthernetAddress(mac));
        
        // Create interface config
        let config = Config::new(hw_addr);
        
        // Create the interface
        let mut iface = Interface::new(config, &mut DeviceWrapper(&mut device), Instant::from_millis(0));
        
        // Configure IP address using the dynamic IP
        iface.update_ip_addrs(|addrs| {
            addrs.push(IpCidr::new(IpAddress::v4(my_ip.0[0], my_ip.0[1], my_ip.0[2], my_ip.0[3]), PREFIX_LEN)).ok();
        });
        
        // Set default gateway
        iface.routes_mut().add_default_ipv4_route(GATEWAY).ok();
        
        // Create socket set with static storage
        let sockets = unsafe {
            SocketSet::new(&mut SOCKET_STORAGE[..])
        };
        
        // Create ICMP socket for ping
        let icmp_rx_buffer = unsafe {
            icmp::PacketBuffer::new(&mut ICMP_RX_META[..], &mut ICMP_RX_DATA[..])
        };
        let icmp_tx_buffer = unsafe {
            icmp::PacketBuffer::new(&mut ICMP_TX_META[..], &mut ICMP_TX_DATA[..])
        };
        let mut icmp_socket = icmp::Socket::new(icmp_rx_buffer, icmp_tx_buffer);
        
        // Bind the ICMP socket to receive echo replies for our identifier
        icmp_socket.bind(icmp::Endpoint::Ident(ICMP_IDENT)).ok();
        
        // Create UDP socket for DNS queries
        let udp_rx_buffer = unsafe {
            udp::PacketBuffer::new(&mut UDP_RX_META[..], &mut UDP_RX_DATA[..])
        };
        let udp_tx_buffer = unsafe {
            udp::PacketBuffer::new(&mut UDP_TX_META[..], &mut UDP_TX_DATA[..])
        };
        let mut udp_socket = udp::Socket::new(udp_rx_buffer, udp_tx_buffer);
        
        // Bind UDP socket to local port for DNS
        udp_socket.bind(DNS_LOCAL_PORT).ok();
        
        let mut state = NetState {
            device,
            iface,
            sockets,
            icmp_handle: SocketHandle::default(),
            udp_handle: SocketHandle::default(),
            arp_cache: None,
            loopback_replies: VecDeque::new(),
        };
        
        state.icmp_handle = state.sockets.add(icmp_socket);
        state.udp_handle = state.sockets.add(udp_socket);
        
        Ok(state)
    }
    
    /// Finalize initialization (must be called AFTER the NetState is in its final memory location)
    pub fn finalize(&mut self) {
        self.device.finalize_init();
    }
    
    /// Poll the network stack (call frequently)
    pub fn poll(&mut self, timestamp_ms: i64) {
        let timestamp = Instant::from_millis(timestamp_ms);
        
        // Poll the device
        self.device.poll();
        
        // Poll the interface
        self.iface.poll(timestamp, &mut DeviceWrapper(&mut self.device), &mut self.sockets);
    }
    
    /// Send a raw ARP request
    fn send_arp_request(&mut self, target_ip: [u8; 4]) -> Result<(), &'static str> {
        let my_ip = get_my_ip();
        let mut frame = [0u8; 42]; // 14 (eth) + 28 (arp)
        
        // Ethernet header
        frame[0..6].copy_from_slice(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff]); // dst = broadcast
        frame[6..12].copy_from_slice(&self.device.mac); // src = our MAC
        frame[12..14].copy_from_slice(&[0x08, 0x06]); // ethertype = ARP
        
        // ARP header
        frame[14..16].copy_from_slice(&[0x00, 0x01]); // hardware type = ethernet
        frame[16..18].copy_from_slice(&[0x08, 0x00]); // protocol type = IPv4
        frame[18] = 6; // hardware addr len
        frame[19] = 4; // protocol addr len
        frame[20..22].copy_from_slice(&[0x00, 0x01]); // operation = request
        frame[22..28].copy_from_slice(&self.device.mac); // sender hardware addr
        frame[28..32].copy_from_slice(&my_ip.0); // sender protocol addr
        frame[32..38].copy_from_slice(&[0x00; 6]); // target hardware addr (unknown)
        frame[38..42].copy_from_slice(&target_ip); // target protocol addr
        
        self.device.send(&frame)
    }
    
    /// Compute ICMP checksum
    fn icmp_checksum(data: &[u8]) -> u16 {
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
    
    /// Resolve MAC address for an IP via ARP (with caching)
    fn resolve_mac(&mut self, target_ip: [u8; 4]) -> Option<[u8; 6]> {
        // Check cache first
        if let Some(ref cache) = self.arp_cache {
            if cache.ip == target_ip {
                return Some(cache.mac);
            }
        }
        
        // Try multiple times with increasing waits
        for attempt in 0..5 {
            // Send ARP request
            if attempt == 0 || attempt == 2 {
                // Resend ARP on first attempt and after some retries
                if self.send_arp_request(target_ip).is_err() {
                    continue;
                }
            }
            
            // Wait with increasing delay (100k, 200k, 400k, 800k, 1600k spins)
            let wait_cycles = 100_000 << attempt;
            for _ in 0..wait_cycles { 
                core::hint::spin_loop(); 
            }
            self.device.poll();
            
            // Check for ARP reply
            if let Some((desc_idx, data)) = self.device.recv_with_desc() {
                if data.len() >= 42 && data[12] == 0x08 && data[13] == 0x06 {
                    // ARP packet - extract sender MAC
                    let mut mac = [0u8; 6];
                    mac.copy_from_slice(&data[22..28]);
                    
                    // Recycle the buffer
                    self.device.recycle_rx(desc_idx);
                    
                    // Cache the result
                    self.arp_cache = Some(ArpCache { ip: target_ip, mac });
                    
                    return Some(mac);
                }
                // Not an ARP reply, recycle and keep trying
                self.device.recycle_rx(desc_idx);
            }
        }
        
        None
    }
    
    /// Check if an address is a loopback address (127.x.x.x)
    fn is_loopback(addr: &Ipv4Address) -> bool {
        addr.0[0] == 127
    }
    
    /// Check if an address is our own IP
    fn is_self(addr: &Ipv4Address) -> bool {
        let my_ip = get_my_ip();
        addr.0 == my_ip.0
    }
    
    /// Check if an address is on the local subnet (10.0.2.x/24)
    fn is_on_local_subnet(addr: &Ipv4Address) -> bool {
        let my_ip = get_my_ip();
        addr.0[0] == my_ip.0[0] && 
        addr.0[1] == my_ip.0[1] && 
        addr.0[2] == my_ip.0[2]
    }
    
    /// Send an ICMP echo request (ping) - directly via VirtIO or loopback
    pub fn send_ping(&mut self, target: Ipv4Address, seq: u16, _timestamp_ms: i64) -> Result<(), &'static str> {
        // Handle loopback addresses (127.x.x.x) and self-ping locally
        if Self::is_loopback(&target) || Self::is_self(&target) {
            // Queue an immediate reply for loopback
            self.loopback_replies.push_back(LoopbackReply {
                from: target,
                ident: ICMP_IDENT,
                seq,
            });
            return Ok(());
        }
        
        let target_bytes = target.0;
        
        // For external IPs (not on local subnet), route through gateway
        let next_hop = if Self::is_on_local_subnet(&target) {
            target_bytes
        } else {
            GATEWAY.0  // Use gateway for external destinations
        };
        
        // Resolve MAC address for the next hop (gateway or direct target)
        let dst_mac = self.resolve_mac(next_hop)
            .ok_or("Failed to resolve MAC address")?;
        
        // Build Ethernet + IP + ICMP packet
        let icmp_data = b"RISCV_PING";
        let icmp_len = 8 + icmp_data.len(); // ICMP header (8) + data
        let ip_len = 20 + icmp_len; // IP header (20) + ICMP
        let frame_len = 14 + ip_len; // Ethernet header (14) + IP
        
        let mut frame = vec![0u8; frame_len];
        
        // Ethernet header
        frame[0..6].copy_from_slice(&dst_mac); // dst MAC
        frame[6..12].copy_from_slice(&self.device.mac); // src MAC
        frame[12..14].copy_from_slice(&[0x08, 0x00]); // ethertype = IPv4
        
        // IP header
        let my_ip = get_my_ip();
        frame[14] = 0x45; // version + IHL
        frame[15] = 0; // TOS
        frame[16..18].copy_from_slice(&(ip_len as u16).to_be_bytes()); // total length
        frame[18..20].copy_from_slice(&ICMP_IDENT.to_be_bytes()); // identification
        frame[20..22].copy_from_slice(&[0x00, 0x00]); // flags + fragment
        frame[22] = 64; // TTL
        frame[23] = 1; // protocol = ICMP
        frame[24..26].copy_from_slice(&[0x00, 0x00]); // checksum (fill later)
        frame[26..30].copy_from_slice(&my_ip.0); // src IP
        frame[30..34].copy_from_slice(&target_bytes); // dst IP
        
        // IP checksum
        let ip_header = &frame[14..34];
        let mut sum: u32 = 0;
        for i in (0..20).step_by(2) {
            sum += u16::from_be_bytes([ip_header[i], ip_header[i + 1]]) as u32;
        }
        while sum > 0xFFFF {
            sum = (sum & 0xFFFF) + (sum >> 16);
        }
        let ip_cksum = !(sum as u16);
        frame[24..26].copy_from_slice(&ip_cksum.to_be_bytes());
        
        // ICMP header
        frame[34] = 8; // type = echo request
        frame[35] = 0; // code
        frame[36..38].copy_from_slice(&[0x00, 0x00]); // checksum (fill later)
        frame[38..40].copy_from_slice(&ICMP_IDENT.to_be_bytes()); // identifier
        frame[40..42].copy_from_slice(&seq.to_be_bytes()); // sequence
        frame[42..].copy_from_slice(icmp_data); // data
        
        // ICMP checksum
        let icmp_cksum = Self::icmp_checksum(&frame[34..]);
        frame[36..38].copy_from_slice(&icmp_cksum.to_be_bytes());
        
        // Send the ICMP request
        self.device.send(&frame)?;
        
        Ok(())
    }
    
    /// Get MAC address
    #[allow(dead_code)]
    pub fn mac(&self) -> [u8; 6] {
        self.device.mac
    }
    
    /// Get MAC address as string
    pub fn mac_str(&self) -> [u8; 17] {
        self.device.mac_str()
    }
    
    /// Check for ICMP echo reply by directly examining received packets
    /// Also handles loopback replies
    pub fn check_ping_reply(&mut self) -> Option<(Ipv4Address, u16, u16)> {
        // First check for loopback replies (highest priority, instant)
        if let Some(reply) = self.loopback_replies.pop_front() {
            return Some((reply.from, reply.ident, reply.seq));
        }
        
        // Poll the VirtIO device for incoming packets
        self.device.poll();
        
        // Check for received packet
        if let Some((desc_idx, data)) = self.device.recv_with_desc() {
            // Must be at least: eth(14) + ip(20) + icmp(8) = 42 bytes
            if data.len() >= 42 {
                // Check for IPv4 (ethertype 0x0800)
                if data[12] == 0x08 && data[13] == 0x00 {
                    // Check IP protocol is ICMP (1)
                    if data[23] == 1 {
                        // Check ICMP type is echo reply (0)
                        if data[34] == 0 {
                            // Parse ICMP echo reply
                            let ident = u16::from_be_bytes([data[38], data[39]]);
                            let seq = u16::from_be_bytes([data[40], data[41]]);
                            let src_ip = Ipv4Address::new(data[26], data[27], data[28], data[29]);
                            
                            // Recycle the buffer
                            self.device.recycle_rx(desc_idx);
                            
                            // Check if this is for our identifier
                            if ident == ICMP_IDENT {
                                return Some((src_ip, ident, seq));
                            }
                        }
                    }
                }
            }
            
            // Recycle buffer if we didn't return earlier
            self.device.recycle_rx(desc_idx);
        }
        None
    }
    
    /// Send a UDP packet to the specified destination
    pub fn udp_send(&mut self, dest_ip: Ipv4Address, dest_port: u16, data: &[u8], timestamp_ms: i64) -> Result<(), &'static str> {
        let timestamp = Instant::from_millis(timestamp_ms);
        
        // Get the UDP socket
        let socket = self.sockets.get_mut::<udp::Socket>(self.udp_handle);
        
        // Create destination endpoint
        let endpoint = IpEndpoint::new(IpAddress::Ipv4(dest_ip), dest_port);
        
        // Check if socket can send
        if !socket.can_send() {
            return Err("UDP socket cannot send");
        }
        
        // Send the data
        socket.send_slice(data, endpoint)
            .map_err(|_| "Failed to send UDP packet")?;
        
        // Poll to actually transmit
        self.iface.poll(timestamp, &mut DeviceWrapper(&mut self.device), &mut self.sockets);
        
        Ok(())
    }
    
    /// Receive a UDP packet (non-blocking)
    /// Returns (source_ip, source_port, data) if a packet is available
    pub fn udp_recv(&mut self, buf: &mut [u8], timestamp_ms: i64) -> Option<(Ipv4Address, u16, usize)> {
        let timestamp = Instant::from_millis(timestamp_ms);
        
        // Poll to receive any pending packets
        self.iface.poll(timestamp, &mut DeviceWrapper(&mut self.device), &mut self.sockets);
        
        // Get the UDP socket
        let socket = self.sockets.get_mut::<udp::Socket>(self.udp_handle);
        
        // Check if we can receive
        if !socket.can_recv() {
            return None;
        }
        
        // Try to receive
        match socket.recv_slice(buf) {
            Ok((len, meta)) => {
                let IpAddress::Ipv4(src_ip) = meta.endpoint.addr;
                Some((src_ip, meta.endpoint.port, len))
            }
            Err(_) => None,
        }
    }
    
    /// Check if UDP socket can receive data
    pub fn udp_can_recv(&mut self) -> bool {
        let socket = self.sockets.get_mut::<udp::Socket>(self.udp_handle);
        socket.can_recv()
    }
}

/// Wrapper for VirtioNet to implement smoltcp Device trait
struct DeviceWrapper<'a>(&'a mut VirtioNet);

impl Device for DeviceWrapper<'_> {
    type RxToken<'a> = VirtioRxToken where Self: 'a;
    type TxToken<'a> = VirtioTxToken<'a> where Self: 'a;
    
    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = 1500; // Standard Ethernet MTU (IP payload size)
        caps.max_burst_size = Some(1);
        caps
    }
    
    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        // Check if there's a received packet
        if let Some((desc_idx, data)) = self.0.recv_with_desc() {
            // Copy data since we need to recycle the buffer
            let mut buf = vec![0u8; data.len()];
            buf.copy_from_slice(data);
            
            // Recycle the RX buffer immediately
            self.0.recycle_rx(desc_idx);
            
            Some((
                VirtioRxToken { buffer: buf },
                VirtioTxToken { device: self.0 },
            ))
        } else {
            None
        }
    }
    
    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        // Always allow transmit (the device will handle buffer exhaustion)
        Some(VirtioTxToken { device: self.0 })
    }
}

/// RX token for received packets
struct VirtioRxToken {
    buffer: Vec<u8>,
}

impl RxToken for VirtioRxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buffer = self.buffer;
        f(&mut buffer)
    }
}

/// TX token for transmitting packets
struct VirtioTxToken<'a> {
    device: &'a mut VirtioNet,
}

impl TxToken for VirtioTxToken<'_> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buffer = vec![0u8; len];
        let result = f(&mut buffer);
        
        // Send the packet
        if let Err(e) = self.device.send(&buffer) {
            // Log error but don't fail (network errors are recoverable)
            crate::uart::write_str("TX error: ");
            crate::uart::write_line(e);
        }
        
        result
    }
}

/// Parse an IPv4 address from bytes
pub fn parse_ipv4(s: &[u8]) -> Option<Ipv4Address> {
    let mut octets = [0u8; 4];
    let mut octet_idx = 0;
    let mut current = 0u16;
    let mut has_digit = false;
    
    for &b in s {
        if b >= b'0' && b <= b'9' {
            current = current * 10 + (b - b'0') as u16;
            has_digit = true;
            if current > 255 {
                return None;
            }
        } else if b == b'.' {
            if !has_digit || octet_idx >= 3 {
                return None;
            }
            octets[octet_idx] = current as u8;
            octet_idx += 1;
            current = 0;
            has_digit = false;
        } else {
            return None;
        }
    }
    
    if !has_digit || octet_idx != 3 {
        return None;
    }
    octets[3] = current as u8;
    
    Some(Ipv4Address::new(octets[0], octets[1], octets[2], octets[3]))
}

/// Format an IPv4 address to a buffer
pub fn format_ipv4(addr: Ipv4Address, buf: &mut [u8]) -> usize {
    let octets = addr.0;
    let mut pos = 0;
    
    for (i, &octet) in octets.iter().enumerate() {
        // Write octet
        if octet >= 100 {
            buf[pos] = b'0' + (octet / 100);
            pos += 1;
        }
        if octet >= 10 {
            buf[pos] = b'0' + ((octet / 10) % 10);
            pos += 1;
        }
        buf[pos] = b'0' + (octet % 10);
        pos += 1;
        
        // Write dot (except after last octet)
        if i < 3 {
            buf[pos] = b'.';
            pos += 1;
        }
    }
    
    pos
}
