//! WebTransport network backend with P2P relay protocol support.
//!
//! This backend tunnels Ethernet frames over WebTransport DATAGRAMs
//! using the relay protocol:
//! - 0x00 prefix: Control messages (JSON-encoded)
//! - 0x01 prefix: Ethernet data frames

use crate::net::NetworkBackend;

/// Message type prefix for control messages
const MSG_TYPE_CONTROL: u8 = 0x00;
/// Message type prefix for Ethernet data frames
const MSG_TYPE_DATA: u8 = 0x01;

/// Heartbeat interval in seconds
const HEARTBEAT_INTERVAL_SECS: u64 = 20;

/// Control message for registration
fn make_register_message(mac: &[u8; 6]) -> Vec<u8> {
    let json = format!(
        r#"{{"type":"Register","mac":[{},{},{},{},{},{}]}}"#,
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    );
    let mut msg = Vec::with_capacity(1 + json.len());
    msg.push(MSG_TYPE_CONTROL);
    msg.extend(json.bytes());
    msg
}

/// Control message for heartbeat
fn make_heartbeat_message() -> Vec<u8> {
    let json = r#"{"type":"Heartbeat"}"#;
    let mut msg = Vec::with_capacity(1 + json.len());
    msg.push(MSG_TYPE_CONTROL);
    msg.extend(json.bytes());
    msg
}

/// Encode an Ethernet frame with the data prefix
fn encode_data_frame(ethernet_frame: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(1 + ethernet_frame.len());
    frame.push(MSG_TYPE_DATA);
    frame.extend(ethernet_frame);
    frame
}

/// Decode a received message, stripping the type prefix for data frames
fn decode_message(data: &[u8]) -> Option<Vec<u8>> {
    if data.is_empty() {
        return None;
    }
    
    match data[0] {
        MSG_TYPE_DATA => {
            // Return the Ethernet frame without the prefix
            Some(data[1..].to_vec())
        }
        MSG_TYPE_CONTROL => {
            // Control messages are handled internally, not passed to the VM
            // Log assigned IP if present
            if let Ok(json_str) = std::str::from_utf8(&data[1..]) {
                if json_str.contains("\"type\":\"Assigned\"") {
                    log::info!("[WebTransport] Received IP assignment: {}", json_str);
                } else if json_str.contains("\"type\":\"HeartbeatAck\"") {
                    log::trace!("[WebTransport] Heartbeat acknowledged");
                } else if json_str.contains("\"type\":\"Error\"") {
                    log::error!("[WebTransport] Error from relay: {}", json_str);
                }
            }
            None
        }
        _ => {
            log::warn!("[WebTransport] Unknown message type: {}", data[0]);
            None
        }
    }
}

/// Parse IP address from JSON string containing "ip":[a,b,c,d]
fn parse_ip_from_json(json_str: &str) -> Option<[u8; 4]> {
    // Look for "ip":[ pattern
    let start_marker = "\"ip\":[";
    if let Some(start) = json_str.find(start_marker) {
        let rest = &json_str[start + start_marker.len()..];
        if let Some(end) = rest.find(']') {
            let ip_str = &rest[..end]; // e.g. "10,0,2,15"
            let parts: Vec<&str> = ip_str.split(',').collect();
            if parts.len() == 4 {
                let b0 = parts[0].trim().parse().ok()?;
                let b1 = parts[1].trim().parse().ok()?;
                let b2 = parts[2].trim().parse().ok()?;
                let b3 = parts[3].trim().parse().ok()?;
                return Some([b0, b1, b2, b3]);
            }
        }
    }
    None
}

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{channel, Receiver, Sender};
    use std::sync::Mutex;
    use std::thread;
    use std::time::Duration;
    use tokio::runtime::Runtime;
    use wtransport::tls::Sha256Digest;
    use wtransport::ClientConfig;
    use wtransport::Endpoint;

    pub struct WebTransportBackend {
        tx_to_transport: Option<Sender<Vec<u8>>>,
        rx_from_transport: Option<Receiver<Vec<u8>>>,
        mac: [u8; 6],
        registered: Arc<AtomicBool>,
        /// IP address assigned by the relay server
        assigned_ip: Arc<Mutex<Option<[u8; 4]>>>,
    }

    impl WebTransportBackend {
        pub fn new(url: &str, cert_hash: Option<String>) -> Self {
            eprintln!("[WebTransport] Creating backend for URL: {}", url);
            
            // Generate a random MAC address (locally administered, unicast)
            // Use system time + process id for randomness
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default();
            let nanos = now.as_nanos() as u64;
            let pid = std::process::id() as u64;
            
            // Mix in URL hash for additional entropy
            let url_hash: u64 = url
                .bytes()
                .fold(0u64, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u64));
            
            // Combine all sources of entropy
            let seed = nanos ^ (pid << 32) ^ url_hash;
            
            let mut mac = [0x52, 0x54, 0x00, 0x00, 0x00, 0x00];
            // Set locally administered bit (0x02) and clear multicast bit (0x01)
            mac[0] = 0x52; // Already has locally administered bit set
            mac[1] = 0x54;
            mac[2] = ((seed >> 40) & 0xff) as u8;
            mac[3] = ((seed >> 32) & 0xff) as u8;
            mac[4] = ((seed >> 16) & 0xff) as u8;
            mac[5] = (seed & 0xff) as u8;

            let (tx_to_transport, rx_to_transport) = channel::<Vec<u8>>();
            let (tx_from_transport, rx_from_transport) = channel::<Vec<u8>>();

            let url = url.to_string();
            let mac_copy = mac;
            let registered = Arc::new(AtomicBool::new(false));
            let registered_clone = registered.clone();
            let assigned_ip = Arc::new(Mutex::new(None));
            let assigned_ip_clone = assigned_ip.clone();

            thread::spawn(move || {
                let rt = Runtime::new().unwrap();
                rt.block_on(async move {
                    eprintln!("[WebTransport] Starting connection to {}...", url);
                    
                    let config = if let Some(hash_str) = cert_hash {
                        // Parse hex hash
                        eprintln!("[WebTransport] Using certificate hash: {}", hash_str);
                        let bytes = match hex::decode(hash_str.replace(":", "")) {
                            Ok(b) => b,
                            Err(e) => {
                                eprintln!("[WebTransport] ERROR: Invalid hex hash: {}", e);
                                return;
                            }
                        };
                        let bytes_len = bytes.len();
                        let array: [u8; 32] = match bytes.try_into() {
                            Ok(a) => a,
                            Err(_) => {
                                eprintln!("[WebTransport] ERROR: Hash must be 32 bytes, got {} bytes", bytes_len);
                                return;
                            }
                        };
                        let digest = Sha256Digest::from(array);
                        ClientConfig::builder()
                            .with_bind_default()
                            .with_server_certificate_hashes(vec![digest])
                            .build()
                    } else {
                        eprintln!("[WebTransport] WARNING: No certificate hash provided, disabling cert validation");
                        ClientConfig::builder()
                            .with_bind_default()
                            .with_no_cert_validation()
                            .build()
                    };

                    let endpoint = match Endpoint::client(config) {
                        Ok(ep) => ep,
                        Err(e) => {
                            eprintln!("[WebTransport] ERROR: Failed to create endpoint: {}", e);
                            return;
                        }
                    };

                    eprintln!("[WebTransport] Connecting to {}...", url);
                    let connection = match endpoint.connect(&url).await {
                        Ok(conn) => conn,
                        Err(e) => {
                            eprintln!("[WebTransport] ERROR: Connection failed: {}", e);
                            log::error!("[WebTransport] Connection failed: {}", e);
                            return;
                        }
                    };
                    eprintln!("[WebTransport] Connected successfully!");

                    // Send registration message
                    let register_msg = make_register_message(&mac_copy);
                    if let Err(e) = connection.send_datagram(register_msg) {
                        eprintln!("[WebTransport] ERROR: Failed to send registration: {}", e);
                        return;
                    }
                    eprintln!("[WebTransport] Registration sent, MAC: {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                        mac_copy[0], mac_copy[1], mac_copy[2], mac_copy[3], mac_copy[4], mac_copy[5]);

                    let connection = Arc::new(connection);
                    let conn_send = connection.clone();
                    let conn_recv = connection.clone();
                    let conn_heartbeat = connection.clone();

                    // Sender task - frames Ethernet data with protocol prefix
                    tokio::spawn(async move {
                        loop {
                            if let Ok(data) = rx_to_transport.try_recv() {
                                // Data from VM is already framed by send()
                                if let Err(e) = conn_send.send_datagram(data) {
                                    log::error!("Failed to send datagram: {}", e);
                                    break;
                                }
                            } else {
                                tokio::time::sleep(Duration::from_millis(1)).await;
                            }
                        }
                    });

                    // Heartbeat task - sends periodic heartbeats to keep connection alive
                    tokio::spawn(async move {
                        let mut interval = tokio::time::interval(Duration::from_secs(HEARTBEAT_INTERVAL_SECS));
                        loop {
                            interval.tick().await;
                            let heartbeat = make_heartbeat_message();
                            if let Err(e) = conn_heartbeat.send_datagram(heartbeat) {
                                log::warn!("[WebTransport] Failed to send heartbeat: {}", e);
                                break;
                            }
                            log::trace!("[WebTransport] Heartbeat sent");
                        }
                    });

                    // Receiver loop - decodes protocol and passes Ethernet frames to VM
                    loop {
                        match conn_recv.receive_datagram().await {
                            Ok(datagram) => {
                                let data = datagram.to_vec();
                                
                                // Check for Assigned message to confirm registration and extract IP
                                if !data.is_empty() && data[0] == MSG_TYPE_CONTROL {
                                    if let Ok(json_str) = std::str::from_utf8(&data[1..]) {
                                        if json_str.contains("\"type\":\"Assigned\"") {
                                            registered_clone.store(true, Ordering::SeqCst);
                                            
                                            // Parse IP from JSON: {"type":"Assigned","ip":[10,0,2,X],...}
                                            if let Some(ip) = parse_ip_from_json(json_str) {
                                                if let Ok(mut guard) = assigned_ip_clone.lock() {
                                                    *guard = Some(ip);
                                                }
                                                eprintln!("[WebTransport] IP Assigned: {}.{}.{}.{}", 
                                                    ip[0], ip[1], ip[2], ip[3]);
                                            }
                                            
                                            eprintln!("[WebTransport] Registered with relay: {}", json_str);
                                        }
                                    }
                                }
                                
                                // Decode and forward Ethernet frames
                                if let Some(ethernet_frame) = decode_message(&data) {
                                    let _ = tx_from_transport.send(ethernet_frame);
                                }
                            }
                            Err(e) => {
                                log::error!("Receive error: {}", e);
                                break;
                            }
                        }
                    }
                });
            });

            Self {
                tx_to_transport: Some(tx_to_transport),
                rx_from_transport: Some(rx_from_transport),
                mac,
                registered,
                assigned_ip,
            }
        }

        /// Check if registered with the relay
        pub fn is_registered(&self) -> bool {
            self.registered.load(Ordering::SeqCst)
        }
    }

    impl NetworkBackend for WebTransportBackend {
        fn init(&mut self) -> Result<(), String> {
            Ok(())
        }

        fn recv(&mut self) -> Result<Option<Vec<u8>>, String> {
            if let Some(rx) = &self.rx_from_transport {
                match rx.try_recv() {
                    Ok(data) => Ok(Some(data)),
                    Err(std::sync::mpsc::TryRecvError::Empty) => Ok(None),
                    Err(_) => Err("Disconnected".to_string()),
                }
            } else {
                Ok(None)
            }
        }

        fn send(&self, buf: &[u8]) -> Result<(), String> {
            if let Some(tx) = &self.tx_to_transport {
                // Frame the Ethernet data with the protocol prefix
                let framed = encode_data_frame(buf);
                tx.send(framed).map_err(|e| e.to_string())?;
                Ok(())
            } else {
                Err("Not connected".to_string())
            }
        }

        fn mac_address(&self) -> [u8; 6] {
            self.mac
        }
        
        fn get_assigned_ip(&self) -> Option<[u8; 4]> {
            if let Ok(guard) = self.assigned_ip.lock() {
                *guard
            } else {
                None
            }
        }
    }
}

#[cfg(target_arch = "wasm32")]
mod wasm {
    use super::*;
    use js_sys::{Array, Uint8Array};
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;
    use wasm_bindgen::prelude::*;
    use wasm_bindgen::JsCast;
    use wasm_bindgen_futures::JsFuture;
    use web_sys::{
        ReadableStreamDefaultReader, WebTransport, WebTransportHash, WebTransportOptions,
        WritableStreamDefaultWriter,
    };

    pub struct WebTransportBackend {
        url: String,
        cert_hash: Option<String>,
        mac: [u8; 6],
        transport: Option<WebTransport>,
        writer: Option<WritableStreamDefaultWriter>,
        rx_queue: Rc<RefCell<VecDeque<Vec<u8>>>>,
        registered: Rc<RefCell<bool>>,
        /// IP address assigned by the relay server
        assigned_ip: Rc<RefCell<Option<[u8; 4]>>>,
    }

    // WASM is single threaded
    unsafe impl Send for WebTransportBackend {}

    impl WebTransportBackend {
        pub fn new(url: &str, cert_hash: Option<String>) -> Self {
            // Generate a random MAC address using JS Math.random()
            // This ensures each browser tab/VM instance gets a unique MAC
            let rand1 = (js_sys::Math::random() * 0xFFFFFFFFu32 as f64) as u32;
            let rand2 = (js_sys::Math::random() * 0xFFFFu32 as f64) as u32;
            
            let mut mac = [0x52, 0x54, 0x00, 0x00, 0x00, 0x00];
            // Set locally administered bit (0x02) and clear multicast bit (0x01)
            mac[0] = 0x52; // Already has locally administered bit set
            mac[1] = 0x54;
            mac[2] = ((rand1 >> 24) & 0xff) as u8;
            mac[3] = ((rand1 >> 16) & 0xff) as u8;
            mac[4] = ((rand1 >> 8) & 0xff) as u8;
            mac[5] = (rand2 & 0xff) as u8;

            Self {
                url: url.to_string(),
                cert_hash,
                mac,
                transport: None,
                writer: None,
                rx_queue: Rc::new(RefCell::new(VecDeque::new())),
                registered: Rc::new(RefCell::new(false)),
                assigned_ip: Rc::new(RefCell::new(None)),
            }
        }

        /// Check if registered with the relay
        pub fn is_registered(&self) -> bool {
            *self.registered.borrow()
        }
    }

    // Helper to log to browser console
    fn console_log(msg: &str) {
        web_sys::console::log_1(&JsValue::from_str(msg));
    }
    
    fn console_error(msg: &str) {
        web_sys::console::error_1(&JsValue::from_str(msg));
    }

    impl NetworkBackend for WebTransportBackend {
        fn init(&mut self) -> Result<(), String> {
            console_log(&format!("[WebTransport] Initializing connection to {}", self.url));
            
            let options = WebTransportOptions::new();

            if let Some(hash_hex) = &self.cert_hash {
                console_log(&format!("[WebTransport] Using certificate hash: {}", hash_hex));
                let bytes = hex::decode(hash_hex.replace(":", "")).map_err(|e| {
                    console_error(&format!("[WebTransport] Invalid hex hash: {}", e));
                    e.to_string()
                })?;
                let array = Uint8Array::from(&bytes[..]);

                let hash_obj = WebTransportHash::new();
                hash_obj.set_algorithm("sha-256");
                hash_obj.set_value(&array);

                let hashes = Array::new();
                hashes.push(&hash_obj);
                options.set_server_certificate_hashes(&hashes);
            } else {
                console_log("[WebTransport] No certificate hash provided (using system trust)");
            }

            let transport = match WebTransport::new_with_options(&self.url, &options) {
                Ok(t) => t,
                Err(e) => {
                    let msg = format!("Failed to create WebTransport: {:?}", e);
                    console_error(&format!("[WebTransport] {}", msg));
                    return Err(msg);
                }
            };
            console_log("[WebTransport] Transport object created, waiting for connection...");

            let rx_queue = self.rx_queue.clone();
            let registered = self.registered.clone();
            let assigned_ip = self.assigned_ip.clone();
            let mac = self.mac;
            let url = self.url.clone();

            // Setup writer first so we can send registration
            let datagrams = transport.datagrams();
            let writable = datagrams.writable();
            let writer = writable.get_writer().map_err(|e| format!("{:?}", e))?;

            let writer_clone = writer.clone();
            let transport_clone = transport.clone();
            let ready_promise = transport.ready();

            wasm_bindgen_futures::spawn_local(async move {
                console_log(&format!("[WebTransport] Connecting to {}...", url));
                
                match JsFuture::from(ready_promise).await {
                    Ok(_) => {
                        console_log("[WebTransport] Connected successfully!");

                        // Send registration message
                        let register_msg = make_register_message(&mac);
                        let array = Uint8Array::from(&register_msg[..]);
                        let promise = writer_clone.write_with_chunk(&array);
                        if let Err(e) = JsFuture::from(promise).await {
                            console_error(&format!("[WebTransport] Failed to send registration: {:?}", e));
                            return;
                        }
                        console_log(&format!(
                            "[WebTransport] Registration sent, MAC: {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                            mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
                        ));

                        // Spawn heartbeat sender using JS setInterval via global scope
                        let writer_heartbeat = writer_clone.clone();
                        let heartbeat_closure = wasm_bindgen::closure::Closure::wrap(Box::new(move || {
                            let heartbeat = make_heartbeat_message();
                            let array = Uint8Array::from(&heartbeat[..]);
                            let _ = writer_heartbeat.write_with_chunk(&array);
                            log::trace!("[WebTransport] Heartbeat sent");
                        }) as Box<dyn Fn()>);
                        
                        // Use global setInterval via js_sys
                        let global = js_sys::global();
                        let set_interval = js_sys::Reflect::get(&global, &JsValue::from_str("setInterval"))
                            .expect("setInterval should exist");
                        let set_interval_fn: js_sys::Function = set_interval.unchecked_into();
                        let _ = set_interval_fn.call2(
                            &JsValue::NULL,
                            heartbeat_closure.as_ref(),
                            &JsValue::from((HEARTBEAT_INTERVAL_SECS * 1000) as i32),
                        );
                        heartbeat_closure.forget(); // Let the closure live forever

                        // Setup reader
                        let datagrams = transport_clone.datagrams();
                        let readable = datagrams.readable();
                        let reader: ReadableStreamDefaultReader =
                            readable.get_reader().unchecked_into();

                        loop {
                            match JsFuture::from(reader.read()).await {
                                Ok(result) => {
                                    let done = js_sys::Reflect::get(
                                        &result,
                                        &JsValue::from_str("done"),
                                    )
                                    .unwrap()
                                    .as_bool()
                                    .unwrap();
                                    if done {
                                        log::info!("WebTransport reader done");
                                        break;
                                    }
                                    let value = js_sys::Reflect::get(
                                        &result,
                                        &JsValue::from_str("value"),
                                    )
                                    .unwrap();
                                    let array = Uint8Array::new(&value);
                                    let data = array.to_vec();

                                    // Check for Assigned message and extract IP
                                    if !data.is_empty() && data[0] == MSG_TYPE_CONTROL {
                                        if let Ok(json_str) = std::str::from_utf8(&data[1..]) {
                                            if json_str.contains("\"type\":\"Assigned\"") {
                                                *registered.borrow_mut() = true;
                                                
                                                // Parse IP from JSON
                                                if let Some(ip) = parse_ip_from_json(json_str) {
                                                    *assigned_ip.borrow_mut() = Some(ip);
                                                    console_log(&format!(
                                                        "[WebTransport] IP Assigned: {}.{}.{}.{}",
                                                        ip[0], ip[1], ip[2], ip[3]
                                                    ));
                                                }
                                                
                                                console_log(&format!(
                                                    "[WebTransport] Registered with relay: {}",
                                                    json_str
                                                ));
                                            } else if json_str.contains("\"type\":\"HeartbeatAck\"") {
                                                // Heartbeat acknowledged - connection is alive
                                            } else if json_str.contains("\"type\":\"Error\"") {
                                                console_error(&format!("[WebTransport] Relay error: {}", json_str));
                                            }
                                        }
                                    }

                                    // Decode and queue Ethernet frames
                                    if let Some(ethernet_frame) = decode_message(&data) {
                                        rx_queue.borrow_mut().push_back(ethernet_frame);
                                    }
                                }
                                Err(e) => {
                                    console_error(&format!("[WebTransport] Read error: {:?}", e));
                                    break;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        console_error(&format!("[WebTransport] Failed to connect: {:?}", e));
                    }
                }
            });

            self.transport = Some(transport);
            self.writer = Some(writer);

            Ok(())
        }

        fn recv(&mut self) -> Result<Option<Vec<u8>>, String> {
            Ok(self.rx_queue.borrow_mut().pop_front())
        }

        fn send(&self, buf: &[u8]) -> Result<(), String> {
            if let Some(writer) = &self.writer {
                // Frame the Ethernet data with the protocol prefix
                let framed = encode_data_frame(buf);
                let array = Uint8Array::from(&framed[..]);
                let _ = writer.write_with_chunk(&array);
                Ok(())
            } else {
                Err("Not connected".to_string())
            }
        }

        fn mac_address(&self) -> [u8; 6] {
            self.mac
        }
        
        fn get_assigned_ip(&self) -> Option<[u8; 4]> {
            *self.assigned_ip.borrow()
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub use native::WebTransportBackend;

#[cfg(target_arch = "wasm32")]
pub use wasm::WebTransportBackend;
