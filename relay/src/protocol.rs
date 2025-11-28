//! Protocol definitions for the P2P WebTransport relay network.
//!
//! Frames are prefixed with a message type byte:
//! - 0x00 = Control message (JSON-encoded)
//! - 0x01 = Ethernet data frame
//!
//! Control messages handle peer registration, IP assignment, and heartbeat.

use serde::{Deserialize, Serialize};

/// Message type prefix bytes
pub const MSG_TYPE_CONTROL: u8 = 0x00;
pub const MSG_TYPE_DATA: u8 = 0x01;

/// Network configuration constants
pub const GATEWAY_IP: [u8; 4] = [10, 0, 2, 2];
pub const GATEWAY_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
pub const NETWORK_MASK: [u8; 4] = [255, 255, 255, 0];
pub const DNS_SERVER: [u8; 4] = [8, 8, 8, 8];

/// IP pool range for peer assignment
pub const IP_POOL_START: u8 = 10; // 10.0.2.10
pub const IP_POOL_END: u8 = 254;  // 10.0.2.254

/// Control messages exchanged between peers and the relay hub.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ControlMessage {
    /// Peer requests registration with its MAC address
    Register {
        mac: [u8; 6],
    },
    
    /// Hub assigns IP configuration to peer
    Assigned {
        ip: [u8; 4],
        gateway: [u8; 4],
        netmask: [u8; 4],
        dns: [u8; 4],
    },
    
    /// Heartbeat to keep connection alive
    Heartbeat,
    
    /// Heartbeat acknowledgment
    HeartbeatAck,
    
    /// Peer disconnecting gracefully
    Disconnect,
    
    /// Error message from hub
    Error {
        message: String,
    },
    
    /// List of connected peers (optional, for discovery)
    PeerList {
        peers: Vec<PeerInfo>,
    },
}

/// Information about a connected peer
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerInfo {
    pub ip: [u8; 4],
    pub mac: [u8; 6],
}

impl ControlMessage {
    /// Encode a control message as a framed datagram (type prefix + JSON)
    pub fn encode(&self) -> Vec<u8> {
        let json = serde_json::to_vec(self).expect("Failed to serialize control message");
        let mut frame = Vec::with_capacity(1 + json.len());
        frame.push(MSG_TYPE_CONTROL);
        frame.extend(json);
        frame
    }
    
    /// Decode a control message from a framed datagram
    pub fn decode(data: &[u8]) -> Result<Self, String> {
        if data.is_empty() {
            return Err("Empty message".to_string());
        }
        if data[0] != MSG_TYPE_CONTROL {
            return Err(format!("Not a control message (type={})", data[0]));
        }
        serde_json::from_slice(&data[1..])
            .map_err(|e| format!("Failed to parse control message: {}", e))
    }
}

/// Encode an Ethernet frame as a data datagram
pub fn encode_data_frame(ethernet_frame: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(1 + ethernet_frame.len());
    frame.push(MSG_TYPE_DATA);
    frame.extend(ethernet_frame);
    frame
}

/// Helper to format MAC address for display
pub fn format_mac(mac: &[u8; 6]) -> String {
    format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    )
}

/// Helper to format IP address for display
pub fn format_ip(ip: &[u8; 4]) -> String {
    format!("{}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_control_message_roundtrip() {
        let msg = ControlMessage::Register {
            mac: [0x52, 0x54, 0x00, 0xab, 0xcd, 0xef],
        };
        let encoded = msg.encode();
        let decoded = ControlMessage::decode(&encoded).unwrap();
        
        match decoded {
            ControlMessage::Register { mac } => {
                assert_eq!(mac, [0x52, 0x54, 0x00, 0xab, 0xcd, 0xef]);
            }
            _ => panic!("Wrong message type"),
        }
    }

    #[test]
    fn test_data_frame_roundtrip() {
        let ethernet = vec![0xff; 64];
        let encoded = encode_data_frame(&ethernet);
        let decoded = decode_data_frame(&encoded).unwrap();
        assert_eq!(decoded, ethernet.as_slice());
    }
}

