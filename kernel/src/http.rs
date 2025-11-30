//! HTTP client implementation for making HTTP requests.
//!
//! This module provides a simple HTTP/1.1 client that supports:
//! - GET, POST, PUT, DELETE methods
//! - Custom headers
//! - Response parsing with status, headers, and body
//! - Automatic redirect following (301, 302, 303, 307, 308)

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use alloc::collections::BTreeMap;
use alloc::format;
use smoltcp::wire::Ipv4Address;

/// Maximum number of redirects to follow before giving up
const MAX_REDIRECTS: u8 = 10;

/// HTTP methods
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Delete,
    Head,
}

impl HttpMethod {
    pub fn as_str(&self) -> &'static str {
        match self {
            HttpMethod::Get => "GET",
            HttpMethod::Post => "POST",
            HttpMethod::Put => "PUT",
            HttpMethod::Delete => "DELETE",
            HttpMethod::Head => "HEAD",
        }
    }
}

/// HTTP request builder
pub struct HttpRequest {
    pub method: HttpMethod,
    pub host: String,
    pub path: String,
    pub port: u16,
    pub headers: BTreeMap<String, String>,
    pub body: Option<Vec<u8>>,
    pub is_https: bool,
}

impl HttpRequest {
    /// Create a new GET request
    pub fn get(url: &str) -> Result<Self, &'static str> {
        Self::new(HttpMethod::Get, url)
    }
    
    /// Create a new POST request
    pub fn post(url: &str) -> Result<Self, &'static str> {
        Self::new(HttpMethod::Post, url)
    }
    
    /// Create a new request with the given method
    pub fn new(method: HttpMethod, url: &str) -> Result<Self, &'static str> {
        let parsed = parse_url(url)?;
        
        let mut headers = BTreeMap::new();
        headers.insert("Host".to_string(), parsed.host.clone());
        headers.insert("User-Agent".to_string(), format!("BAVY OS/{}", env!("CARGO_PKG_VERSION")));
        headers.insert("Accept".to_string(), "*/*".to_string());
        headers.insert("Connection".to_string(), "close".to_string());
        
        Ok(HttpRequest {
            method,
            host: parsed.host,
            path: parsed.path,
            port: parsed.port,
            headers,
            body: None,
            is_https: parsed.is_https,
        })
    }
    
    /// Set a header
    pub fn header(mut self, key: &str, value: &str) -> Self {
        self.headers.insert(key.to_string(), value.to_string());
        self
    }
    
    /// Set the request body
    pub fn body(mut self, body: Vec<u8>) -> Self {
        let len = body.len();
        self.body = Some(body);
        self.headers.insert("Content-Length".to_string(), len.to_string());
        self
    }
    
    /// Set the request body as a string
    pub fn body_str(self, body: &str) -> Self {
        self.body(body.as_bytes().to_vec())
    }
    
    /// Build the HTTP request bytes
    pub fn build(&self) -> Vec<u8> {
        let mut request = format!(
            "{} {} HTTP/1.1\r\n",
            self.method.as_str(),
            self.path
        );
        
        for (key, value) in &self.headers {
            request.push_str(key);
            request.push_str(": ");
            request.push_str(value);
            request.push_str("\r\n");
        }
        
        request.push_str("\r\n");
        
        let mut bytes = request.into_bytes();
        
        if let Some(ref body) = self.body {
            bytes.extend_from_slice(body);
        }
        
        bytes
    }
}

/// HTTP response
#[derive(Debug)]
pub struct HttpResponse {
    pub status_code: u16,
    pub status_text: String,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// Get body as UTF-8 string
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
    
    /// Check if response is successful (2xx)
    pub fn is_success(&self) -> bool {
        self.status_code >= 200 && self.status_code < 300
    }
    
    /// Check if response is redirect (3xx)
    pub fn is_redirect(&self) -> bool {
        self.status_code >= 300 && self.status_code < 400
    }
    
    /// Get a header value (case-insensitive)
    pub fn header(&self, name: &str) -> Option<&String> {
        let lower = name.to_lowercase();
        self.headers.iter()
            .find(|(k, _)| k.to_lowercase() == lower)
            .map(|(_, v)| v)
    }
    
    /// Get content length from headers
    pub fn content_length(&self) -> Option<usize> {
        self.header("content-length")
            .and_then(|v| v.parse().ok())
    }
    
    /// Get redirect location if this is a redirect response
    pub fn redirect_location(&self) -> Option<&String> {
        if self.is_redirect() {
            self.header("location")
        } else {
            None
        }
    }
}

/// URL parsing result
pub struct ParsedUrl {
    pub host: String,
    pub port: u16,
    pub path: String,
    pub is_https: bool,
}

/// Parse URL into (host, port, path, is_https)
fn parse_url(url: &str) -> Result<ParsedUrl, &'static str> {
    // Detect protocol and strip prefix
    let (url, is_https, default_port) = if url.starts_with("https://") {
        (&url[8..], true, 443u16)
    } else if url.starts_with("http://") {
        (&url[7..], false, 80u16)
    } else {
        (url, false, 80u16)
    };
    
    // Split host and path
    let (host_port, path) = match url.find('/') {
        Some(idx) => (&url[..idx], &url[idx..]),
        None => (url, "/"),
    };
    
    // Parse host and port
    let (host, port) = match host_port.find(':') {
        Some(idx) => {
            let port_str = &host_port[idx + 1..];
            let port: u16 = port_str.parse().map_err(|_| "Invalid port number")?;
            (&host_port[..idx], port)
        }
        None => (host_port, default_port),
    };
    
    Ok(ParsedUrl {
        host: host.to_string(),
        port,
        path: path.to_string(),
        is_https,
    })
}

/// Resolve a redirect URL relative to the original request
/// 
/// Handles:
/// - Absolute URLs (http://example.com/path or https://...)
/// - Protocol-relative URLs (//example.com/path)  
/// - Absolute paths (/path/to/resource)
/// - Relative paths (path/to/resource)
fn resolve_redirect_url(original: &HttpRequest, location: &str) -> Result<String, &'static str> {
    // Absolute URL
    if location.starts_with("http://") || location.starts_with("https://") {
        return Ok(location.to_string());
    }
    
    // Protocol-relative URL
    if location.starts_with("//") {
        let protocol = if original.is_https { "https:" } else { "http:" };
        return Ok(format!("{}{}", protocol, location));
    }
    
    // Build base URL from original request
    let protocol = if original.is_https { "https" } else { "http" };
    let port_str = if (original.is_https && original.port == 443) || 
                      (!original.is_https && original.port == 80) {
        String::new()
    } else {
        format!(":{}", original.port)
    };
    
    // Absolute path
    if location.starts_with('/') {
        return Ok(format!("{}://{}{}{}", protocol, original.host, port_str, location));
    }
    
    // Relative path - resolve against current path
    let base_path = if let Some(last_slash) = original.path.rfind('/') {
        &original.path[..=last_slash]
    } else {
        "/"
    };
    
    Ok(format!("{}://{}{}{}{}", protocol, original.host, port_str, base_path, location))
}

/// Parse raw HTTP response bytes into HttpResponse
pub fn parse_response(data: &[u8]) -> Result<HttpResponse, &'static str> {
    // Convert to string for easier parsing
    let response_str = core::str::from_utf8(data)
        .map_err(|_| "Invalid UTF-8 in response")?;
    
    // Find header/body separator
    let header_end = response_str.find("\r\n\r\n")
        .ok_or("No header/body separator found")?;
    
    let header_section = &response_str[..header_end];
    let body_start = header_end + 4;
    
    // Parse status line
    let mut lines = header_section.lines();
    let status_line = lines.next().ok_or("Missing status line")?;
    
    // Parse "HTTP/1.x STATUS STATUS_TEXT"
    let mut parts = status_line.splitn(3, ' ');
    let _version = parts.next().ok_or("Missing HTTP version")?;
    let status_str = parts.next().ok_or("Missing status code")?;
    let status_text = parts.next().unwrap_or("").to_string();
    
    let status_code: u16 = status_str.parse()
        .map_err(|_| "Invalid status code")?;
    
    // Parse headers
    let mut headers = BTreeMap::new();
    for line in lines {
        if let Some(colon_idx) = line.find(':') {
            let key = line[..colon_idx].trim().to_string();
            let value = line[colon_idx + 1..].trim().to_string();
            headers.insert(key, value);
        }
    }
    
    // Extract body
    let body = data[body_start..].to_vec();
    
    Ok(HttpResponse {
        status_code,
        status_text,
        headers,
        body,
    })
}

/// Perform an HTTP request using the network stack
/// 
/// This is a blocking call that:
/// 1. Resolves the hostname to IP (if needed)
/// 2. Connects via TCP (and TLS for HTTPS)
/// 3. Sends the HTTP request
/// 4. Receives and parses the response
pub fn http_request(
    net: &mut crate::net::NetState,
    request: &HttpRequest,
    timeout_ms: i64,
    get_time_ms: fn() -> i64,
) -> Result<HttpResponse, &'static str> {
    // For HTTPS, use the TLS module
    if request.is_https {
        return https_request(net, request, timeout_ms, get_time_ms);
    }
    
    // HTTP (non-TLS) request
    let dest_ip = resolve_host(net, &request.host, timeout_ms, get_time_ms)?;
    
    let start_time = get_time_ms();
    
    // Connect to the server
    net.tcp_connect(dest_ip, request.port, start_time)?;
    
    // Wait for connection to establish
    loop {
        let now = get_time_ms();
        if now - start_time > timeout_ms {
            net.tcp_abort();
            return Err("Connection timeout");
        }
        
        net.poll(now);
        
        if net.tcp_is_connected() {
            break;
        }
        
        if net.tcp_connection_failed() {
            return Err("Connection failed");
        }
        
        // Small delay to avoid busy-waiting
        for _ in 0..10000 {
            core::hint::spin_loop();
        }
    }
    
    // Send the HTTP request
    let request_bytes = request.build();
    let mut sent = 0;
    
    while sent < request_bytes.len() {
        let now = get_time_ms();
        if now - start_time > timeout_ms {
            net.tcp_abort();
            return Err("Send timeout");
        }
        
        net.poll(now);
        
        match net.tcp_send(&request_bytes[sent..], now) {
            Ok(n) if n > 0 => sent += n,
            Ok(_) => {}
            Err(e) => {
                net.tcp_abort();
                return Err(e);
            }
        }
        
        // Small delay
        for _ in 0..5000 {
            core::hint::spin_loop();
        }
    }
    
    // Receive the response
    let mut response_buf = Vec::with_capacity(8192);
    let mut recv_buf = [0u8; 1024];
    let mut headers_complete = false;
    let mut content_length: Option<usize> = None;
    let mut body_start = 0;
    
    loop {
        let now = get_time_ms();
        if now - start_time > timeout_ms {
            net.tcp_abort();
            return Err("Receive timeout");
        }
        
        net.poll(now);
        
        match net.tcp_recv(&mut recv_buf, now) {
            Ok(n) if n > 0 => {
                response_buf.extend_from_slice(&recv_buf[..n]);
                
                // Check if we've received all headers
                if !headers_complete {
                    if let Some(pos) = find_header_end(&response_buf) {
                        headers_complete = true;
                        body_start = pos + 4;
                        
                        // Parse content-length from headers
                        if let Ok(s) = core::str::from_utf8(&response_buf[..pos]) {
                            for line in s.lines() {
                                if line.to_lowercase().starts_with("content-length:") {
                                    if let Some(len_str) = line.split(':').nth(1) {
                                        content_length = len_str.trim().parse().ok();
                                    }
                                }
                            }
                        }
                    }
                }
                
                // Check if we've received the complete response
                if headers_complete {
                    let body_len = response_buf.len() - body_start;
                    match content_length {
                        Some(expected) if body_len >= expected => break,
                        None => {
                            // No content-length, wait for connection close
                        }
                        _ => {}
                    }
                }
            }
            Ok(_) => {
                // No data available, check if connection closed
                if net.tcp_connection_failed() {
                    break;
                }
            }
            Err(e) => {
                if e == "Connection closed by peer" && response_buf.len() > 0 {
                    break;
                }
                net.tcp_abort();
                return Err(e);
            }
        }
        
        // Small delay
        for _ in 0..5000 {
            core::hint::spin_loop();
        }
    }
    
    // Close the connection
    net.tcp_close(get_time_ms());
    
    // Parse the response
    if response_buf.is_empty() {
        return Err("Empty response");
    }
    
    parse_response(&response_buf)
}

/// Perform an HTTP request with automatic redirect following
/// 
/// This is similar to `http_request` but automatically follows 3xx redirects
/// up to MAX_REDIRECTS times.
/// 
/// Redirect types handled:
/// - 301 Moved Permanently
/// - 302 Found
/// - 303 See Other (method changes to GET)
/// - 307 Temporary Redirect (preserves method)
/// - 308 Permanent Redirect (preserves method)
pub fn http_request_follow_redirects(
    net: &mut crate::net::NetState,
    request: &HttpRequest,
    timeout_ms: i64,
    get_time_ms: fn() -> i64,
) -> Result<HttpResponse, &'static str> {
    let mut current_request = HttpRequest {
        method: request.method,
        host: request.host.clone(),
        path: request.path.clone(),
        port: request.port,
        headers: request.headers.clone(),
        body: request.body.clone(),
        is_https: request.is_https,
    };
    
    let mut redirects = 0u8;
    
    loop {
        let response = http_request(net, &current_request, timeout_ms, get_time_ms)?;
        
        // Check if this is a redirect
        if !response.is_redirect() {
            return Ok(response);
        }
        
        // Check redirect limit
        redirects += 1;
        if redirects > MAX_REDIRECTS {
            return Err("Too many redirects");
        }
        
        // Get redirect location
        let location = response.redirect_location()
            .ok_or("Redirect without Location header")?;
        
        // Resolve the redirect URL
        let new_url = resolve_redirect_url(&current_request, location)?;
        
        crate::uart::write_str("Redirecting to: ");
        crate::uart::write_line(&new_url);
        
        // Parse the new URL
        let parsed = parse_url(&new_url)?;
        
        // For 303, change method to GET and drop body
        // For 301/302, many clients also change to GET (though technically shouldn't for 301)
        let new_method = if response.status_code == 303 || 
                           (response.status_code == 301 || response.status_code == 302) && 
                           current_request.method == HttpMethod::Post {
            HttpMethod::Get
        } else {
            current_request.method
        };
        
        // Build new request
        let mut new_headers = BTreeMap::new();
        new_headers.insert("Host".to_string(), parsed.host.clone());
        new_headers.insert("User-Agent".to_string(), format!("BAVY OS/{}", env!("CARGO_PKG_VERSION")));
        new_headers.insert("Accept".to_string(), "*/*".to_string());
        new_headers.insert("Connection".to_string(), "close".to_string());
        
        // Drop body for GET requests
        let new_body = if new_method == HttpMethod::Get {
            None
        } else {
            current_request.body.clone()
        };
        
        current_request = HttpRequest {
            method: new_method,
            host: parsed.host,
            path: parsed.path,
            port: parsed.port,
            headers: new_headers,
            body: new_body,
            is_https: parsed.is_https,
        };
    }
}

/// Resolve hostname to IP address (handles both IPs and hostnames)
fn resolve_host(
    net: &mut crate::net::NetState,
    host: &str,
    timeout_ms: i64,
    get_time_ms: fn() -> i64,
) -> Result<Ipv4Address, &'static str> {
    // Try to parse as IP address first
    if let Some(ip) = crate::net::parse_ipv4(host.as_bytes()) {
        return Ok(ip);
    }
    
    // Resolve via DNS
    crate::dns::resolve(net, host.as_bytes(), crate::net::DNS_SERVER, timeout_ms, get_time_ms)
        .ok_or("DNS resolution failed")
}

/// Find the end of HTTP headers (double CRLF)
fn find_header_end(data: &[u8]) -> Option<usize> {
    for i in 0..data.len().saturating_sub(3) {
        if data[i] == b'\r' && data[i + 1] == b'\n' 
           && data[i + 2] == b'\r' && data[i + 3] == b'\n' {
            return Some(i);
        }
    }
    None
}

/// Simple GET request helper (does not follow redirects)
pub fn get(
    net: &mut crate::net::NetState,
    url: &str,
    timeout_ms: i64,
    get_time_ms: fn() -> i64,
) -> Result<HttpResponse, &'static str> {
    let request = HttpRequest::get(url)?;
    http_request(net, &request, timeout_ms, get_time_ms)
}

/// Simple GET request helper that follows redirects
pub fn get_follow_redirects(
    net: &mut crate::net::NetState,
    url: &str,
    timeout_ms: i64,
    get_time_ms: fn() -> i64,
) -> Result<HttpResponse, &'static str> {
    let request = HttpRequest::get(url)?;
    http_request_follow_redirects(net, &request, timeout_ms, get_time_ms)
}

/// Simple POST request helper
pub fn post(
    net: &mut crate::net::NetState,
    url: &str,
    body: &str,
    content_type: &str,
    timeout_ms: i64,
    get_time_ms: fn() -> i64,
) -> Result<HttpResponse, &'static str> {
    let request = HttpRequest::post(url)?
        .header("Content-Type", content_type)
        .body_str(body);
    http_request(net, &request, timeout_ms, get_time_ms)
}

// ═══════════════════════════════════════════════════════════════════════════════
// HTTPS SUPPORT
// ═══════════════════════════════════════════════════════════════════════════════

/// Perform an HTTPS request using TLS
/// Tries TLS 1.3 first, falls back to TLS 1.2 if needed
fn https_request(
    net: &mut crate::net::NetState,
    request: &HttpRequest,
    timeout_ms: i64,
    get_time_ms: fn() -> i64,
) -> Result<HttpResponse, &'static str> {
    // Resolve hostname to IP address
    let dest_ip = resolve_host(net, &request.host, timeout_ms, get_time_ms)?;
    
    // Build the HTTP request bytes
    let request_bytes = request.build();
    
    // Use longer timeout for HTTPS (TLS handshake needs multiple round trips)
    let https_timeout = timeout_ms.max(30000);
    
    // Try TLS 1.3 first (embedded-tls)
    let response_bytes = match crate::tls::https_request(
        net,
        dest_ip,
        request.port,
        &request.host,
        &request_bytes,
        https_timeout,
        get_time_ms,
    ) {
        Ok(bytes) => bytes,
        Err(_) => {
            // TLS 1.3 failed, try TLS 1.2
            crate::uart::write_line("TLS 1.3 failed, trying TLS 1.2...");
            
            crate::tls12::https_request_tls12(
                net,
                dest_ip,
                request.port,
                &request.host,
                &request_bytes,
                https_timeout,
                get_time_ms,
            ).map_err(|e| match e {
                crate::tls::TlsError::ConnectionError => "HTTPS: TCP connection failed",
                crate::tls::TlsError::TlsProtocolError => "HTTPS: TLS handshake failed",
                crate::tls::TlsError::Timeout => "HTTPS: Connection timeout",
                crate::tls::TlsError::InvalidData => "HTTPS: Invalid TLS data",
                crate::tls::TlsError::Io => "HTTPS: I/O error",
                crate::tls::TlsError::ConnectionClosed => "HTTPS: Connection closed",
                crate::tls::TlsError::NotConnected => "HTTPS: Not connected",
                crate::tls::TlsError::DnsError => "HTTPS: DNS resolution failed",
                crate::tls::TlsError::InternalError => "HTTPS: Internal TLS error",
            })?
        }
    };
    
    // Parse the response
    if response_bytes.is_empty() {
        return Err("Empty HTTPS response");
    }
    
    parse_response(&response_bytes)
}

