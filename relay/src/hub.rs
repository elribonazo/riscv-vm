//! Central hub logic for the P2P WebTransport relay.
//!
//! The hub manages:
//! - Peer connections and registration
//! - Ethernet frame routing between peers
//! - ARP handling for the virtual gateway
//! - Forwarding external traffic to the proxy

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, RwLock};

use crate::peer::{PeerId, PeerManager};
use crate::protocol::{
    encode_data_frame, format_ip, format_mac, ControlMessage, DNS_SERVER, GATEWAY_IP, GATEWAY_MAC,
    MSG_TYPE_CONTROL, MSG_TYPE_DATA, NETWORK_MASK,
};
use crate::proxy::ExternalProxy;

/// Message sent to a peer connection task
#[derive(Debug, Clone)]
pub enum PeerMessage {
    /// Send a datagram to the peer
    Send(Vec<u8>),
    /// Disconnect the peer
    Disconnect,
}

/// The central hub that manages all peer connections and routing
pub struct Hub {
    /// Peer manager (shared state)
    peers: Arc<RwLock<PeerManager>>,
    /// Per-peer sender channels
    peer_senders: Arc<RwLock<HashMap<PeerId, mpsc::Sender<PeerMessage>>>>,
    /// External traffic proxy
    proxy: Arc<ExternalProxy>,
    /// Broadcast channel for frames (used for broadcasting)
    broadcast_tx: broadcast::Sender<(PeerId, Vec<u8>)>,
}

impl Hub {
    pub fn new() -> Self {
        let (broadcast_tx, _) = broadcast::channel(1024);
        Self {
            peers: Arc::new(RwLock::new(PeerManager::new())),
            peer_senders: Arc::new(RwLock::new(HashMap::new())),
            proxy: Arc::new(ExternalProxy::new()),
            broadcast_tx,
        }
    }

    /// Get a clone of the peers manager
    pub fn peers(&self) -> Arc<RwLock<PeerManager>> {
        self.peers.clone()
    }

    /// Get a clone of the proxy
    pub fn proxy(&self) -> Arc<ExternalProxy> {
        self.proxy.clone()
    }

    /// Subscribe to the broadcast channel
    pub fn subscribe(&self) -> broadcast::Receiver<(PeerId, Vec<u8>)> {
        self.broadcast_tx.subscribe()
    }

    /// Register a new peer connection
    pub async fn register_peer(
        &self,
        mac: [u8; 6],
        sender: mpsc::Sender<PeerMessage>,
    ) -> Option<(PeerId, [u8; 4])> {
        let mut peers = self.peers.write().await;
        let result = peers.register(mac)?;
        let (peer_id, ip) = result;

        let mut senders = self.peer_senders.write().await;
        senders.insert(peer_id, sender);

        // Send the assignment message
        let msg = ControlMessage::Assigned {
            ip,
            gateway: GATEWAY_IP,
            netmask: NETWORK_MASK,
            dns: DNS_SERVER,
        };

        if let Some(sender) = senders.get(&peer_id) {
            let _ = sender.send(PeerMessage::Send(msg.encode())).await;
        }

        Some((peer_id, ip))
    }

    /// Unregister a peer
    pub async fn unregister_peer(&self, peer_id: PeerId) {
        let mut peers = self.peers.write().await;
        peers.unregister(peer_id);

        let mut senders = self.peer_senders.write().await;
        senders.remove(&peer_id);
    }

    /// Update last-seen timestamp for a peer
    pub async fn touch_peer(&self, peer_id: PeerId) {
        let mut peers = self.peers.write().await;
        peers.touch(peer_id);
    }

    /// Route an incoming frame from a peer
    pub async fn route_frame(&self, from_peer: PeerId, data: Vec<u8>) {
        if data.is_empty() {
            return;
        }

        match data[0] {
            MSG_TYPE_CONTROL => {
                self.handle_control_message(from_peer, &data).await;
            }
            MSG_TYPE_DATA => {
                self.route_data_frame(from_peer, &data[1..]).await;
            }
            _ => {
                tracing::warn!("Unknown message type: {}", data[0]);
            }
        }
    }

    /// Handle a control message
    async fn handle_control_message(&self, from_peer: PeerId, data: &[u8]) {
        match ControlMessage::decode(data) {
            Ok(ControlMessage::Heartbeat) => {
                self.touch_peer(from_peer).await;
                // Send heartbeat ack
                let ack = ControlMessage::HeartbeatAck;
                self.send_to_peer(from_peer, ack.encode()).await;
            }
            Ok(ControlMessage::Disconnect) => {
                tracing::info!("Peer {} requested disconnect", from_peer);
                self.unregister_peer(from_peer).await;
            }
            Ok(msg) => {
                tracing::debug!("Received control message from peer {}: {:?}", from_peer, msg);
            }
            Err(e) => {
                tracing::warn!("Failed to decode control message: {}", e);
            }
        }
    }

    /// Route an Ethernet data frame
    async fn route_data_frame(&self, from_peer: PeerId, ethernet_frame: &[u8]) {
        if ethernet_frame.len() < 14 {
            return; // Too short for Ethernet header
        }

        let dst_mac: [u8; 6] = ethernet_frame[0..6].try_into().unwrap();
        let _src_mac: [u8; 6] = ethernet_frame[6..12].try_into().unwrap();
        let ethertype = u16::from_be_bytes([ethernet_frame[12], ethernet_frame[13]]);

        // Check for broadcast MAC
        let is_broadcast = dst_mac == [0xff, 0xff, 0xff, 0xff, 0xff, 0xff];

        // Handle ARP for gateway
        if ethertype == 0x0806 && self.is_arp_request_for_gateway(ethernet_frame) {
            let reply = self.generate_arp_reply(ethernet_frame);
            self.send_to_peer(from_peer, encode_data_frame(&reply)).await;
            return;
        }

        // Handle IPv4
        if ethertype == 0x0800 && ethernet_frame.len() >= 34 {
            let dst_ip: [u8; 4] = ethernet_frame[30..34].try_into().unwrap();

            // Check if destination is gateway (ping to gateway)
            if dst_ip == GATEWAY_IP {
                if let Some(reply) = self.handle_gateway_packet(ethernet_frame).await {
                    self.send_to_peer(from_peer, encode_data_frame(&reply)).await;
                }
                return;
            }

            // Check if destination is external
            let peers = self.peers.read().await;
            if !peers.is_internal_ip(&dst_ip) {
                drop(peers);
                // Route to external proxy
                if let Some(reply) = self.proxy.handle_external_packet(ethernet_frame).await {
                    self.send_to_peer(from_peer, encode_data_frame(&reply)).await;
                }
                return;
            }

            // Route to internal peer
            if let Some(target_peer) = peers.peer_id_by_ip(&dst_ip) {
                drop(peers);
                if target_peer != from_peer {
                    self.send_to_peer(target_peer, encode_data_frame(ethernet_frame))
                        .await;
                }
                return;
            }
        }

        // Broadcast handling
        if is_broadcast {
            let _ = self
                .broadcast_tx
                .send((from_peer, encode_data_frame(ethernet_frame)));
        } else if dst_mac == GATEWAY_MAC {
            // Addressed to gateway but not handled above - drop
            tracing::trace!("Dropping frame addressed to gateway MAC");
        } else {
            // Try to find peer by MAC
            let peers = self.peers.read().await;
            if let Some(peer) = peers.find_by_mac(&dst_mac) {
                let target_id = peer.id;
                drop(peers);
                self.send_to_peer(target_id, encode_data_frame(ethernet_frame))
                    .await;
            }
        }
    }

    /// Send a message to a specific peer
    pub async fn send_to_peer(&self, peer_id: PeerId, data: Vec<u8>) {
        let senders = self.peer_senders.read().await;
        if let Some(sender) = senders.get(&peer_id) {
            let _ = sender.send(PeerMessage::Send(data)).await;
        }
    }

    /// Check if this is an ARP request for the gateway
    fn is_arp_request_for_gateway(&self, frame: &[u8]) -> bool {
        if frame.len() < 42 {
            return false;
        }
        // ARP operation = request (1)
        if frame[20] != 0x00 || frame[21] != 0x01 {
            return false;
        }
        // Target protocol address = gateway IP
        frame[38..42] == GATEWAY_IP
    }

    /// Generate an ARP reply for the gateway
    fn generate_arp_reply(&self, request: &[u8]) -> Vec<u8> {
        let mut reply = vec![0u8; 42];

        // Ethernet header
        reply[0..6].copy_from_slice(&request[6..12]); // dst = requester's MAC
        reply[6..12].copy_from_slice(&GATEWAY_MAC); // src = gateway MAC
        reply[12..14].copy_from_slice(&[0x08, 0x06]); // ethertype = ARP

        // ARP header
        reply[14..16].copy_from_slice(&[0x00, 0x01]); // hardware type = ethernet
        reply[16..18].copy_from_slice(&[0x08, 0x00]); // protocol type = IPv4
        reply[18] = 6; // hardware addr len
        reply[19] = 4; // protocol addr len
        reply[20..22].copy_from_slice(&[0x00, 0x02]); // operation = reply
        reply[22..28].copy_from_slice(&GATEWAY_MAC); // sender hardware addr
        reply[28..32].copy_from_slice(&GATEWAY_IP); // sender protocol addr
        reply[32..38].copy_from_slice(&request[22..28]); // target hardware addr
        reply[38..42].copy_from_slice(&request[28..32]); // target protocol addr

        reply
    }

    /// Handle a packet addressed to the gateway (e.g., ICMP ping)
    async fn handle_gateway_packet(&self, frame: &[u8]) -> Option<Vec<u8>> {
        if frame.len() < 34 {
            return None;
        }

        let protocol = frame[23];

        // ICMP echo request to gateway
        if protocol == 1 && frame.len() >= 42 && frame[34] == 8 {
            return Some(self.generate_icmp_reply(frame));
        }

        None
    }

    /// Generate an ICMP echo reply
    fn generate_icmp_reply(&self, request: &[u8]) -> Vec<u8> {
        let mut reply = request.to_vec();

        // Swap MAC addresses
        reply[0..6].copy_from_slice(&request[6..12]);
        reply[6..12].copy_from_slice(&GATEWAY_MAC);

        // Swap IP addresses
        let src_ip: [u8; 4] = request[26..30].try_into().unwrap();
        let dst_ip: [u8; 4] = request[30..34].try_into().unwrap();
        reply[26..30].copy_from_slice(&dst_ip);
        reply[30..34].copy_from_slice(&src_ip);

        // Recalculate IP checksum
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
        let icmp_checksum = compute_checksum(&reply[34..]);
        reply[36] = (icmp_checksum >> 8) as u8;
        reply[37] = (icmp_checksum & 0xff) as u8;

        reply
    }

    /// Cleanup expired peers
    pub async fn cleanup_expired_peers(&self) {
        let mut peers = self.peers.write().await;
        let expired = peers.cleanup_expired();
        drop(peers);

        let mut senders = self.peer_senders.write().await;
        for id in expired {
            senders.remove(&id);
        }
    }

    /// Log hub statistics
    pub async fn log_stats(&self) {
        let peers = self.peers.read().await;
        let count = peers.peer_count();
        if count > 0 {
            tracing::info!("Hub stats: {} connected peers", count);
            for peer in peers.all_peers() {
                tracing::debug!(
                    "  Peer {}: MAC={}, IP={}",
                    peer.id,
                    format_mac(&peer.mac),
                    format_ip(&peer.ip)
                );
            }
        }
    }
}

impl Default for Hub {
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
    async fn test_hub_peer_registration() {
        let hub = Hub::new();
        let (tx, _rx) = mpsc::channel(16);

        let mac = [0x52, 0x54, 0x00, 0xab, 0xcd, 0xef];
        let result = hub.register_peer(mac, tx).await;

        assert!(result.is_some());
        let (peer_id, ip) = result.unwrap();
        assert!(peer_id > 0);
        assert_eq!(ip[0..3], [10, 0, 2]);

        assert_eq!(hub.peer_count().await, 1);

        hub.unregister_peer(peer_id).await;
        assert_eq!(hub.peer_count().await, 0);
    }
}

