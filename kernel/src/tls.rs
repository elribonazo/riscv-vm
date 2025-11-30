//! TLS 1.3 support for HTTPS connections using embedded-tls.
//!
//! This module provides full TLS 1.3 support for making secure HTTPS connections.
//! It uses the embedded-tls crate with AES-128-GCM-SHA256 cipher suite.
//!
//! ## Features
//! - TLS 1.3 handshake with modern cipher suites
//! - Blocking I/O wrapper for smoltcp TCP sockets
//! - Certificate verification disabled for development (NoVerify)
//!
//! ## Architecture
//! The TLS implementation uses a single-request-per-connection model:
//! 1. Establish TCP connection
//! 2. Perform TLS handshake
//! 3. Send HTTP request over TLS
//! 4. Receive HTTP response over TLS
//! 5. Close connection
//!
//! This matches the HTTP/1.1 "Connection: close" behavior and avoids
//! complex lifetime management in a no_std environment.

use alloc::vec::Vec;
use embedded_io::{ErrorType, Read, Write};

// Re-export embedded-tls types we use
pub use embedded_tls::blocking::{Aes128GcmSha256, TlsConfig, TlsContext, NoVerify, TlsConnection};
pub use embedded_tls::TlsError as EmbeddedTlsError;

// ═══════════════════════════════════════════════════════════════════════════════
// SIMPLE RNG - Using timer-based entropy
// ═══════════════════════════════════════════════════════════════════════════════

/// Simple RNG using CLINT timer as entropy source.
/// 
/// Note: This is NOT cryptographically secure in a production sense,
/// but provides functional randomness for TLS handshakes in our
/// bare-metal environment. For production use, consider adding
/// a hardware RNG or entropy accumulator.
pub struct SimpleRng {
    state: u64,
}

impl SimpleRng {
    pub fn new() -> Self {
        // Seed from timer
        const CLINT_MTIME: usize = 0x0200_BFF8;
        let seed = unsafe { core::ptr::read_volatile(CLINT_MTIME as *const u64) };
        // Mix in some additional entropy from multiple timer reads
        let mut state = seed ^ 0xdeadbeef_cafebabe;
        for _ in 0..10 {
            let t = unsafe { core::ptr::read_volatile(CLINT_MTIME as *const u64) };
            state = state.wrapping_mul(6364136223846793005).wrapping_add(t);
        }
        Self { state }
    }
    
    fn next_u64(&mut self) -> u64 {
        // xorshift128+ style PRNG for better quality
        let mut s = self.state;
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        self.state = s;
        s
    }
}

impl Default for SimpleRng {
    fn default() -> Self {
        Self::new()
    }
}

impl rand_core::RngCore for SimpleRng {
    fn next_u32(&mut self) -> u32 {
        self.next_u64() as u32
    }
    
    fn next_u64(&mut self) -> u64 {
        SimpleRng::next_u64(self)
    }
    
    fn fill_bytes(&mut self, dest: &mut [u8]) {
        let mut i = 0;
        while i < dest.len() {
            let r = self.next_u64().to_le_bytes();
            let remaining = dest.len() - i;
            let to_copy = remaining.min(8);
            dest[i..i + to_copy].copy_from_slice(&r[..to_copy]);
            i += to_copy;
        }
    }
    
    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core::Error> {
        self.fill_bytes(dest);
        Ok(())
    }
}

// Required for TLS - marks this as suitable for cryptographic use
// WARNING: In production, use a proper CSPRNG with hardware entropy
impl rand_core::CryptoRng for SimpleRng {}

// ═══════════════════════════════════════════════════════════════════════════════
// ERROR TYPES
// ═══════════════════════════════════════════════════════════════════════════════

/// Error type for TLS operations
#[derive(Debug, Clone, Copy)]
pub enum TlsError {
    /// TCP connection error
    ConnectionError,
    /// TLS handshake or protocol error
    TlsProtocolError,
    /// Operation timed out
    Timeout,
    /// Invalid data received
    InvalidData,
    /// I/O error
    Io,
    /// Connection closed
    ConnectionClosed,
    /// Handshake not completed
    NotConnected,
    /// DNS resolution failed
    DnsError,
    /// Internal error
    InternalError,
}

impl core::fmt::Display for TlsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            TlsError::ConnectionError => write!(f, "Connection error"),
            TlsError::TlsProtocolError => write!(f, "TLS protocol error"),
            TlsError::Timeout => write!(f, "Timeout"),
            TlsError::InvalidData => write!(f, "Invalid data"),
            TlsError::Io => write!(f, "I/O error"),
            TlsError::ConnectionClosed => write!(f, "Connection closed"),
            TlsError::NotConnected => write!(f, "Not connected"),
            TlsError::DnsError => write!(f, "DNS error"),
            TlsError::InternalError => write!(f, "Internal error"),
        }
    }
}

impl embedded_io::Error for TlsError {
    fn kind(&self) -> embedded_io::ErrorKind {
        match self {
            TlsError::ConnectionClosed => embedded_io::ErrorKind::ConnectionReset,
            TlsError::Timeout => embedded_io::ErrorKind::TimedOut,
            _ => embedded_io::ErrorKind::Other,
        }
    }
}

impl From<EmbeddedTlsError> for TlsError {
    fn from(e: EmbeddedTlsError) -> Self {
        match e {
            EmbeddedTlsError::ConnectionClosed => TlsError::ConnectionClosed,
            EmbeddedTlsError::IoError => TlsError::Io,
            EmbeddedTlsError::Io(_) => TlsError::Io,
            _ => TlsError::TlsProtocolError,
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// BLOCKING TCP SOCKET
// ═══════════════════════════════════════════════════════════════════════════════

/// Blocking TCP socket that implements embedded-io traits.
/// This allows embedded-tls to use our smoltcp-based TCP stack.
///
/// The socket holds mutable references to the network state and timing function,
/// and provides blocking read/write operations with timeout support.
pub struct BlockingTcpSocket<'a> {
    net: &'a mut crate::net::NetState,
    timeout_ms: i64,
    get_time: fn() -> i64,
    start_time: i64,
}

impl<'a> BlockingTcpSocket<'a> {
    /// Create a new blocking TCP socket wrapper.
    pub fn new(
        net: &'a mut crate::net::NetState,
        timeout_ms: i64,
        get_time: fn() -> i64,
    ) -> Self {
        let start_time = get_time();
        Self {
            net,
            timeout_ms,
            get_time,
            start_time,
        }
    }
    
    /// Reset the timeout timer (call after successful operations).
    pub fn reset_timeout(&mut self) {
        self.start_time = (self.get_time)();
    }
    
    /// Check if we've exceeded the timeout.
    fn check_timeout(&self) -> bool {
        let now = (self.get_time)();
        now - self.start_time > self.timeout_ms
    }
    
    /// Poll the network stack.
    fn poll_network(&mut self) {
        let now = (self.get_time)();
        self.net.poll(now);
    }
    
    /// Small delay to avoid busy-waiting.
    fn small_delay(&self) {
        for _ in 0..1000 {
            core::hint::spin_loop();
        }
    }
    
    /// Connect to a remote host (TCP only, no TLS).
    pub fn connect(&mut self, ip: smoltcp::wire::Ipv4Address, port: u16) -> Result<(), TlsError> {
        let now = (self.get_time)();
        self.net.tcp_connect(ip, port, now)
            .map_err(|_| TlsError::ConnectionError)?;
        
        // Wait for TCP connection to establish
        self.reset_timeout();
        loop {
            if self.check_timeout() {
                self.net.tcp_abort();
                return Err(TlsError::Timeout);
            }
            
            self.poll_network();
            
            if self.net.tcp_is_connected() {
                self.reset_timeout();
                return Ok(());
            }
            
            if self.net.tcp_connection_failed() {
                return Err(TlsError::ConnectionError);
            }
            
            self.small_delay();
        }
    }
    
    /// Close the TCP connection.
    pub fn close(&mut self) {
        let now = (self.get_time)();
        self.net.tcp_close(now);
    }
    
    /// Abort the TCP connection immediately.
    pub fn abort(&mut self) {
        self.net.tcp_abort();
    }
}

impl ErrorType for BlockingTcpSocket<'_> {
    type Error = TlsError;
}

impl Read for BlockingTcpSocket<'_> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        let mut poll_count = 0u32;
        loop {
            if self.check_timeout() {
                // Debug: Print TCP state on timeout
                crate::uart::write_str("TCP read timeout after ");
                let mut num_buf = [0u8; 12];
                let n = format_u32(poll_count, &mut num_buf);
                crate::uart::write_str(core::str::from_utf8(&num_buf[..n]).unwrap_or("?"));
                crate::uart::write_str(" polls, state=");
                crate::uart::write_line(self.net.tcp_state());
                return Err(TlsError::Timeout);
            }
            
            self.poll_network();
            poll_count += 1;
            
            let now = (self.get_time)();
            match self.net.tcp_recv(buf, now) {
                Ok(n) if n > 0 => {
                    self.reset_timeout();
                    return Ok(n);
                }
                Ok(_) => {
                    // No data available yet
                    if self.net.tcp_connection_failed() {
                        crate::uart::write_str("TCP connection failed, state=");
                        crate::uart::write_line(self.net.tcp_state());
                        return Err(TlsError::ConnectionClosed);
                    }
                    self.small_delay();
                }
                Err(e) => {
                    if e == "Connection closed by peer" {
                        return Err(TlsError::ConnectionClosed);
                    }
                    return Err(TlsError::ConnectionError);
                }
            }
        }
    }
}

/// Format u32 as decimal string
fn format_u32(mut n: u32, buf: &mut [u8]) -> usize {
    if n == 0 {
        buf[0] = b'0';
        return 1;
    }
    let mut i = 0;
    let mut tmp = [0u8; 10];
    while n > 0 {
        tmp[i] = b'0' + (n % 10) as u8;
        n /= 10;
        i += 1;
    }
    for j in 0..i {
        buf[j] = tmp[i - 1 - j];
    }
    i
}

impl Write for BlockingTcpSocket<'_> {
    fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        let mut total_sent = 0;
        
        while total_sent < buf.len() {
            if self.check_timeout() {
                return if total_sent > 0 {
                    Ok(total_sent)
                } else {
                    Err(TlsError::Timeout)
                };
            }
            
            self.poll_network();
            let now = (self.get_time)();
            
            match self.net.tcp_send(&buf[total_sent..], now) {
                Ok(n) if n > 0 => {
                    total_sent += n;
                    self.reset_timeout();
                }
                Ok(_) => {
                    // Buffer full, wait a bit
                    self.small_delay();
                }
                Err(_) => {
                    return if total_sent > 0 {
                        Ok(total_sent)
                    } else {
                        Err(TlsError::ConnectionError)
                    };
                }
            }
        }
        
        Ok(total_sent)
    }
    
    fn flush(&mut self) -> Result<(), Self::Error> {
        self.poll_network();
        Ok(())
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// TLS HTTPS REQUEST - High-level HTTPS API
// ═══════════════════════════════════════════════════════════════════════════════

/// Buffer sizes for TLS records.
/// Maximum TLS record size is 16KB, plus overhead for encryption.
/// Write buffer must be large enough for the TLS handshake (~2KB minimum).
const TLS_READ_BUFFER_SIZE: usize = 16640;
const TLS_WRITE_BUFFER_SIZE: usize = 8192;

/// Perform a complete HTTPS request and return the response.
///
/// This function handles the entire HTTPS request lifecycle:
/// 1. Establish TCP connection
/// 2. Perform TLS 1.3 handshake
/// 3. Send HTTP request over encrypted connection
/// 4. Receive and return HTTP response
/// 5. Close connection
///
/// # Arguments
/// * `net` - Network state
/// * `ip` - Server IP address
/// * `port` - Server port (typically 443)
/// * `hostname` - Server hostname (for SNI)
/// * `request_bytes` - Complete HTTP request as bytes
/// * `timeout_ms` - Timeout in milliseconds
/// * `get_time` - Function to get current time in milliseconds
///
/// # Returns
/// Response body as bytes on success, or TlsError on failure.
pub fn https_request(
    net: &mut crate::net::NetState,
    ip: smoltcp::wire::Ipv4Address,
    port: u16,
    hostname: &str,
    request_bytes: &[u8],
    timeout_ms: i64,
    get_time: fn() -> i64,
) -> Result<Vec<u8>, TlsError> {
    // Allocate TLS buffers
    let mut read_buffer = alloc::vec![0u8; TLS_READ_BUFFER_SIZE];
    let mut write_buffer = alloc::vec![0u8; TLS_WRITE_BUFFER_SIZE];
    let mut rng = SimpleRng::new();
    
    // Create blocking TCP socket and connect
    crate::uart::write_str("TLS: Connecting to port ");
    let mut port_buf = [0u8; 8];
    let port_len = format_u16(port, &mut port_buf);
    crate::uart::write_line(core::str::from_utf8(&port_buf[..port_len]).unwrap_or("?"));
    
    let mut socket = BlockingTcpSocket::new(net, timeout_ms, get_time);
    socket.connect(ip, port).map_err(|e| {
        crate::uart::write_line("TLS: TCP connection failed");
        e
    })?;
    
    crate::uart::write_line("TLS: TCP connected");
    
    // Create TLS config with SNI
    let config: TlsConfig<'_, Aes128GcmSha256> = TlsConfig::new()
        .with_server_name(hostname);
    
    // Create TLS connection wrapping our TCP socket
    let mut tls: TlsConnection<'_, BlockingTcpSocket<'_>, Aes128GcmSha256> = 
        TlsConnection::new(
            socket,
            &mut read_buffer,
            &mut write_buffer,
        );
    
    // Create context with config and RNG
    let context = TlsContext::new(&config, &mut rng);
    
    // Perform TLS 1.3 handshake
    crate::uart::write_str("TLS: Starting handshake with ");
    crate::uart::write_line(hostname);
    
    tls.open::<_, NoVerify>(context).map_err(|e| {
        crate::uart::write_str("TLS: Handshake failed - ");
        log_tls_error(&e);
        TlsError::from(e)
    })?;
    
    crate::uart::write_line("TLS: Handshake complete");
    
    // Send HTTP request over TLS
    let mut sent = 0;
    while sent < request_bytes.len() {
        match tls.write(&request_bytes[sent..]) {
            Ok(n) if n > 0 => sent += n,
            Ok(_) => {}
            Err(e) => {
                let _ = tls.close();
                return Err(TlsError::from(e));
            }
        }
    }
    
    // Flush to ensure all data is sent
    if let Err(e) = tls.flush() {
        let _ = tls.close();
        return Err(TlsError::from(e));
    }
    
    // Receive HTTP response over TLS
    let mut response_buf = Vec::with_capacity(8192);
    let mut recv_buf = [0u8; 1024];
    
    loop {
        match tls.read(&mut recv_buf) {
            Ok(0) => {
                // Connection closed, we have the full response
                break;
            }
            Ok(n) => {
                response_buf.extend_from_slice(&recv_buf[..n]);
                
                // Check if we've received a complete HTTP response
                if is_http_response_complete(&response_buf) {
                    break;
                }
            }
            Err(EmbeddedTlsError::ConnectionClosed) => {
                // Server closed connection, this is normal for Connection: close
                break;
            }
            Err(e) => {
                let _ = tls.close();
                return Err(TlsError::from(e));
            }
        }
    }
    
    // Close TLS connection cleanly
    let _ = tls.close();
    
    Ok(response_buf)
}

/// Check if we've received a complete HTTP response.
fn is_http_response_complete(data: &[u8]) -> bool {
    // Find end of headers
    let header_end = match find_header_end(data) {
        Some(pos) => pos,
        None => return false,
    };
    
    let body_start = header_end + 4;
    
    // Try to parse Content-Length
    if let Ok(headers_str) = core::str::from_utf8(&data[..header_end]) {
        for line in headers_str.lines() {
            let lower = line.to_lowercase();
            if lower.starts_with("content-length:") {
                if let Some(len_str) = line.split(':').nth(1) {
                    if let Ok(content_length) = len_str.trim().parse::<usize>() {
                        let body_len = data.len().saturating_sub(body_start);
                        return body_len >= content_length;
                    }
                }
            }
            // Check for Transfer-Encoding: chunked - harder to parse
            if lower.starts_with("transfer-encoding:") && lower.contains("chunked") {
                // For chunked, check if we have the final chunk marker
                return data.windows(5).any(|w| w == b"0\r\n\r\n");
            }
        }
    }
    
    // No Content-Length header, assume complete if we have headers + some body
    data.len() > body_start
}

/// Find the end of HTTP headers (double CRLF).
fn find_header_end(data: &[u8]) -> Option<usize> {
    for i in 0..data.len().saturating_sub(3) {
        if data[i] == b'\r' && data[i + 1] == b'\n' 
           && data[i + 2] == b'\r' && data[i + 3] == b'\n' {
            return Some(i);
        }
    }
    None
}

/// Log TLS error details for debugging.
fn log_tls_error(e: &EmbeddedTlsError) {
    crate::uart::write_str("TLS error: ");
    match e {
        EmbeddedTlsError::HandshakeAborted(level, desc) => {
            crate::uart::write_str("Handshake aborted (level=");
            let mut buf = [0u8; 20];
            let n = format_u8(*level as u8, &mut buf);
            crate::uart::write_str(core::str::from_utf8(&buf[..n]).unwrap_or("?"));
            crate::uart::write_str(", desc=");
            let n = format_u8(*desc as u8, &mut buf);
            crate::uart::write_str(core::str::from_utf8(&buf[..n]).unwrap_or("?"));
            crate::uart::write_line(")");
        }
        EmbeddedTlsError::InvalidCertificate => crate::uart::write_line("Invalid certificate"),
        EmbeddedTlsError::InvalidSignature => crate::uart::write_line("Invalid signature"),
        EmbeddedTlsError::InvalidHandshake => crate::uart::write_line("Invalid handshake (server may not support TLS 1.3)"),
        EmbeddedTlsError::InvalidRecord => crate::uart::write_line("Invalid record"),
        EmbeddedTlsError::InvalidSupportedVersions => crate::uart::write_line("Invalid supported versions (server may not support TLS 1.3)"),
        EmbeddedTlsError::ConnectionClosed => crate::uart::write_line("Connection closed by server"),
        EmbeddedTlsError::IoError => crate::uart::write_line("I/O error"),
        EmbeddedTlsError::DecodeError => crate::uart::write_line("Decode error (incompatible TLS version?)"),
        EmbeddedTlsError::Io(k) => {
            crate::uart::write_str("I/O: ");
            crate::uart::write_line(match k {
                embedded_io::ErrorKind::Other => "Other",
                embedded_io::ErrorKind::NotFound => "NotFound",
                embedded_io::ErrorKind::PermissionDenied => "PermissionDenied",
                embedded_io::ErrorKind::ConnectionRefused => "ConnectionRefused",
                embedded_io::ErrorKind::ConnectionReset => "ConnectionReset",
                embedded_io::ErrorKind::ConnectionAborted => "ConnectionAborted",
                embedded_io::ErrorKind::NotConnected => "NotConnected",
                embedded_io::ErrorKind::AddrInUse => "AddrInUse",
                embedded_io::ErrorKind::AddrNotAvailable => "AddrNotAvailable",
                embedded_io::ErrorKind::BrokenPipe => "BrokenPipe",
                embedded_io::ErrorKind::AlreadyExists => "AlreadyExists",
                embedded_io::ErrorKind::InvalidInput => "InvalidInput",
                embedded_io::ErrorKind::InvalidData => "InvalidData",
                embedded_io::ErrorKind::TimedOut => "TimedOut",
                embedded_io::ErrorKind::Interrupted => "Interrupted",
                embedded_io::ErrorKind::Unsupported => "Unsupported",
                embedded_io::ErrorKind::OutOfMemory => "OutOfMemory",
                _ => "Unknown",
            });
        }
        _ => crate::uart::write_line("Unknown error (check TLS 1.3 compatibility)"),
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// HTTPS GET HELPER
// ═══════════════════════════════════════════════════════════════════════════════

/// Perform an HTTPS GET request.
///
/// This is a convenience function that builds the HTTP request and calls `https_request`.
///
/// # Arguments
/// * `net` - Network state
/// * `hostname` - Server hostname (used for DNS and SNI)
/// * `ip` - Server IP (if already resolved)
/// * `port` - Server port (typically 443)
/// * `path` - Request path (e.g., "/api/data")
/// * `timeout_ms` - Timeout in milliseconds
/// * `get_time` - Function to get current time in milliseconds
///
/// # Returns
/// Response bytes on success, or TlsError on failure.
pub fn https_get(
    net: &mut crate::net::NetState,
    hostname: &str,
    ip: smoltcp::wire::Ipv4Address,
    port: u16,
    path: &str,
    timeout_ms: i64,
    get_time: fn() -> i64,
) -> Result<Vec<u8>, TlsError> {
    // Build HTTP GET request
    let request = alloc::format!(
        "GET {} HTTP/1.1\r\n\
         Host: {}\r\n\
         User-Agent: BAVY OS/{}\r\n\
         Accept: */*\r\n\
         Connection: close\r\n\
         \r\n",
        path, hostname, env!("CARGO_PKG_VERSION")
    );
    
    https_request(net, ip, port, hostname, request.as_bytes(), timeout_ms, get_time)
}

/// Resolve hostname and perform HTTPS GET request.
///
/// This function handles DNS resolution before making the request.
pub fn https_get_url(
    net: &mut crate::net::NetState,
    hostname: &str,
    port: u16,
    path: &str,
    timeout_ms: i64,
    get_time: fn() -> i64,
) -> Result<Vec<u8>, TlsError> {
    // Try to parse as IP first
    if let Some(ip) = crate::net::parse_ipv4(hostname.as_bytes()) {
        return https_get(net, hostname, ip, port, path, timeout_ms, get_time);
    }
    
    // Resolve via DNS
    let ip = crate::dns::resolve(
        net, 
        hostname.as_bytes(), 
        crate::net::DNS_SERVER, 
        timeout_ms, 
        get_time
    ).ok_or(TlsError::DnsError)?;
    
    https_get(net, hostname, ip, port, path, timeout_ms, get_time)
}

// ═══════════════════════════════════════════════════════════════════════════════
// PUBLIC API
// ═══════════════════════════════════════════════════════════════════════════════

/// Check if TLS/HTTPS is available.
///
/// Returns true if TLS 1.3 support is compiled in and functional.
pub fn is_available() -> bool {
    true
}

/// Get TLS status message.
pub fn status() -> &'static str {
    "TLS 1.3 only (AES-128-GCM-SHA256, no cert verification)"
}

// ═══════════════════════════════════════════════════════════════════════════════
// UTILITY FUNCTIONS
// ═══════════════════════════════════════════════════════════════════════════════

/// Format a u8 as decimal string.
pub fn format_u8(n: u8, buf: &mut [u8]) -> usize {
    if n >= 100 {
        buf[0] = b'0' + (n / 100);
        buf[1] = b'0' + ((n / 10) % 10);
        buf[2] = b'0' + (n % 10);
        3
    } else if n >= 10 {
        buf[0] = b'0' + (n / 10);
        buf[1] = b'0' + (n % 10);
        2
    } else {
        buf[0] = b'0' + n;
        1
    }
}

/// Format a u16 as decimal string.
pub fn format_u16(mut n: u16, buf: &mut [u8]) -> usize {
    if n == 0 {
        buf[0] = b'0';
        return 1;
    }
    let mut i = 0;
    let mut tmp = [0u8; 5];
    while n > 0 {
        tmp[i] = b'0' + (n % 10) as u8;
        n /= 10;
        i += 1;
    }
    // Reverse into buf
    for j in 0..i {
        buf[j] = tmp[i - 1 - j];
    }
    i
}

