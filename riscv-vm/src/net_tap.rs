//! TAP network backend for native (non-WASM) builds.
//!
//! This module provides a network backend that uses a TAP interface
//! to communicate with the host network stack on Linux/macOS.

use crate::net::NetworkBackend;
use std::io::Write;
use std::os::unix::io::AsRawFd;

/// TAP network backend using the tun-tap crate.
pub struct TapBackend {
    name: String,
    iface: Option<tun_tap::Iface>,
    mac: [u8; 6],
}

impl TapBackend {
    /// Create a new TAP backend with the given interface name.
    /// 
    /// The interface will not be opened until `init()` is called.
    /// Creating the TAP interface typically requires root privileges.
    /// 
    /// # Example
    /// ```ignore
    /// let mut tap = TapBackend::new("tap0");
    /// tap.init()?;
    /// ```
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            iface: None,
            // Default MAC - locally administered, unicast
            mac: [0x52, 0x54, 0x00, 0x12, 0x34, 0x56],
        }
    }
    
    /// Create a TAP backend with a custom MAC address.
    pub fn with_mac(name: &str, mac: [u8; 6]) -> Self {
        Self {
            name: name.to_string(),
            iface: None,
            mac,
        }
    }
    
    /// Set the interface to non-blocking mode.
    fn set_nonblocking(&self) -> Result<(), String> {
        if let Some(ref iface) = self.iface {
            let fd = iface.as_raw_fd();
            
            // Get current flags
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags < 0 {
                return Err(format!(
                    "Failed to get fd flags: {}",
                    std::io::Error::last_os_error()
                ));
            }
            
            // Set non-blocking flag
            let result = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
            if result < 0 {
                return Err(format!(
                    "Failed to set non-blocking mode: {}",
                    std::io::Error::last_os_error()
                ));
            }
        }
        Ok(())
    }
}

impl NetworkBackend for TapBackend {
    fn init(&mut self) -> Result<(), String> {
        // Open the TAP interface
        let iface = tun_tap::Iface::without_packet_info(&self.name, tun_tap::Mode::Tap)
            .map_err(|e| format!("Failed to open TAP interface '{}': {}", self.name, e))?;
        
        self.iface = Some(iface);
        
        // Set non-blocking mode for recv polling
        self.set_nonblocking()?;
        
        log::info!("[TapBackend] Opened TAP interface '{}'", self.name);
        Ok(())
    }
    
    fn recv(&mut self) -> Result<Option<Vec<u8>>, String> {
        let iface = self.iface.as_mut().ok_or("TAP interface not initialized")?;
        
        // Buffer for maximum Ethernet frame size (MTU 1500 + headers)
        let mut buf = vec![0u8; 1514];
        
        match iface.recv(&mut buf) {
            Ok(n) => {
                buf.truncate(n);
                log::trace!("[TapBackend] Received {} byte packet", n);
                Ok(Some(buf))
            }
            Err(e) => {
                // Check if it's a would-block error (no data available)
                if e.kind() == std::io::ErrorKind::WouldBlock {
                    Ok(None)
                } else {
                    Err(format!("TAP recv error: {}", e))
                }
            }
        }
    }
    
    fn send(&self, buf: &[u8]) -> Result<(), String> {
        let iface = self.iface.as_ref().ok_or("TAP interface not initialized")?;
        
        // tun_tap::Iface doesn't implement Send on the write side,
        // so we need to use the raw fd for writing. However, for simplicity
        // we'll just write directly. Note: this may need adjustment for
        // thread safety in the future.
        let mut iface_clone = unsafe {
            // This is safe because we're only writing and the fd is valid
            std::fs::File::from_raw_fd(std::os::unix::io::AsRawFd::as_raw_fd(iface))
        };
        
        let result = iface_clone.write_all(buf);
        
        // Don't drop the file - it doesn't own the fd
        std::mem::forget(iface_clone);
        
        result.map_err(|e| format!("TAP send error: {}", e))?;
        log::trace!("[TapBackend] Sent {} byte packet", buf.len());
        Ok(())
    }
    
    fn mac_address(&self) -> [u8; 6] {
        self.mac
    }
}

// Manual implementation to handle the Iface not being Send
unsafe impl Send for TapBackend {}

use std::os::unix::io::FromRawFd;

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_tap_backend_creation() {
        let tap = TapBackend::new("test0");
        assert_eq!(tap.name, "test0");
        assert!(tap.iface.is_none());
    }
    
    #[test]
    fn test_tap_backend_custom_mac() {
        let mac = [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF];
        let tap = TapBackend::with_mac("test0", mac);
        assert_eq!(tap.mac_address(), mac);
    }
    
    // Note: Actually opening a TAP interface requires root privileges,
    // so we can't test init() in regular unit tests.
}

