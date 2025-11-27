//! WebTransport network backend with P2P relay protocol support.
//!
//! This backend tunnels Ethernet frames over WebTransport DATAGRAMs
//! using the relay protocol:
//! - 0x00 prefix: Control messages (JSON-encoded)
//! - 0x01 prefix: Ethernet data frames

use crate::net::NetworkBackend;
use std::sync::Arc;

/// Message type prefix for control messages
const MSG_TYPE_CONTROL: u8 = 0x00;
/// Message type prefix for Ethernet data frames
const MSG_TYPE_DATA: u8 = 0x01;

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

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{channel, Receiver, Sender};
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
    }

    impl WebTransportBackend {
        pub fn new(url: &str, cert_hash: Option<String>) -> Self {
            let mut mac = [0x52, 0x54, 0x00, 0x00, 0x00, 0x00];
            // Generate MAC from URL hash
            let hash: u32 = url
                .bytes()
                .fold(0u32, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u32));
            mac[3] = ((hash >> 16) & 0xff) as u8;
            mac[4] = ((hash >> 8) & 0xff) as u8;
            mac[5] = (hash & 0xff) as u8;

            let (tx_to_transport, rx_to_transport) = channel::<Vec<u8>>();
            let (tx_from_transport, rx_from_transport) = channel::<Vec<u8>>();

            let url = url.to_string();
            let mac_copy = mac;
            let registered = Arc::new(AtomicBool::new(false));
            let registered_clone = registered.clone();

            thread::spawn(move || {
                let rt = Runtime::new().unwrap();
                rt.block_on(async move {
                    let config = if let Some(hash_str) = cert_hash {
                        // Parse hex hash
                        let bytes =
                            hex::decode(hash_str.replace(":", "")).expect("Invalid hex hash");
                        let array: [u8; 32] = bytes.try_into().expect("Invalid hash length");
                        let digest = Sha256Digest::from(array);
                        ClientConfig::builder()
                            .with_bind_default()
                            .with_server_certificate_hashes(vec![digest])
                            .build()
                    } else {
                        ClientConfig::builder()
                            .with_bind_default()
                            .with_no_cert_validation()
                            .build()
                    };

                    let endpoint = Endpoint::client(config).unwrap();

                    log::info!("[WebTransport] Connecting to {}...", url);
                    let connection = match endpoint.connect(url).await {
                        Ok(conn) => conn,
                        Err(e) => {
                            log::error!("[WebTransport] Connection failed: {}", e);
                            return;
                        }
                    };
                    log::info!("[WebTransport] Connected!");

                    // Send registration message
                    let register_msg = make_register_message(&mac_copy);
                    if let Err(e) = connection.send_datagram(register_msg) {
                        log::error!("[WebTransport] Failed to send registration: {}", e);
                        return;
                    }
                    log::info!("[WebTransport] Registration sent, MAC: {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                        mac_copy[0], mac_copy[1], mac_copy[2], mac_copy[3], mac_copy[4], mac_copy[5]);

                    let connection = Arc::new(connection);
                    let conn_send = connection.clone();
                    let conn_recv = connection.clone();

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

                    // Receiver loop - decodes protocol and passes Ethernet frames to VM
                    loop {
                        match conn_recv.receive_datagram().await {
                            Ok(datagram) => {
                                let data = datagram.to_vec();
                                
                                // Check for Assigned message to confirm registration
                                if !data.is_empty() && data[0] == MSG_TYPE_CONTROL {
                                    if let Ok(json_str) = std::str::from_utf8(&data[1..]) {
                                        if json_str.contains("\"type\":\"Assigned\"") {
                                            registered_clone.store(true, Ordering::SeqCst);
                                            log::info!("[WebTransport] Registered with relay: {}", json_str);
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
    }

    // WASM is single threaded
    unsafe impl Send for WebTransportBackend {}

    impl WebTransportBackend {
        pub fn new(url: &str, cert_hash: Option<String>) -> Self {
            let mut mac = [0x52, 0x54, 0x00, 0x00, 0x00, 0x00];
            let hash: u32 = url
                .bytes()
                .fold(0u32, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u32));
            mac[3] = ((hash >> 16) & 0xff) as u8;
            mac[4] = ((hash >> 8) & 0xff) as u8;
            mac[5] = (hash & 0xff) as u8;

            Self {
                url: url.to_string(),
                cert_hash,
                mac,
                transport: None,
                writer: None,
                rx_queue: Rc::new(RefCell::new(VecDeque::new())),
                registered: Rc::new(RefCell::new(false)),
            }
        }

        /// Check if registered with the relay
        pub fn is_registered(&self) -> bool {
            *self.registered.borrow()
        }
    }

    impl NetworkBackend for WebTransportBackend {
        fn init(&mut self) -> Result<(), String> {
            let options = WebTransportOptions::new();

            if let Some(hash_hex) = &self.cert_hash {
                let bytes = hex::decode(hash_hex.replace(":", "")).map_err(|e| e.to_string())?;
                let array = Uint8Array::from(&bytes[..]);

                let hash_obj = WebTransportHash::new();
                hash_obj.set_algorithm("sha-256");
                hash_obj.set_value(&array);

                let hashes = Array::new();
                hashes.push(&hash_obj);
                options.set_server_certificate_hashes(&hashes);
            }

            let transport = WebTransport::new_with_options(&self.url, &options)
                .map_err(|e| format!("Failed to create WebTransport: {:?}", e))?;

            let rx_queue = self.rx_queue.clone();
            let registered = self.registered.clone();
            let mac = self.mac;

            // Setup writer first so we can send registration
            let datagrams = transport.datagrams();
            let writable = datagrams.writable();
            let writer = writable.get_writer().map_err(|e| format!("{:?}", e))?;

            let writer_clone = writer.clone();
            let transport_clone = transport.clone();
            let ready_promise = transport.ready();

            wasm_bindgen_futures::spawn_local(async move {
                match JsFuture::from(ready_promise).await {
                    Ok(_) => {
                        log::info!("WebTransport ready!");

                        // Send registration message
                        let register_msg = make_register_message(&mac);
                        let array = Uint8Array::from(&register_msg[..]);
                        if let Err(e) = writer_clone.write_with_chunk(&array) {
                            log::error!("Failed to send registration: {:?}", e);
                            return;
                        }
                        log::info!(
                            "Registration sent, MAC: {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                            mac[0],
                            mac[1],
                            mac[2],
                            mac[3],
                            mac[4],
                            mac[5]
                        );

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

                                    // Check for Assigned message
                                    if !data.is_empty() && data[0] == MSG_TYPE_CONTROL {
                                        if let Ok(json_str) = std::str::from_utf8(&data[1..]) {
                                            if json_str.contains("\"type\":\"Assigned\"") {
                                                *registered.borrow_mut() = true;
                                                log::info!(
                                                    "Registered with relay: {}",
                                                    json_str
                                                );
                                            }
                                        }
                                    }

                                    // Decode and queue Ethernet frames
                                    if let Some(ethernet_frame) = decode_message(&data) {
                                        rx_queue.borrow_mut().push_back(ethernet_frame);
                                    }
                                }
                                Err(e) => {
                                    log::error!("Read error: {:?}", e);
                                    break;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        log::error!("WebTransport failed to connect: {:?}", e);
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
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub use native::WebTransportBackend;

#[cfg(target_arch = "wasm32")]
pub use wasm::WebTransportBackend;
