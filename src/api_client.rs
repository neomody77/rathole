use crate::config::{ClientServiceConfig, ServiceType};
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use tracing::{debug, info};

#[derive(Debug, Serialize)]
struct CreateServiceRequest {
    name: String,
    #[serde(rename = "type")]
    service_type: String,
    bind_addr: String,
    token: String,
}

#[derive(Debug, Deserialize)]
pub struct ServiceInfo {
    pub name: String,
    #[serde(rename = "type")]
    pub service_type: String,
    pub bind_addr: String,
}

#[derive(Debug, Deserialize)]
struct ApiError {
    error: String,
}

pub struct ApiClient {
    base_url: String,
    token: String,
}

impl ApiClient {
    pub fn new(api_addr: &str, token: &str) -> Self {
        Self {
            base_url: api_addr.to_string(),
            token: token.to_string(),
        }
    }

    /// Register a service with the server via API
    pub fn register_service(&self, service: &ClientServiceConfig) -> Result<ServiceInfo> {
        let bind_addr = service
            .bind_addr
            .clone()
            .unwrap_or_else(|| "0.0.0.0:0".to_string());

        let token = service
            .token
            .as_ref()
            .map(|t| t.to_string())
            .unwrap_or_default();

        let req = CreateServiceRequest {
            name: service.name.clone(),
            service_type: match service.service_type {
                ServiceType::Tcp => "tcp".to_string(),
                ServiceType::Udp => "udp".to_string(),
            },
            bind_addr,
            token,
        };

        let body = serde_json::to_string(&req)?;
        let response = self.http_request("POST", "/api/services", Some(&body))?;

        if response.status >= 200 && response.status < 300 {
            let info: ServiceInfo = serde_json::from_str(&response.body)
                .with_context(|| "Failed to parse service info response")?;
            info!(
                "Registered service '{}' at {}",
                info.name, info.bind_addr
            );
            Ok(info)
        } else {
            let err: ApiError = serde_json::from_str(&response.body)
                .unwrap_or(ApiError { error: response.body.clone() });
            bail!("Failed to register service '{}': {}", service.name, err.error)
        }
    }

    /// Check if the API server is reachable
    pub fn health_check(&self) -> Result<bool> {
        match self.http_request("GET", "/api/health", None) {
            Ok(response) => Ok(response.status >= 200 && response.status < 300),
            Err(_) => Ok(false),
        }
    }

    fn http_request(&self, method: &str, path: &str, body: Option<&str>) -> Result<HttpResponse> {
        let addr = &self.base_url;
        debug!("API request: {} {} to {}", method, path, addr);

        let mut stream = TcpStream::connect(addr)
            .with_context(|| format!("Failed to connect to API server at {}", addr))?;

        // Set timeout
        stream.set_read_timeout(Some(std::time::Duration::from_secs(10)))?;
        stream.set_write_timeout(Some(std::time::Duration::from_secs(10)))?;

        // Build request
        let content_length = body.map(|b| b.len()).unwrap_or(0);
        let host = addr.split(':').next().unwrap_or(addr);

        let mut request = format!(
            "{} {} HTTP/1.1\r\n\
             Host: {}\r\n\
             Authorization: Bearer {}\r\n\
             Content-Type: application/json\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n\
             \r\n",
            method, path, host, self.token, content_length
        );

        if let Some(body) = body {
            request.push_str(body);
        }

        stream.write_all(request.as_bytes())?;
        stream.flush()?;

        // Read response
        let mut reader = BufReader::new(stream);

        // Parse status line
        let mut status_line = String::new();
        reader.read_line(&mut status_line)?;
        let status = parse_status_code(&status_line)?;

        // Parse headers
        let mut content_length: usize = 0;
        let mut chunked = false;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line)?;
            let line = line.trim();
            if line.is_empty() {
                break;
            }
            let line_lower = line.to_lowercase();
            if let Some(value) = line_lower.strip_prefix("content-length:") {
                content_length = value.trim().parse().unwrap_or(0);
            }
            if let Some(value) = line_lower.strip_prefix("transfer-encoding:") {
                chunked = value.trim().eq_ignore_ascii_case("chunked");
            }
        }

        // Read body
        let body = if chunked {
            read_chunked_body(&mut reader)?
        } else if content_length > 0 {
            let mut body = vec![0u8; content_length];
            std::io::Read::read_exact(&mut reader, &mut body)?;
            String::from_utf8_lossy(&body).to_string()
        } else {
            String::new()
        };

        debug!("API response: {} - {}", status, body);

        Ok(HttpResponse { status, body })
    }
}

struct HttpResponse {
    status: u16,
    body: String,
}

fn parse_status_code(status_line: &str) -> Result<u16> {
    // HTTP/1.1 200 OK
    let parts: Vec<&str> = status_line.split_whitespace().collect();
    if parts.len() >= 2 {
        parts[1]
            .parse()
            .with_context(|| format!("Invalid status code: {}", status_line))
    } else {
        Err(anyhow!("Invalid status line: {}", status_line))
    }
}

fn read_chunked_body<R: BufRead>(reader: &mut R) -> Result<String> {
    let mut body = String::new();
    loop {
        let mut size_line = String::new();
        reader.read_line(&mut size_line)?;
        let size = usize::from_str_radix(size_line.trim(), 16).unwrap_or(0);
        if size == 0 {
            break;
        }
        let mut chunk = vec![0u8; size];
        std::io::Read::read_exact(reader, &mut chunk)?;
        body.push_str(&String::from_utf8_lossy(&chunk));
        // Read trailing CRLF
        let mut crlf = [0u8; 2];
        let _ = std::io::Read::read_exact(reader, &mut crlf);
    }
    Ok(body)
}

/// Helper to derive API address from remote_addr if not explicitly set
pub fn derive_api_addr(remote_addr: &str, api_addr: Option<&str>, default_port: u16) -> String {
    if let Some(addr) = api_addr {
        return addr.to_string();
    }

    // Extract host from remote_addr and use default API port
    if let Some(colon_pos) = remote_addr.rfind(':') {
        let host = &remote_addr[..colon_pos];
        format!("{}:{}", host, default_port)
    } else {
        format!("{}:{}", remote_addr, default_port)
    }
}
