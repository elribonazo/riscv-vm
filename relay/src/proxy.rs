//! External traffic proxy for the relay hub.
//!
//! Handles:
//! - UDP proxy (DNS queries, etc.)
//! - ICMP proxy (ping requests)

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::sync::Mutex;

use crate::protocol::GATEWAY_MAC;

/// Session for tracking NAT'ed UDP connections
#[derive(Debug, Clone)]
struct UdpSession {
    /// Original source MAC
    src_mac: [u8; 6],
    /// Original source IP
    src_ip: [u8; 4],
    /// Original source port
    src_port: u16,
    /// External destination IP
    dst_ip: [u8; 4],
    /// External destination port
    dst_port: u16,
    /// Creation time
    created: Instant,
}

/// External traffic proxy
pub struct ExternalProxy {
    /// UDP socket for external traffic
    udp_socket: Mutex<Option<Arc<UdpSocket>>>,
    /// Active UDP sessions (keyed by local port or dst:port combo)
    udp_sessions: Mutex<HashMap<(Ipv4Addr, u16, u16), UdpSession>>,
    /// Session timeout
    session_timeout: Duration,
}

impl ExternalProxy {
    pub fn new() -> Self {
        Self {
            udp_socket: Mutex::new(None),
            udp_sessions: Mutex::new(HashMap::new()),
            session_timeout: Duration::from_secs(30),
        }
    }

    /// Initialize the proxy (bind UDP socket)
    pub async fn init(&self) -> anyhow::Result<()> {
        let socket = UdpSocket::bind("0.0.0.0:0").await?;
        tracing::info!("External proxy UDP socket bound to {}", socket.local_addr()?);
        *self.udp_socket.lock().await = Some(Arc::new(socket));
        Ok(())
    }

    /// Get the UDP socket for receiving
    pub async fn udp_socket(&self) -> Option<Arc<UdpSocket>> {
        self.udp_socket.lock().await.clone()
    }

    /// Handle an external-bound packet from a peer
    pub async fn handle_external_packet(&self, frame: &[u8]) -> Option<Vec<u8>> {
        if frame.len() < 34 {
            return None;
        }

        let protocol = frame[23];

        match protocol {
            1 => self.handle_icmp(frame).await,  // ICMP
            17 => self.handle_udp(frame).await,  // UDP
            6 => {
                // TCP - not yet implemented
                tracing::debug!("TCP proxy not implemented");
                None
            }
            _ => {
                tracing::trace!("Unsupported protocol: {}", protocol);
                None
            }
        }
    }

    /// Handle outbound ICMP (ping) request
    async fn handle_icmp(&self, frame: &[u8]) -> Option<Vec<u8>> {
        if frame.len() < 42 {
            return None;
        }

        // Check if this is an echo request (type 8)
        if frame[34] != 8 {
            return None;
        }

        let src_mac: [u8; 6] = frame[6..12].try_into().ok()?;
        let src_ip: [u8; 4] = frame[26..30].try_into().ok()?;
        let dst_ip: [u8; 4] = frame[30..34].try_into().ok()?;
        let ident = u16::from_be_bytes([frame[38], frame[39]]);
        let seq = u16::from_be_bytes([frame[40], frame[41]]);

        let dst_addr = Ipv4Addr::from(dst_ip);
        tracing::debug!(
            "ICMP proxy: ping {} (ident={}, seq={})",
            dst_addr,
            ident,
            seq
        );

        // Execute ping using system command
        // This works in Docker without NET_ADMIN capability
        let output = tokio::process::Command::new("ping")
            .args(["-c", "1", "-W", "3", &dst_addr.to_string()])
            .output()
            .await;

        match output {
            Ok(out) if out.status.success() => {
                tracing::debug!("ICMP proxy: ping {} succeeded", dst_addr);
                Some(self.generate_icmp_reply(&src_mac, &src_ip, &dst_ip, ident, seq))
            }
            Ok(_) => {
                tracing::debug!("ICMP proxy: ping {} failed (timeout or unreachable)", dst_addr);
                None
            }
            Err(e) => {
                tracing::warn!("ICMP proxy: failed to execute ping: {}", e);
                None
            }
        }
    }

    /// Generate an ICMP echo reply frame
    fn generate_icmp_reply(
        &self,
        dst_mac: &[u8; 6],
        dst_ip: &[u8; 4],
        src_ip: &[u8; 4],
        ident: u16,
        seq: u16,
    ) -> Vec<u8> {
        let icmp_data = b"RISCV_PING"; // Match kernel's ping data
        let icmp_len = 8 + icmp_data.len();
        let ip_len = 20 + icmp_len;
        let frame_len = 14 + ip_len;

        let mut frame = vec![0u8; frame_len];

        // Ethernet header
        frame[0..6].copy_from_slice(dst_mac);
        frame[6..12].copy_from_slice(&GATEWAY_MAC);
        frame[12..14].copy_from_slice(&[0x08, 0x00]);

        // IP header
        frame[14] = 0x45;
        frame[15] = 0;
        frame[16..18].copy_from_slice(&(ip_len as u16).to_be_bytes());
        frame[18..20].copy_from_slice(&ident.to_be_bytes());
        frame[20..22].copy_from_slice(&[0x00, 0x00]);
        frame[22] = 64; // TTL
        frame[23] = 1;  // ICMP
        frame[24..26].copy_from_slice(&[0x00, 0x00]); // checksum placeholder
        frame[26..30].copy_from_slice(src_ip);
        frame[30..34].copy_from_slice(dst_ip);

        // IP checksum
        let ip_checksum = compute_checksum(&frame[14..34]);
        frame[24] = (ip_checksum >> 8) as u8;
        frame[25] = (ip_checksum & 0xff) as u8;

        // ICMP header
        frame[34] = 0; // Echo reply
        frame[35] = 0; // Code
        frame[36..38].copy_from_slice(&[0x00, 0x00]); // checksum placeholder
        frame[38..40].copy_from_slice(&ident.to_be_bytes());
        frame[40..42].copy_from_slice(&seq.to_be_bytes());
        frame[42..].copy_from_slice(icmp_data);

        // ICMP checksum
        let icmp_checksum = compute_checksum(&frame[34..]);
        frame[36] = (icmp_checksum >> 8) as u8;
        frame[37] = (icmp_checksum & 0xff) as u8;

        frame
    }

    /// Handle outbound UDP packet
    async fn handle_udp(&self, frame: &[u8]) -> Option<Vec<u8>> {
        if frame.len() < 42 {
            return None;
        }

        let src_mac: [u8; 6] = frame[6..12].try_into().ok()?;
        let src_ip: [u8; 4] = frame[26..30].try_into().ok()?;
        let dst_ip: [u8; 4] = frame[30..34].try_into().ok()?;

        // Get IP header length
        let ihl = ((frame[14] & 0x0f) * 4) as usize;
        let udp_start = 14 + ihl;

        if frame.len() < udp_start + 8 {
            return None;
        }

        let src_port = u16::from_be_bytes([frame[udp_start], frame[udp_start + 1]]);
        let dst_port = u16::from_be_bytes([frame[udp_start + 2], frame[udp_start + 3]]);
        let udp_len = u16::from_be_bytes([frame[udp_start + 4], frame[udp_start + 5]]) as usize;

        let payload_start = udp_start + 8;
        let payload_end = std::cmp::min(udp_start + udp_len, frame.len());

        if payload_start >= payload_end {
            return None;
        }

        let payload = &frame[payload_start..payload_end];
        let dst_addr = Ipv4Addr::from(dst_ip);

        tracing::debug!(
            "UDP proxy: {}:{} -> {}:{} ({} bytes)",
            Ipv4Addr::from(src_ip),
            src_port,
            dst_addr,
            dst_port,
            payload.len()
        );

        // Store session for response matching
        let session = UdpSession {
            src_mac,
            src_ip,
            src_port,
            dst_ip,
            dst_port,
            created: Instant::now(),
        };

        {
            let mut sessions = self.udp_sessions.lock().await;
            sessions.insert((dst_addr, dst_port, src_port), session);
        }

        // Send to external destination
        let socket = self.udp_socket.lock().await;
        if let Some(ref socket) = *socket {
            let dest = SocketAddrV4::new(dst_addr, dst_port);
            match socket.send_to(payload, dest).await {
                Ok(n) => {
                    tracing::debug!("UDP proxy: sent {} bytes to {}", n, dest);
                }
                Err(e) => {
                    tracing::warn!("UDP proxy: send failed: {}", e);
                }
            }
        }

        // For DNS, we need to wait for a response and return it
        // For now, responses are handled asynchronously via handle_incoming_udp
        None
    }

    /// Handle an incoming UDP packet from the external network
    pub async fn handle_incoming_udp(
        &self,
        data: &[u8],
        src_addr: SocketAddr,
        len: usize,
    ) -> Option<Vec<u8>> {
        self.cleanup_expired_sessions().await;

        let src_ip = match src_addr.ip() {
            std::net::IpAddr::V4(ip) => ip,
            _ => return None,
        };
        let src_port = src_addr.port();

        // Find matching session
        let session = {
            let sessions = self.udp_sessions.lock().await;
            // For DNS responses, the source is the DNS server
            // Try to find a session that matches
            let mut found = None;
            for ((dst_ip, dst_port, _vm_port), session) in sessions.iter() {
                // Match by destination (external server) port
                if *dst_port == src_port {
                    // Check if IP matches or if it's DNS (port 53)
                    if *dst_ip == src_ip || src_port == 53 {
                        found = Some(session.clone());
                        break;
                    }
                }
            }
            found
        };

        if let Some(session) = session {
            tracing::debug!(
                "UDP proxy: response from {} -> VM port {}",
                src_addr,
                session.src_port
            );
            Some(self.generate_udp_response(&session, &data[..len]))
        } else {
            tracing::trace!("UDP proxy: no matching session for {}", src_addr);
            None
        }
    }

    /// Generate a UDP response frame to send back to the VM
    fn generate_udp_response(&self, session: &UdpSession, payload: &[u8]) -> Vec<u8> {
        let udp_len = 8 + payload.len();
        let ip_len = 20 + udp_len;
        let frame_len = 14 + ip_len;

        let mut frame = vec![0u8; frame_len];

        // Ethernet header
        frame[0..6].copy_from_slice(&session.src_mac);
        frame[6..12].copy_from_slice(&GATEWAY_MAC);
        frame[12..14].copy_from_slice(&[0x08, 0x00]);

        // IP header
        frame[14] = 0x45;
        frame[15] = 0;
        frame[16..18].copy_from_slice(&(ip_len as u16).to_be_bytes());
        frame[18..20].copy_from_slice(&[0x00, 0x00]); // identification
        frame[20..22].copy_from_slice(&[0x40, 0x00]); // DF flag
        frame[22] = 64; // TTL
        frame[23] = 17; // UDP
        frame[24..26].copy_from_slice(&[0x00, 0x00]); // checksum placeholder
        frame[26..30].copy_from_slice(&session.dst_ip); // src = external server
        frame[30..34].copy_from_slice(&session.src_ip); // dst = VM

        // IP checksum
        let ip_checksum = compute_checksum(&frame[14..34]);
        frame[24] = (ip_checksum >> 8) as u8;
        frame[25] = (ip_checksum & 0xff) as u8;

        // UDP header
        let udp_start = 34;
        frame[udp_start..udp_start + 2].copy_from_slice(&session.dst_port.to_be_bytes());
        frame[udp_start + 2..udp_start + 4].copy_from_slice(&session.src_port.to_be_bytes());
        frame[udp_start + 4..udp_start + 6].copy_from_slice(&(udp_len as u16).to_be_bytes());
        frame[udp_start + 6..udp_start + 8].copy_from_slice(&[0x00, 0x00]); // checksum optional

        // UDP payload
        frame[udp_start + 8..].copy_from_slice(payload);

        frame
    }

    /// Clean up expired sessions
    async fn cleanup_expired_sessions(&self) {
        let mut sessions = self.udp_sessions.lock().await;
        sessions.retain(|_, session| session.created.elapsed() < self.session_timeout);
    }
}

impl Default for ExternalProxy {
    fn default() -> Self {
        Self::new()
    }
}

/// Compute Internet checksum
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_proxy_creation() {
        let proxy = ExternalProxy::new();
        assert!(proxy.udp_socket().await.is_none());
    }
}

