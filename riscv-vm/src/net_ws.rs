//! WebSocket network backend for cross-platform networking.
//!
//! This backend tunnels Ethernet frames over WebSocket, enabling
//! networking on platforms without TAP support (macOS, WASM/browser).

use crate::net::NetworkBackend;
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use super::*;
    use std::thread;
    use tungstenite::{connect, Message, WebSocket};
    use tungstenite::stream::MaybeTlsStream;
    use std::net::TcpStream;

    /// WebSocket backend for native platforms (macOS, Linux, Windows).
    pub struct WsBackend {
        url: String,
        mac: [u8; 6],
        tx_to_ws: Option<Sender<Vec<u8>>>,
        rx_from_ws: Option<Receiver<Vec<u8>>>,
        connected: Arc<Mutex<bool>>,
        error_message: Arc<Mutex<Option<String>>>,
    }

    impl WsBackend {
        pub fn new(url: &str) -> Self {
            // Generate a random-ish MAC based on URL hash
            let mut mac = [0x52, 0x54, 0x00, 0x00, 0x00, 0x00];
            let hash: u32 = url.bytes().fold(0u32, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u32));
            mac[3] = ((hash >> 16) & 0xff) as u8;
            mac[4] = ((hash >> 8) & 0xff) as u8;
            mac[5] = (hash & 0xff) as u8;
            
            Self {
                url: url.to_string(),
                mac,
                tx_to_ws: None,
                rx_from_ws: None,
                connected: Arc::new(Mutex::new(false)),
                error_message: Arc::new(Mutex::new(None)),
            }
        }
        
        /// Check if the backend is currently connected.
        pub fn is_connected(&self) -> bool {
            *self.connected.lock().unwrap()
        }
        
        /// Get any error message from the connection.
        pub fn error_message(&self) -> Option<String> {
            self.error_message.lock().unwrap().clone()
        }

        /// Reader thread - reads from WebSocket and sends to channel
        fn reader_thread(
            mut socket: WebSocket<MaybeTlsStream<TcpStream>>,
            tx_received: Sender<Vec<u8>>,
            rx_to_send: Receiver<Vec<u8>>,
            connected: Arc<Mutex<bool>>,
        ) {
            // Set socket to blocking mode for reliable reads
            if let MaybeTlsStream::Plain(ref stream) = *socket.get_ref() {
                let _ = stream.set_nonblocking(false);
                // Set a read timeout so we can also check for outgoing messages
                let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(10)));
            }
            
            loop {
                // First, check if we have outgoing messages to send
                loop {
                    match rx_to_send.try_recv() {
                        Ok(data) => {
                            log::trace!("[WsBackend] Sending {} bytes", data.len());
                            if let Err(e) = socket.send(Message::Binary(data.into())) {
                                log::warn!("[WsBackend] Send error: {}", e);
                                *connected.lock().unwrap() = false;
                                return;
                            }
                        }
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => {
                            log::info!("[WsBackend] Send channel closed, exiting");
                            *connected.lock().unwrap() = false;
                            return;
                        }
                    }
                }
                
                // Flush any pending writes
                if let Err(e) = socket.flush() {
                    if !matches!(e, tungstenite::Error::Io(ref io) if io.kind() == std::io::ErrorKind::WouldBlock) {
                        log::warn!("[WsBackend] Flush error: {}", e);
                    }
                }
                
                // Try to read from WebSocket
                match socket.read() {
                    Ok(Message::Binary(data)) => {
                        log::debug!("[WsBackend] Received {} bytes from relay", data.len());
                        if tx_received.send(data.into()).is_err() {
                            log::info!("[WsBackend] Receiver dropped, exiting");
                            *connected.lock().unwrap() = false;
                            return;
                        }
                    }
                    Ok(Message::Close(_)) => {
                        log::info!("[WsBackend] WebSocket closed by server");
                        *connected.lock().unwrap() = false;
                        return;
                    }
                    Ok(Message::Ping(data)) => {
                        // Respond to ping with pong
                        let _ = socket.send(Message::Pong(data));
                    }
                    Ok(_) => {} // Ignore text, pong
                    Err(tungstenite::Error::Io(ref e)) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        // Timeout - no data available, continue loop
                    }
                    Err(tungstenite::Error::Io(ref e)) if e.kind() == std::io::ErrorKind::TimedOut => {
                        // Timeout - no data available, continue loop
                    }
                    Err(e) => {
                        log::warn!("[WsBackend] Read error: {}", e);
                        *connected.lock().unwrap() = false;
                        return;
                    }
                }
            }
        }
    }

    impl NetworkBackend for WsBackend {
        fn init(&mut self) -> Result<(), String> {
            log::info!("[WsBackend] Connecting to {}", self.url);
            
            let (socket, _response) = connect(&self.url)
                .map_err(|e| {
                    let msg = format!("Failed to connect: {}", e);
                    log::error!("[WsBackend] {}", msg);
                    *self.error_message.lock().unwrap() = Some(msg.clone());
                    msg
                })?;
            
            log::info!("[WsBackend] Connected successfully!");
            *self.connected.lock().unwrap() = true;
            *self.error_message.lock().unwrap() = None;
            
            let (tx_to_ws, rx_to_send) = channel();
            let (tx_received, rx_from_ws) = channel();
            
            self.tx_to_ws = Some(tx_to_ws);
            self.rx_from_ws = Some(rx_from_ws);
            
            let connected = self.connected.clone();
            
            thread::spawn(move || {
                Self::reader_thread(socket, tx_received, rx_to_send, connected);
            });
            
            log::info!("[WsBackend] Initialized successfully");
            Ok(())
        }

        fn recv(&mut self) -> Result<Option<Vec<u8>>, String> {
            if let Some(ref rx) = self.rx_from_ws {
                match rx.try_recv() {
                    Ok(data) => {
                        log::trace!("[WsBackend] recv() returning {} bytes", data.len());
                        Ok(Some(data))
                    }
                    Err(TryRecvError::Empty) => Ok(None),
                    Err(TryRecvError::Disconnected) => {
                        Err("WebSocket disconnected".to_string())
                    }
                }
            } else {
                Ok(None)
            }
        }

        fn send(&self, buf: &[u8]) -> Result<(), String> {
            if !*self.connected.lock().unwrap() {
                return Err("Not connected".to_string());
            }
            
            if let Some(ref tx) = self.tx_to_ws {
                tx.send(buf.to_vec()).map_err(|e| format!("Send failed: {}", e))
            } else {
                Err("WebSocket not initialized".to_string())
            }
        }

        fn mac_address(&self) -> [u8; 6] {
            self.mac
        }
    }
}

#[cfg(target_arch = "wasm32")]
mod wasm {
    use super::*;
    use wasm_bindgen::prelude::*;
    use wasm_bindgen::JsCast;
    use web_sys::{WebSocket, MessageEvent, ErrorEvent, CloseEvent, BinaryType};
    use js_sys::{ArrayBuffer, Uint8Array};
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::collections::VecDeque;

    /// Maximum number of packets to buffer before dropping
    const MAX_RX_QUEUE_SIZE: usize = 256;
    
    /// Connection state for tracking
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub enum ConnectionState {
        Disconnected,
        Connecting,
        Connected,
        Error,
    }

    /// WebSocket backend for WASM/browser.
    pub struct WsBackend {
        url: String,
        mac: [u8; 6],
        ws: Option<WebSocket>,
        rx_queue: Rc<RefCell<VecDeque<Vec<u8>>>>,
        state: Rc<RefCell<ConnectionState>>,
        error_message: Rc<RefCell<Option<String>>>,
        packets_dropped: Rc<RefCell<u64>>,
    }

    // WASM types are !Send by default, but we need Send for NetworkBackend.
    // This is safe because WASM is single-threaded.
    unsafe impl Send for WsBackend {}

    impl WsBackend {
        pub fn new(url: &str) -> Self {
            let mut mac = [0x52, 0x54, 0x00, 0x00, 0x00, 0x00];
            let hash: u32 = url.bytes().fold(0u32, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u32));
            mac[3] = ((hash >> 16) & 0xff) as u8;
            mac[4] = ((hash >> 8) & 0xff) as u8;
            mac[5] = (hash & 0xff) as u8;
            
            Self {
                url: url.to_string(),
                mac,
                ws: None,
                rx_queue: Rc::new(RefCell::new(VecDeque::with_capacity(MAX_RX_QUEUE_SIZE))),
                state: Rc::new(RefCell::new(ConnectionState::Disconnected)),
                error_message: Rc::new(RefCell::new(None)),
                packets_dropped: Rc::new(RefCell::new(0)),
            }
        }
        
        /// Check if the backend is currently connected.
        pub fn is_connected(&self) -> bool {
            *self.state.borrow() == ConnectionState::Connected
        }
        
        /// Get the current connection state.
        pub fn connection_state(&self) -> ConnectionState {
            *self.state.borrow()
        }
        
        /// Get any error message.
        pub fn error_message(&self) -> Option<String> {
            self.error_message.borrow().clone()
        }
        
        /// Get the number of packets dropped due to buffer overflow.
        pub fn packets_dropped(&self) -> u64 {
            *self.packets_dropped.borrow()
        }
    }

    impl NetworkBackend for WsBackend {
        fn init(&mut self) -> Result<(), String> {
            *self.state.borrow_mut() = ConnectionState::Connecting;
            *self.error_message.borrow_mut() = None;
            
            let ws = WebSocket::new(&self.url)
                .map_err(|e| {
                    *self.state.borrow_mut() = ConnectionState::Error;
                    let msg = format!("Failed to create WebSocket: {:?}", e);
                    *self.error_message.borrow_mut() = Some(msg.clone());
                    msg
                })?;
            
            ws.set_binary_type(BinaryType::Arraybuffer);
            
            // Set up message handler with buffer overflow protection
            let rx_queue = self.rx_queue.clone();
            let packets_dropped = self.packets_dropped.clone();
            let onmessage_callback = Closure::<dyn FnMut(_)>::new(move |e: MessageEvent| {
                if let Ok(abuf) = e.data().dyn_into::<ArrayBuffer>() {
                    let array = Uint8Array::new(&abuf);
                    let mut data = vec![0u8; array.length() as usize];
                    array.copy_to(&mut data);
                    
                    let mut queue = rx_queue.borrow_mut();
                    // Handle buffer overflow - drop oldest packets if queue is full
                    while queue.len() >= MAX_RX_QUEUE_SIZE {
                        queue.pop_front();
                        *packets_dropped.borrow_mut() += 1;
                    }
                    queue.push_back(data);
                }
            });
            ws.set_onmessage(Some(onmessage_callback.as_ref().unchecked_ref()));
            onmessage_callback.forget();
            
            // Set up open handler
            let state_open = self.state.clone();
            let error_clear = self.error_message.clone();
            let onopen_callback = Closure::<dyn FnMut()>::new(move || {
                *state_open.borrow_mut() = ConnectionState::Connected;
                *error_clear.borrow_mut() = None;
                log::info!("[WsBackend] WebSocket connected!");
            });
            ws.set_onopen(Some(onopen_callback.as_ref().unchecked_ref()));
            onopen_callback.forget();
            
            // Set up error handler
            let state_error = self.state.clone();
            let error_message = self.error_message.clone();
            let onerror_callback = Closure::<dyn FnMut(_)>::new(move |e: ErrorEvent| {
                *state_error.borrow_mut() = ConnectionState::Error;
                let msg = format!("WebSocket error: {}", e.message());
                *error_message.borrow_mut() = Some(msg.clone());
                log::error!("[WsBackend] {}", msg);
            });
            ws.set_onerror(Some(onerror_callback.as_ref().unchecked_ref()));
            onerror_callback.forget();
            
            // Set up close handler
            let state_close = self.state.clone();
            let onclose_callback = Closure::<dyn FnMut(_)>::new(move |e: CloseEvent| {
                *state_close.borrow_mut() = ConnectionState::Disconnected;
                log::info!("[WsBackend] WebSocket closed (code: {}, reason: {})", 
                    e.code(), e.reason());
            });
            ws.set_onclose(Some(onclose_callback.as_ref().unchecked_ref()));
            onclose_callback.forget();
            
            self.ws = Some(ws);
            
            log::info!("[WsBackend] Initialized, connecting to {}", self.url);
            Ok(())
        }

        fn recv(&mut self) -> Result<Option<Vec<u8>>, String> {
            // Check connection state
            let state = *self.state.borrow();
            if state == ConnectionState::Error {
                if let Some(msg) = self.error_message.borrow().clone() {
                    return Err(msg);
                }
                return Err("WebSocket in error state".to_string());
            }
            
            Ok(self.rx_queue.borrow_mut().pop_front())
        }

        fn send(&self, buf: &[u8]) -> Result<(), String> {
            let state = *self.state.borrow();
            
            if state != ConnectionState::Connected {
                // Silently drop if not connected (don't spam errors)
                return Ok(());
            }
            
            if let Some(ref ws) = self.ws {
                let array = Uint8Array::from(buf);
                ws.send_with_array_buffer(&array.buffer())
                    .map_err(|e| format!("Send failed: {:?}", e))
            } else {
                Err("WebSocket not initialized".to_string())
            }
        }

        fn mac_address(&self) -> [u8; 6] {
            self.mac
        }
    }
}

// Re-export the appropriate backend based on target
#[cfg(not(target_arch = "wasm32"))]
pub use native::WsBackend;

#[cfg(target_arch = "wasm32")]
pub use wasm::WsBackend;
