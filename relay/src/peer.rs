//! Peer state management and IP pool allocation for the relay hub.

use std::collections::HashMap;
use std::time::Instant;

use crate::protocol::{format_ip, format_mac, IP_POOL_END, IP_POOL_START};

/// Unique identifier for a connected peer
pub type PeerId = u64;

/// State of a connected peer
#[derive(Debug, Clone)]
pub struct Peer {
    /// Unique peer ID
    pub id: PeerId,
    /// MAC address of the peer's virtual NIC
    pub mac: [u8; 6],
    /// Assigned IP address
    pub ip: [u8; 4],
    /// Last activity timestamp (for heartbeat timeout)
    pub last_seen: Instant,
}

impl Peer {
    pub fn new(id: PeerId, mac: [u8; 6], ip: [u8; 4]) -> Self {
        Self {
            id,
            mac,
            ip,
            last_seen: Instant::now(),
        }
    }

    pub fn touch(&mut self) {
        self.last_seen = Instant::now();
    }

    pub fn is_expired(&self, timeout_secs: u64) -> bool {
        self.last_seen.elapsed().as_secs() > timeout_secs
    }
}

/// IP address pool manager
#[derive(Debug)]
pub struct IpPool {
    /// Base network (10.0.2.x)
    network_prefix: [u8; 3],
    /// Available host addresses (set of last octet values)
    available: Vec<u8>,
    /// Allocated addresses mapped to peer ID
    allocated: HashMap<u8, PeerId>,
}

impl IpPool {
    pub fn new() -> Self {
        // Initialize with all available addresses in the pool
        let available: Vec<u8> = (IP_POOL_START..=IP_POOL_END).collect();
        Self {
            network_prefix: [10, 0, 2],
            available,
            allocated: HashMap::new(),
        }
    }

    /// Allocate an IP address for a peer
    pub fn allocate(&mut self, peer_id: PeerId) -> Option<[u8; 4]> {
        let host = self.available.pop()?;
        self.allocated.insert(host, peer_id);
        Some([
            self.network_prefix[0],
            self.network_prefix[1],
            self.network_prefix[2],
            host,
        ])
    }

    /// Release an IP address back to the pool
    pub fn release(&mut self, ip: &[u8; 4]) {
        if ip[0] == self.network_prefix[0]
            && ip[1] == self.network_prefix[1]
            && ip[2] == self.network_prefix[2]
        {
            let host = ip[3];
            if self.allocated.remove(&host).is_some() {
                self.available.push(host);
            }
        }
    }

    /// Check if an IP is in our managed range
    pub fn is_internal(&self, ip: &[u8; 4]) -> bool {
        ip[0] == self.network_prefix[0]
            && ip[1] == self.network_prefix[1]
            && ip[2] == self.network_prefix[2]
    }
}

impl Default for IpPool {
    fn default() -> Self {
        Self::new()
    }
}

/// Manages all connected peers and their state
#[derive(Debug)]
pub struct PeerManager {
    /// Connected peers by ID
    peers: HashMap<PeerId, Peer>,
    /// MAC to peer ID mapping for fast lookup
    mac_to_peer: HashMap<[u8; 6], PeerId>,
    /// IP to peer ID mapping for routing
    ip_to_peer: HashMap<[u8; 4], PeerId>,
    /// IP address pool
    ip_pool: IpPool,
    /// Next peer ID
    next_id: PeerId,
    /// Heartbeat timeout in seconds
    heartbeat_timeout: u64,
}

impl PeerManager {
    pub fn new() -> Self {
        Self {
            peers: HashMap::new(),
            mac_to_peer: HashMap::new(),
            ip_to_peer: HashMap::new(),
            ip_pool: IpPool::new(),
            next_id: 1,
            // Increased timeout to tolerate browser tabs going to background
            // Browser tabs may not run JS timers reliably when backgrounded
            heartbeat_timeout: 120,
        }
    }

    /// Register a new peer with the given MAC address
    /// Returns the peer ID and assigned IP, or None if pool exhausted
    pub fn register(&mut self, mac: [u8; 6]) -> Option<(PeerId, [u8; 4])> {
        // Check if MAC already registered
        if let Some(&existing_id) = self.mac_to_peer.get(&mac) {
            // Return existing registration
            if let Some(peer) = self.peers.get(&existing_id) {
                return Some((existing_id, peer.ip));
            }
        }

        // Allocate new peer
        let id = self.next_id;
        self.next_id += 1;

        let ip = self.ip_pool.allocate(id)?;

        let peer = Peer::new(id, mac, ip);
        self.peers.insert(id, peer);
        self.mac_to_peer.insert(mac, id);
        self.ip_to_peer.insert(ip, id);

        tracing::info!(
            "Registered peer {} with MAC {} -> IP {}",
            id,
            format_mac(&mac),
            format_ip(&ip)
        );

        Some((id, ip))
    }

    /// Unregister a peer and release its resources
    pub fn unregister(&mut self, peer_id: PeerId) {
        if let Some(peer) = self.peers.remove(&peer_id) {
            self.mac_to_peer.remove(&peer.mac);
            self.ip_to_peer.remove(&peer.ip);
            self.ip_pool.release(&peer.ip);

            tracing::info!(
                "Unregistered peer {} (MAC {} / IP {})",
                peer_id,
                format_mac(&peer.mac),
                format_ip(&peer.ip)
            );
        }
    }

    /// Update last-seen timestamp for a peer
    pub fn touch(&mut self, peer_id: PeerId) {
        if let Some(peer) = self.peers.get_mut(&peer_id) {
            peer.touch();
        }
    }

    /// Find peer by MAC address
    pub fn find_by_mac(&self, mac: &[u8; 6]) -> Option<&Peer> {
        self.mac_to_peer.get(mac).and_then(|id| self.peers.get(id))
    }

    /// Get peer ID by IP address
    pub fn peer_id_by_ip(&self, ip: &[u8; 4]) -> Option<PeerId> {
        self.ip_to_peer.get(ip).copied()
    }

    /// Get all peers (for peer list message)
    pub fn all_peers(&self) -> Vec<&Peer> {
        self.peers.values().collect()
    }

    /// Check if an IP is internal to our virtual network
    pub fn is_internal_ip(&self, ip: &[u8; 4]) -> bool {
        self.ip_pool.is_internal(ip)
    }

    /// Remove expired peers and return their IDs
    pub fn cleanup_expired(&mut self) -> Vec<PeerId> {
        let expired: Vec<PeerId> = self
            .peers
            .iter()
            .filter(|(_, peer)| peer.is_expired(self.heartbeat_timeout))
            .map(|(&id, _)| id)
            .collect();

        for id in &expired {
            self.unregister(*id);
        }

        expired
    }

    /// Get the number of connected peers
    pub fn peer_count(&self) -> usize {
        self.peers.len()
    }
}

impl Default for PeerManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ip_pool_allocation() {
        let mut pool = IpPool::new();
        
        let ip1 = pool.allocate(1).unwrap();
        assert_eq!(ip1[0..3], [10, 0, 2]);
        assert!(ip1[3] >= IP_POOL_START && ip1[3] <= IP_POOL_END);

        let ip2 = pool.allocate(2).unwrap();
        assert_ne!(ip1, ip2);

        pool.release(&ip1);
        let ip3 = pool.allocate(3).unwrap();
        assert_eq!(ip1, ip3); // Should get the same IP back
    }

    #[test]
    fn test_peer_lookup() {
        let mut manager = PeerManager::new();
        
        let mac = [0x52, 0x54, 0x00, 0xab, 0xcd, 0xef];
        let (id, ip) = manager.register(mac).unwrap();

        assert!(manager.find_by_mac(&mac).is_some());
        assert_eq!(manager.peer_id_by_ip(&ip), Some(id));
    }
}

