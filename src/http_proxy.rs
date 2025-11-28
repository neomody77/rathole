//! HTTP proxy module using Pingora for domain-based routing
//!
//! This module provides HTTP/HTTPS reverse proxy functionality with:
//! - Domain-based routing (Host header / SNI)
//! - Wildcard domain support (e.g., *.example.com)
//! - Dynamic service mapping updates
//! - HTTPS termination with manual certificates
//! - Backend TLS support (https->https, https->http)

use anyhow::Result;
use async_trait::async_trait;
use pingora::prelude::*;
use pingora_proxy::{http_proxy_service, ProxyHttp, Session};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

use crate::config::HttpProxyConfig;

/// Service information for routing
#[derive(Debug, Clone)]
pub struct ProxyServiceInfo {
    pub name: String,
    pub bind_addr: String,
    pub domains: Vec<String>,
    /// Whether the backend uses HTTPS
    pub backend_tls: bool,
}

/// Backend routing information
#[derive(Debug, Clone)]
pub struct BackendInfo {
    pub bind_addr: String,
    pub use_tls: bool,
}

/// Domain matcher for wildcard support
pub struct DomainMatcher {
    /// Exact domain matches: domain -> backend info
    exact: HashMap<String, BackendInfo>,
    /// Wildcard patterns: (suffix, backend info)
    /// e.g., ".example.com" for *.example.com
    wildcards: Vec<(String, BackendInfo)>,
}

impl DomainMatcher {
    pub fn new() -> Self {
        Self {
            exact: HashMap::new(),
            wildcards: Vec::new(),
        }
    }

    /// Add a domain pattern for a service
    pub fn add(&mut self, pattern: &str, bind_addr: &str, use_tls: bool) {
        let backend = BackendInfo {
            bind_addr: bind_addr.to_string(),
            use_tls,
        };
        if pattern.starts_with("*.") {
            // Wildcard pattern: *.example.com -> .example.com
            let suffix = pattern.strip_prefix('*').unwrap().to_lowercase();
            self.wildcards.push((suffix, backend));
        } else {
            // Exact match
            self.exact.insert(pattern.to_lowercase(), backend);
        }
    }

    /// Match a domain and return the backend info
    pub fn match_domain(&self, domain: &str) -> Option<&BackendInfo> {
        let domain_lower = domain.to_lowercase();

        // Strip port if present
        let domain_without_port = domain_lower.split(':').next().unwrap_or(&domain_lower);

        // Try exact match first
        if let Some(backend) = self.exact.get(domain_without_port) {
            return Some(backend);
        }

        // Try wildcard matches
        for (suffix, backend) in &self.wildcards {
            if domain_without_port.ends_with(suffix) {
                return Some(backend);
            }
        }

        None
    }

    /// Clear all mappings
    pub fn clear(&mut self) {
        self.exact.clear();
        self.wildcards.clear();
    }
}

impl Default for DomainMatcher {
    fn default() -> Self {
        Self::new()
    }
}

/// Shared state for the HTTP proxy
pub struct HttpProxyState {
    pub matcher: RwLock<DomainMatcher>,
    pub services: RwLock<HashMap<String, ProxyServiceInfo>>,
}

impl HttpProxyState {
    pub fn new() -> Self {
        Self {
            matcher: RwLock::new(DomainMatcher::new()),
            services: RwLock::new(HashMap::new()),
        }
    }

    /// Update service mappings
    pub async fn update_services(&self, services: Vec<ProxyServiceInfo>) {
        let mut matcher = self.matcher.write().await;
        let mut svc_map = self.services.write().await;

        matcher.clear();
        svc_map.clear();

        for svc in services {
            for domain in &svc.domains {
                matcher.add(domain, &svc.bind_addr, svc.backend_tls);
                debug!("HTTP proxy: {} -> {} (tls: {})", domain, svc.bind_addr, svc.backend_tls);
            }
            svc_map.insert(svc.name.clone(), svc);
        }

        info!("HTTP proxy: updated {} services", svc_map.len());
    }

    /// Add or update a single service
    pub async fn upsert_service(&self, svc: ProxyServiceInfo) {
        let mut matcher = self.matcher.write().await;
        let mut svc_map = self.services.write().await;

        // Remove old domains if service exists
        if let Some(old_svc) = svc_map.get(&svc.name) {
            for domain in &old_svc.domains {
                if domain.starts_with("*.") {
                    let suffix = domain.strip_prefix('*').unwrap().to_lowercase();
                    matcher.wildcards.retain(|(s, _)| s != &suffix);
                } else {
                    matcher.exact.remove(&domain.to_lowercase());
                }
            }
        }

        // Add new domains
        for domain in &svc.domains {
            matcher.add(domain, &svc.bind_addr, svc.backend_tls);
            debug!("HTTP proxy: {} -> {} (tls: {})", domain, svc.bind_addr, svc.backend_tls);
        }

        svc_map.insert(svc.name.clone(), svc);
    }

    /// Remove a service
    pub async fn remove_service(&self, name: &str) {
        let mut matcher = self.matcher.write().await;
        let mut svc_map = self.services.write().await;

        if let Some(svc) = svc_map.remove(name) {
            for domain in &svc.domains {
                if domain.starts_with("*.") {
                    let suffix = domain.strip_prefix('*').unwrap().to_lowercase();
                    matcher.wildcards.retain(|(s, _)| s != &suffix);
                } else {
                    matcher.exact.remove(&domain.to_lowercase());
                }
            }
            info!("HTTP proxy: removed service {}", name);
        }
    }
}

impl Default for HttpProxyState {
    fn default() -> Self {
        Self::new()
    }
}

/// Pingora HTTP proxy implementation
pub struct RatholeHttpProxy {
    state: Arc<HttpProxyState>,
}

impl RatholeHttpProxy {
    pub fn new(state: Arc<HttpProxyState>) -> Self {
        Self { state }
    }
}

#[async_trait]
impl ProxyHttp for RatholeHttpProxy {
    type CTX = ();

    fn new_ctx(&self) -> Self::CTX {}

    async fn upstream_peer(
        &self,
        session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>, Box<pingora::Error>> {
        // Get Host header
        let host = match session.req_header().headers.get("host").and_then(|h| h.to_str().ok()) {
            Some(h) => h,
            None => {
                error!("HTTP proxy: Missing Host header");
                return Err(pingora::Error::new(pingora::ErrorType::HTTPStatus(400)).into_down());
            }
        };

        // Look up service by domain
        let matcher = self.state.matcher.read().await;
        let backend = match matcher.match_domain(host) {
            Some(b) => b.clone(),
            None => {
                warn!("HTTP proxy: Unknown domain: {}", host);
                return Err(pingora::Error::new(pingora::ErrorType::HTTPStatus(404)).into_down());
            }
        };
        drop(matcher); // Release read lock

        debug!("HTTP proxy: {} -> {} (tls: {})", host, backend.bind_addr, backend.use_tls);

        // Create peer with or without TLS
        // For TLS backend, use the host for SNI
        let sni = if backend.use_tls {
            // Extract hostname without port for SNI
            host.split(':').next().unwrap_or(host).to_string()
        } else {
            String::new()
        };

        let peer = HttpPeer::new(&backend.bind_addr, backend.use_tls, sni);
        Ok(Box::new(peer))
    }

    async fn logging(
        &self,
        session: &mut Session,
        _e: Option<&pingora::Error>,
        _ctx: &mut Self::CTX,
    ) {
        let response_code = session
            .response_written()
            .map_or(0, |resp| resp.status.as_u16());

        let host = session
            .req_header()
            .headers
            .get("host")
            .and_then(|h| h.to_str().ok())
            .unwrap_or("-");

        let method = session.req_header().method.as_str();
        let path = session.req_header().uri.path();

        info!(
            "HTTP proxy: {} {} {} -> {}",
            method, host, path, response_code
        );
    }
}

/// HTTP proxy server wrapper
pub struct HttpProxyServer {
    config: HttpProxyConfig,
    state: Arc<HttpProxyState>,
}

impl HttpProxyServer {
    pub fn new(config: HttpProxyConfig, state: Arc<HttpProxyState>) -> Self {
        Self { config, state }
    }

    /// Run the HTTP proxy server
    /// Note: Pingora runs its own runtime, so we spawn it in a separate OS thread
    pub async fn run(self, mut shutdown_rx: tokio::sync::broadcast::Receiver<bool>) -> Result<()> {
        let has_tls = self.config.tls_cert.is_some() && self.config.tls_key.is_some();

        if has_tls {
            info!(
                "Starting HTTP proxy on {} (HTTP) and {} (HTTPS)",
                self.config.http_addr, self.config.https_addr
            );
        } else {
            info!("Starting HTTP proxy on {} (HTTP only)", self.config.http_addr);
        }

        let http_addr = self.config.http_addr.clone();
        let https_addr = self.config.https_addr.clone();
        let tls_cert = self.config.tls_cert.clone();
        let tls_key = self.config.tls_key.clone();
        let state = self.state.clone();

        // Run Pingora in a separate OS thread to avoid runtime conflicts
        let server_handle = std::thread::spawn(move || {
            let mut server = match Server::new(None) {
                Ok(s) => s,
                Err(e) => {
                    error!("Failed to create HTTP proxy server: {}", e);
                    return;
                }
            };
            server.bootstrap();

            let proxy = RatholeHttpProxy::new(state);
            let mut http_service = http_proxy_service(&server.configuration, proxy);

            // Add HTTP listener
            http_service.add_tcp(&http_addr);

            // Add HTTPS listener if TLS is configured
            if let (Some(cert_path), Some(key_path)) = (tls_cert, tls_key) {
                use pingora::listeners::tls::TlsSettings;
                use std::path::Path;

                // Verify files exist
                if !Path::new(&cert_path).exists() {
                    error!("TLS certificate file not found: {}", cert_path);
                } else if !Path::new(&key_path).exists() {
                    error!("TLS key file not found: {}", key_path);
                } else {
                    match TlsSettings::intermediate(&cert_path, &key_path) {
                        Ok(mut tls_settings) => {
                            tls_settings.enable_h2();
                            http_service.add_tls_with_settings(&https_addr, None, tls_settings);
                            info!("HTTPS enabled on {}", https_addr);
                        }
                        Err(e) => {
                            error!("Failed to create TLS settings: {}", e);
                        }
                    }
                }
            }

            server.add_service(http_service);
            server.run_forever();
        });

        // Wait for shutdown signal
        let _ = shutdown_rx.recv().await;
        info!("HTTP proxy received shutdown signal");

        // Note: Pingora doesn't have a clean shutdown mechanism in run_forever()
        // The thread will be terminated when the process exits
        drop(server_handle);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_domain_matcher_exact() {
        let mut matcher = DomainMatcher::new();
        matcher.add("example.com", "127.0.0.1:8080", false);
        matcher.add("api.example.com", "127.0.0.1:8081", true);

        let backend1 = matcher.match_domain("example.com").unwrap();
        assert_eq!(backend1.bind_addr, "127.0.0.1:8080");
        assert!(!backend1.use_tls);

        let backend2 = matcher.match_domain("api.example.com").unwrap();
        assert_eq!(backend2.bind_addr, "127.0.0.1:8081");
        assert!(backend2.use_tls);

        assert!(matcher.match_domain("unknown.com").is_none());
    }

    #[test]
    fn test_domain_matcher_wildcard() {
        let mut matcher = DomainMatcher::new();
        matcher.add("*.example.com", "127.0.0.1:8080", false);
        matcher.add("exact.example.com", "127.0.0.1:8081", false);

        // Exact match takes precedence
        assert_eq!(matcher.match_domain("exact.example.com").unwrap().bind_addr, "127.0.0.1:8081");

        // Wildcard matches
        assert_eq!(matcher.match_domain("app.example.com").unwrap().bind_addr, "127.0.0.1:8080");
        assert_eq!(matcher.match_domain("api.example.com").unwrap().bind_addr, "127.0.0.1:8080");

        // Does not match root domain
        assert!(matcher.match_domain("example.com").is_none());
    }

    #[test]
    fn test_domain_matcher_case_insensitive() {
        let mut matcher = DomainMatcher::new();
        matcher.add("Example.COM", "127.0.0.1:8080", false);

        assert_eq!(matcher.match_domain("example.com").unwrap().bind_addr, "127.0.0.1:8080");
        assert_eq!(matcher.match_domain("EXAMPLE.COM").unwrap().bind_addr, "127.0.0.1:8080");
    }

    #[test]
    fn test_domain_matcher_with_port() {
        let mut matcher = DomainMatcher::new();
        matcher.add("example.com", "127.0.0.1:8080", false);

        assert_eq!(matcher.match_domain("example.com:443").unwrap().bind_addr, "127.0.0.1:8080");
    }

    #[test]
    fn test_backend_tls() {
        let mut matcher = DomainMatcher::new();
        matcher.add("http.example.com", "127.0.0.1:8080", false);
        matcher.add("https.example.com", "127.0.0.1:8443", true);

        let http_backend = matcher.match_domain("http.example.com").unwrap();
        assert!(!http_backend.use_tls);

        let https_backend = matcher.match_domain("https.example.com").unwrap();
        assert!(https_backend.use_tls);
    }
}
