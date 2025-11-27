//! Network backend abstraction for VirtIO networking.
//!
//! This module defines the `NetworkBackend` trait that abstracts packet I/O
//! to support both Host (TAP) and WASM (WebSocket) environments.

/// Trait for network backends that provide packet I/O.
/// 
/// Implementations must be `Send` to allow the backend to be used
/// across thread boundaries (e.g., when the VM runs in a separate thread).
pub trait NetworkBackend: Send {
    /// Initialize the backend (e.g., open TAP device or connect WebSocket).
    fn init(&mut self) -> Result<(), String>;
    
    /// Poll for an incoming packet. Returns None if no packet is available.
    /// This should be non-blocking.
    fn recv(&mut self) -> Result<Option<Vec<u8>>, String>;
    
    /// Send a packet.
    fn send(&self, buf: &[u8]) -> Result<(), String>;
    
    /// Get the MAC address of the backend (if available).
    /// Returns a default MAC if the backend doesn't have one.
    fn mac_address(&self) -> [u8; 6] {
        // Default MAC: locally administered, unicast
        [0x52, 0x54, 0x00, 0x12, 0x34, 0x56]
    }
}

/// A no-op network backend for testing purposes.
/// 
/// This backend discards all sent packets and never receives any packets.
pub struct DummyBackend {
    initialized: bool,
    mac: [u8; 6],
}

impl DummyBackend {
    pub fn new() -> Self {
        Self {
            initialized: false,
            mac: [0x52, 0x54, 0x00, 0x12, 0x34, 0x56],
        }
    }
    
    /// Create a dummy backend with a custom MAC address.
    pub fn with_mac(mac: [u8; 6]) -> Self {
        Self {
            initialized: false,
            mac,
        }
    }
}

impl Default for DummyBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl NetworkBackend for DummyBackend {
    fn init(&mut self) -> Result<(), String> {
        self.initialized = true;
        log::debug!("[DummyBackend] Initialized (no-op)");
        Ok(())
    }
    
    fn recv(&mut self) -> Result<Option<Vec<u8>>, String> {
        // No packets ever available
        Ok(None)
    }
    
    fn send(&self, buf: &[u8]) -> Result<(), String> {
        // Discard packet, but log it for debugging
        log::trace!("[DummyBackend] Discarding {} byte packet", buf.len());
        Ok(())
    }
    
    fn mac_address(&self) -> [u8; 6] {
        self.mac
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_dummy_backend_init() {
        let mut backend = DummyBackend::new();
        assert!(backend.init().is_ok());
    }
    
    #[test]
    fn test_dummy_backend_recv_returns_none() {
        let mut backend = DummyBackend::new();
        backend.init().unwrap();
        assert!(backend.recv().unwrap().is_none());
    }
    
    #[test]
    fn test_dummy_backend_send_succeeds() {
        let backend = DummyBackend::new();
        assert!(backend.send(&[1, 2, 3, 4]).is_ok());
    }
    
    #[test]
    fn test_dummy_backend_mac_address() {
        let backend = DummyBackend::new();
        let mac = backend.mac_address();
        // Check locally administered bit is set (second bit of first byte)
        assert_eq!(mac[0] & 0x02, 0x02);
    }
    
    #[test]
    fn test_dummy_backend_custom_mac() {
        let custom_mac = [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF];
        let backend = DummyBackend::with_mac(custom_mac);
        assert_eq!(backend.mac_address(), custom_mac);
    }
}

