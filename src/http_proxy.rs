//! HTTP proxy module using Pingora for domain-based routing
//!
//! This module provides HTTP/HTTPS reverse proxy functionality with:
//! - Domain-based routing (Host header / SNI)
//! - Wildcard domain support (e.g., *.example.com)
//! - Dynamic service mapping updates

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
}

/// Domain matcher for wildcard support
pub struct DomainMatcher {
    /// Exact domain matches: domain -> service_bind_addr
    exact: HashMap<String, String>,
    /// Wildcard patterns: (suffix, service_bind_addr)
    /// e.g., ".example.com" for *.example.com
    wildcards: Vec<(String, String)>,
}

impl DomainMatcher {
    pub fn new() -> Self {
        Self {
            exact: HashMap::new(),
            wildcards: Vec::new(),
        }
    }

    /// Add a domain pattern for a service
    pub fn add(&mut self, pattern: &str, bind_addr: &str) {
        if pattern.starts_with("*.") {
            // Wildcard pattern: *.example.com -> .example.com
            let suffix = pattern.strip_prefix('*').unwrap().to_lowercase();
            self.wildcards.push((suffix, bind_addr.to_string()));
        } else {
            // Exact match
            self.exact.insert(pattern.to_lowercase(), bind_addr.to_string());
        }
    }

    /// Match a domain and return the service bind address
    pub fn match_domain(&self, domain: &str) -> Option<&str> {
        let domain_lower = domain.to_lowercase();

        // Strip port if present
        let domain_without_port = domain_lower.split(':').next().unwrap_or(&domain_lower);

        // Try exact match first
        if let Some(addr) = self.exact.get(domain_without_port) {
            return Some(addr);
        }

        // Try wildcard matches
        for (suffix, addr) in &self.wildcards {
            if domain_without_port.ends_with(suffix) {
                return Some(addr);
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
                matcher.add(domain, &svc.bind_addr);
                debug!("HTTP proxy: {} -> {}", domain, svc.bind_addr);
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
            matcher.add(domain, &svc.bind_addr);
            debug!("HTTP proxy: {} -> {}", domain, svc.bind_addr);
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
        let bind_addr = match matcher.match_domain(host) {
            Some(addr) => addr.to_string(),
            None => {
                warn!("HTTP proxy: Unknown domain: {}", host);
                return Err(pingora::Error::new(pingora::ErrorType::HTTPStatus(404)).into_down());
            }
        };

        debug!("HTTP proxy: {} -> {}", host, bind_addr);

        // Parse bind address
        let peer = HttpPeer::new(&bind_addr, false, String::new());
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
    pub async fn run(self, mut shutdown_rx: tokio::sync::broadcast::Receiver<bool>) -> Result<()> {
        info!("Starting HTTP proxy on {}", self.config.http_addr);

        let mut server = Server::new(None)?;
        server.bootstrap();

        let proxy = RatholeHttpProxy::new(self.state.clone());
        let mut http_service = http_proxy_service(&server.configuration, proxy);
        http_service.add_tcp(&self.config.http_addr);

        // TODO: Add HTTPS support with ACME in Phase 2

        server.add_service(http_service);

        // Run server in background
        let server_handle = tokio::spawn(async move {
            server.run_forever();
        });

        // Wait for shutdown signal
        tokio::select! {
            _ = shutdown_rx.recv() => {
                info!("HTTP proxy received shutdown signal");
            }
            _ = server_handle => {
                warn!("HTTP proxy server exited unexpectedly");
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_domain_matcher_exact() {
        let mut matcher = DomainMatcher::new();
        matcher.add("example.com", "127.0.0.1:8080");
        matcher.add("api.example.com", "127.0.0.1:8081");

        assert_eq!(matcher.match_domain("example.com"), Some("127.0.0.1:8080"));
        assert_eq!(matcher.match_domain("api.example.com"), Some("127.0.0.1:8081"));
        assert_eq!(matcher.match_domain("unknown.com"), None);
    }

    #[test]
    fn test_domain_matcher_wildcard() {
        let mut matcher = DomainMatcher::new();
        matcher.add("*.example.com", "127.0.0.1:8080");
        matcher.add("exact.example.com", "127.0.0.1:8081");

        // Exact match takes precedence
        assert_eq!(matcher.match_domain("exact.example.com"), Some("127.0.0.1:8081"));

        // Wildcard matches
        assert_eq!(matcher.match_domain("app.example.com"), Some("127.0.0.1:8080"));
        assert_eq!(matcher.match_domain("api.example.com"), Some("127.0.0.1:8080"));

        // Does not match root domain
        assert_eq!(matcher.match_domain("example.com"), None);
    }

    #[test]
    fn test_domain_matcher_case_insensitive() {
        let mut matcher = DomainMatcher::new();
        matcher.add("Example.COM", "127.0.0.1:8080");

        assert_eq!(matcher.match_domain("example.com"), Some("127.0.0.1:8080"));
        assert_eq!(matcher.match_domain("EXAMPLE.COM"), Some("127.0.0.1:8080"));
    }

    #[test]
    fn test_domain_matcher_with_port() {
        let mut matcher = DomainMatcher::new();
        matcher.add("example.com", "127.0.0.1:8080");

        assert_eq!(matcher.match_domain("example.com:443"), Some("127.0.0.1:8080"));
    }
}
