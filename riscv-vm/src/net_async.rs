//! Async network backend wrapper for VirtIO networking.
//!
//! This module provides `AsyncNetworkBackend`, which wraps any `NetworkBackend`
//! implementation and provides non-blocking I/O through a dedicated I/O thread
//! and channel-based communication.
//!
//! Benefits:
//! - Non-blocking `try_receive()` for polling without waiting
//! - Non-blocking `send()` that queues packets for async transmission
//! - Lower latency: packets are queued by I/O thread while CPU runs
//! - Better throughput: batched processing of multiple packets

use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::net::NetworkBackend;

/// Async wrapper around NetworkBackend.
///
/// Uses a dedicated thread for network I/O, communicating via channels.
/// This allows the main emulation loop to poll for packets without blocking.
pub struct AsyncNetworkBackend {
    /// Receive channel for incoming packets
    rx: Receiver<Vec<u8>>,

    /// Send channel for outgoing packets
    tx: Sender<Vec<u8>>,

    /// Handle to the I/O thread
    _io_thread: JoinHandle<()>,

    /// Shutdown signal
    shutdown: Arc<AtomicBool>,

    /// MAC address (cached from underlying backend)
    mac: [u8; 6],

    /// Assigned IP address (updated from I/O thread)
    assigned_ip: Arc<std::sync::Mutex<Option<[u8; 4]>>>,
}

impl AsyncNetworkBackend {
    /// Create a new async backend wrapping the given underlying backend.
    ///
    /// This spawns a dedicated I/O thread that handles all blocking network
    /// operations, communicating with the main thread via channels.
    pub fn new(mut backend: Box<dyn NetworkBackend>) -> Self {
        // Get MAC before moving backend to I/O thread
        let mac = backend.mac_address();

        // Initialize the backend
        if let Err(e) = backend.init() {
            log::error!("[AsyncNetworkBackend] Failed to initialize backend: {}", e);
        }

        let (tx_to_net, rx_from_vm) = channel::<Vec<u8>>();
        let (tx_to_vm, rx_from_net) = channel::<Vec<u8>>();
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = Arc::clone(&shutdown);
        let assigned_ip = Arc::new(std::sync::Mutex::new(None));
        let assigned_ip_clone = Arc::clone(&assigned_ip);

        let io_thread = thread::Builder::new()
            .name("virtio-net-io".to_string())
            .spawn(move || {
                Self::io_loop(backend, rx_from_vm, tx_to_vm, shutdown_clone, assigned_ip_clone);
            })
            .expect("Failed to spawn network I/O thread");

        Self {
            rx: rx_from_net,
            tx: tx_to_net,
            _io_thread: io_thread,
            shutdown,
            mac,
            assigned_ip,
        }
    }

    /// I/O thread main loop.
    ///
    /// This thread handles all blocking network operations:
    /// - Polls the backend for incoming packets (with timeout)
    /// - Sends queued outgoing packets
    fn io_loop(
        mut backend: Box<dyn NetworkBackend>,
        rx_from_vm: Receiver<Vec<u8>>,
        tx_to_vm: Sender<Vec<u8>>,
        shutdown: Arc<AtomicBool>,
        assigned_ip: Arc<std::sync::Mutex<Option<[u8; 4]>>>,
    ) {
        log::debug!("[AsyncNetworkBackend] I/O thread started");

        loop {
            if shutdown.load(Ordering::Relaxed) {
                log::debug!("[AsyncNetworkBackend] I/O thread shutting down");
                break;
            }

            // Check for outgoing packets (non-blocking)
            loop {
                match rx_from_vm.try_recv() {
                    Ok(packet) => {
                        if let Err(e) = backend.send(&packet) {
                            log::warn!("[AsyncNetworkBackend] Send error: {}", e);
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        log::debug!("[AsyncNetworkBackend] TX channel disconnected");
                        return;
                    }
                }
            }

            // Check for incoming packets (with timeout to allow shutdown checks)
            match backend.receive_timeout(Duration::from_millis(10)) {
                Ok(Some(packet)) => {
                    log::trace!("[AsyncNetworkBackend] Received {} byte packet", packet.len());
                    if tx_to_vm.send(packet).is_err() {
                        log::debug!("[AsyncNetworkBackend] RX channel disconnected");
                        break;
                    }
                }
                Ok(None) => {
                    // No packet available, continue polling
                }
                Err(e) => {
                    log::warn!("[AsyncNetworkBackend] Receive error: {}", e);
                }
            }

            // Update assigned IP (for relay-based networking)
            if let Some(ip) = backend.get_assigned_ip() {
                let mut guard = assigned_ip.lock().unwrap();
                if guard.is_none() {
                    log::info!(
                        "[AsyncNetworkBackend] IP assigned: {}.{}.{}.{}",
                        ip[0], ip[1], ip[2], ip[3]
                    );
                    *guard = Some(ip);
                }
            }
        }
    }

    /// Non-blocking receive.
    ///
    /// Returns the next available packet, or `None` if no packets are queued.
    /// This never blocks - packets are pre-fetched by the I/O thread.
    #[inline]
    pub fn try_receive(&self) -> Option<Vec<u8>> {
        self.rx.try_recv().ok()
    }

    /// Queue packet for sending.
    ///
    /// This is non-blocking - the packet is queued and the I/O thread will
    /// send it asynchronously. If the queue is full, the packet is dropped
    /// (similar to real network behavior under load).
    #[inline]
    pub fn send(&self, packet: Vec<u8>) {
        // Use send() which blocks if channel is full - but with unbounded channel
        // this won't happen in practice. For bounded channels, consider try_send().
        let _ = self.tx.send(packet);
    }

    /// Get the MAC address.
    #[inline]
    pub fn mac_address(&self) -> [u8; 6] {
        self.mac
    }

    /// Get the assigned IP address (if any).
    ///
    /// For relay-based networking, the relay assigns an IP to each client.
    /// Returns `None` until the IP is assigned.
    pub fn get_assigned_ip(&self) -> Option<[u8; 4]> {
        self.assigned_ip.lock().unwrap().clone()
    }
}

impl Drop for AsyncNetworkBackend {
    fn drop(&mut self) {
        log::debug!("[AsyncNetworkBackend] Shutting down");
        self.shutdown.store(true, Ordering::Release);
        // The I/O thread will exit on next iteration when it sees the shutdown flag
    }
}

/// Implement NetworkBackend trait so AsyncNetworkBackend can be used transparently
/// with VirtioNet. All operations are non-blocking.
impl NetworkBackend for AsyncNetworkBackend {
    fn init(&mut self) -> Result<(), String> {
        // Backend is already initialized in new()
        Ok(())
    }

    fn recv(&mut self) -> Result<Option<Vec<u8>>, String> {
        // Non-blocking receive via channel
        Ok(self.try_receive())
    }

    fn send(&self, buf: &[u8]) -> Result<(), String> {
        // Non-blocking send via channel
        self.send(buf.to_vec());
        Ok(())
    }

    fn mac_address(&self) -> [u8; 6] {
        self.mac
    }

    fn get_assigned_ip(&self) -> Option<[u8; 4]> {
        self.assigned_ip.lock().unwrap().clone()
    }

    fn receive_timeout(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>, String> {
        // Use recv_timeout on the channel for timeout support
        match self.rx.recv_timeout(timeout) {
            Ok(packet) => Ok(Some(packet)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                Err("Channel disconnected".to_string())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::DummyBackend;

    #[test]
    fn test_async_backend_creation() {
        let backend = Box::new(DummyBackend::new());
        let async_backend = AsyncNetworkBackend::new(backend);

        // Should get the MAC from the underlying backend
        let mac = async_backend.mac_address();
        assert_eq!(mac[0] & 0x02, 0x02); // Locally administered bit set
    }

    #[test]
    fn test_async_backend_try_receive_empty() {
        let backend = Box::new(DummyBackend::new());
        let async_backend = AsyncNetworkBackend::new(backend);

        // DummyBackend never receives packets, so try_receive should return None
        assert!(async_backend.try_receive().is_none());
    }

    #[test]
    fn test_async_backend_send() {
        let backend = Box::new(DummyBackend::new());
        let async_backend = AsyncNetworkBackend::new(backend);

        // Send should not panic or block
        async_backend.send(vec![0x00, 0x01, 0x02, 0x03]);

        // Give the I/O thread time to process
        std::thread::sleep(Duration::from_millis(50));
    }

    #[test]
    fn test_async_backend_shutdown() {
        let backend = Box::new(DummyBackend::new());
        let async_backend = AsyncNetworkBackend::new(backend);

        // Drop should trigger clean shutdown
        drop(async_backend);

        // If we get here without hanging, the test passes
    }
}

