use crate::config::{ApiConfig, MaskedString, ServerServiceConfig, ServiceType};
use crate::config_watcher::{ConfigChange, ServerServiceChange};
use crate::protocol;
use anyhow::{anyhow, Result};
use axum::{
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{delete, get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc, RwLock};
use tower_http::trace::TraceLayer;
use tracing::{error, info};

type ServiceDigest = protocol::Digest;

#[derive(Clone)]
pub struct ApiState {
    pub config: ApiConfig,
    pub services: Arc<RwLock<HashMap<ServiceDigest, ServerServiceConfig>>>,
    pub services_by_name: Arc<RwLock<HashMap<String, ServerServiceConfig>>>,
    pub config_path: Option<PathBuf>,
    pub event_tx: mpsc::UnboundedSender<ConfigChange>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ServiceInfo {
    pub name: String,
    #[serde(rename = "type")]
    pub service_type: String,
    pub bind_addr: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CreateServiceRequest {
    pub name: String,
    #[serde(rename = "type", default = "default_service_type")]
    pub service_type: String,
    pub bind_addr: String,
    pub token: String,
}

fn default_service_type() -> String {
    "tcp".to_string()
}

#[derive(Debug, Serialize)]
pub struct ListServicesResponse {
    pub services: Vec<ServiceInfo>,
}

#[derive(Debug, Serialize)]
pub struct ApiError {
    pub error: String,
}

impl ApiState {
    pub fn new(
        config: ApiConfig,
        services: Arc<RwLock<HashMap<ServiceDigest, ServerServiceConfig>>>,
        config_path: Option<PathBuf>,
        event_tx: mpsc::UnboundedSender<ConfigChange>,
    ) -> Self {
        Self {
            config,
            services,
            services_by_name: Arc::new(RwLock::new(HashMap::new())),
            config_path,
            event_tx,
        }
    }

    pub async fn sync_services_by_name(&self) {
        let services = self.services.read().await;
        let mut by_name = self.services_by_name.write().await;
        by_name.clear();
        for (_, svc) in services.iter() {
            by_name.insert(svc.name.clone(), svc.clone());
        }
    }
}

fn validate_token(state: &ApiState, headers: &HeaderMap) -> Result<(), (StatusCode, Json<ApiError>)> {
    let expected_token = match &state.config.token {
        Some(t) => t,
        None => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiError {
                    error: "API token not configured".to_string(),
                }),
            ))
        }
    };

    let auth_header = headers
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok());

    match auth_header {
        Some(auth) => {
            let token = auth.strip_prefix("Bearer ").unwrap_or(auth);
            let expected: &str = expected_token.as_ref();
            if token == expected {
                Ok(())
            } else {
                Err((
                    StatusCode::UNAUTHORIZED,
                    Json(ApiError {
                        error: "Invalid token".to_string(),
                    }),
                ))
            }
        }
        None => Err((
            StatusCode::UNAUTHORIZED,
            Json(ApiError {
                error: "Missing Authorization header".to_string(),
            }),
        )),
    }
}

#[derive(Serialize)]
struct HealthResponse {
    status: String,
}

async fn health_check() -> impl IntoResponse {
    Json(HealthResponse {
        status: "ok".to_string(),
    })
}

async fn list_services(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Result<Json<ListServicesResponse>, (StatusCode, Json<ApiError>)> {
    validate_token(&state, &headers)?;

    state.sync_services_by_name().await;
    let services = state.services_by_name.read().await;

    let service_list: Vec<ServiceInfo> = services
        .values()
        .map(|svc| ServiceInfo {
            name: svc.name.clone(),
            service_type: match svc.service_type {
                ServiceType::Tcp => "tcp".to_string(),
                ServiceType::Udp => "udp".to_string(),
            },
            bind_addr: svc.bind_addr.clone(),
            token: None,
        })
        .collect();

    Ok(Json(ListServicesResponse {
        services: service_list,
    }))
}

async fn get_service(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Json<ServiceInfo>, (StatusCode, Json<ApiError>)> {
    validate_token(&state, &headers)?;

    state.sync_services_by_name().await;
    let services = state.services_by_name.read().await;

    match services.get(&name) {
        Some(svc) => Ok(Json(ServiceInfo {
            name: svc.name.clone(),
            service_type: match svc.service_type {
                ServiceType::Tcp => "tcp".to_string(),
                ServiceType::Udp => "udp".to_string(),
            },
            bind_addr: svc.bind_addr.clone(),
            token: None,
        })),
        None => Err((
            StatusCode::NOT_FOUND,
            Json(ApiError {
                error: format!("Service '{}' not found", name),
            }),
        )),
    }
}

async fn create_service(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(req): Json<CreateServiceRequest>,
) -> Result<(StatusCode, Json<ServiceInfo>), (StatusCode, Json<ApiError>)> {
    validate_token(&state, &headers)?;

    // Validate service type
    let service_type = match req.service_type.to_lowercase().as_str() {
        "tcp" => ServiceType::Tcp,
        "udp" => ServiceType::Udp,
        _ => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(ApiError {
                    error: format!("Invalid service type: {}", req.service_type),
                }),
            ))
        }
    };

    // Check if service already exists - if so, return existing service info
    state.sync_services_by_name().await;
    {
        let services = state.services_by_name.read().await;
        if let Some(svc) = services.get(&req.name) {
            info!("Service '{}' already exists, returning existing info", req.name);
            return Ok((StatusCode::OK, Json(ServiceInfo {
                name: svc.name.clone(),
                service_type: match svc.service_type {
                    ServiceType::Tcp => "tcp".to_string(),
                    ServiceType::Udp => "udp".to_string(),
                },
                bind_addr: svc.bind_addr.clone(),
                token: None,
            })));
        }
    }

    // Handle port auto-assignment (port 0)
    let bind_addr = if req.bind_addr.ends_with(":0") {
        // Find an available port
        let addr: SocketAddr = req
            .bind_addr
            .parse()
            .map_err(|_| {
                (
                    StatusCode::BAD_REQUEST,
                    Json(ApiError {
                        error: format!("Invalid bind address: {}", req.bind_addr),
                    }),
                )
            })?;

        // Bind to port 0 to get an available port
        let listener = std::net::TcpListener::bind(addr).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiError {
                    error: format!("Failed to allocate port: {}", e),
                }),
            )
        })?;

        let actual_addr = listener.local_addr().map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiError {
                    error: format!("Failed to get allocated port: {}", e),
                }),
            )
        })?;

        // Drop the listener so the port can be reused
        drop(listener);

        format!("{}:{}", addr.ip(), actual_addr.port())
    } else {
        req.bind_addr.clone()
    };

    // Create the service config
    let service_config = ServerServiceConfig {
        service_type,
        name: req.name.clone(),
        bind_addr: bind_addr.clone(),
        token: Some(MaskedString::from(req.token.as_str())),
        nodelay: None,
    };

    // Send update event
    if let Err(e) = state
        .event_tx
        .send(ConfigChange::ServerChange(ServerServiceChange::Add(
            service_config.clone(),
        )))
    {
        error!("Failed to send service add event: {}", e);
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiError {
                error: "Failed to add service".to_string(),
            }),
        ));
    }

    // Persist to config file if path is set
    if let Some(ref config_path) = state.config_path {
        if let Err(e) = persist_service_to_config(config_path, &service_config).await {
            error!("Failed to persist service to config: {}", e);
            // Continue anyway, the service is added in memory
        }
    }

    info!("Service '{}' created at {}", req.name, bind_addr);

    Ok((
        StatusCode::CREATED,
        Json(ServiceInfo {
            name: req.name,
            service_type: match service_type {
                ServiceType::Tcp => "tcp".to_string(),
                ServiceType::Udp => "udp".to_string(),
            },
            bind_addr,
            token: None,
        }),
    ))
}

async fn delete_service(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<StatusCode, (StatusCode, Json<ApiError>)> {
    validate_token(&state, &headers)?;

    // Check if service exists
    state.sync_services_by_name().await;
    {
        let services = state.services_by_name.read().await;
        if !services.contains_key(&name) {
            return Err((
                StatusCode::NOT_FOUND,
                Json(ApiError {
                    error: format!("Service '{}' not found", name),
                }),
            ));
        }
    }

    // Send delete event
    if let Err(e) = state
        .event_tx
        .send(ConfigChange::ServerChange(ServerServiceChange::Delete(
            name.clone(),
        )))
    {
        error!("Failed to send service delete event: {}", e);
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiError {
                error: "Failed to delete service".to_string(),
            }),
        ));
    }

    // Remove from config file if path is set
    if let Some(ref config_path) = state.config_path {
        if let Err(e) = remove_service_from_config(config_path, &name).await {
            error!("Failed to remove service from config: {}", e);
            // Continue anyway, the service is removed in memory
        }
    }

    info!("Service '{}' deleted", name);

    Ok(StatusCode::NO_CONTENT)
}

async fn persist_service_to_config(config_path: &PathBuf, service: &ServerServiceConfig) -> Result<()> {
    use tokio::fs;

    let content = fs::read_to_string(config_path).await?;
    let mut doc: toml::Value = toml::from_str(&content)?;

    // Get or create server.services
    let server = doc
        .get_mut("server")
        .ok_or_else(|| anyhow!("No [server] section in config"))?;

    let services = server
        .as_table_mut()
        .ok_or_else(|| anyhow!("Invalid server section"))?
        .entry("services")
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));

    let services_table = services
        .as_table_mut()
        .ok_or_else(|| anyhow!("Invalid services section"))?;

    // Add the new service
    let mut service_table = toml::map::Map::new();
    service_table.insert(
        "type".to_string(),
        toml::Value::String(match service.service_type {
            ServiceType::Tcp => "tcp".to_string(),
            ServiceType::Udp => "udp".to_string(),
        }),
    );
    service_table.insert(
        "bind_addr".to_string(),
        toml::Value::String(service.bind_addr.clone()),
    );
    if let Some(ref token) = service.token {
        service_table.insert(
            "token".to_string(),
            toml::Value::String(token.to_string()),
        );
    }

    services_table.insert(service.name.clone(), toml::Value::Table(service_table));

    // Write back
    let new_content = toml::to_string_pretty(&doc)?;
    fs::write(config_path, new_content).await?;

    Ok(())
}

async fn remove_service_from_config(config_path: &PathBuf, service_name: &str) -> Result<()> {
    use tokio::fs;

    let content = fs::read_to_string(config_path).await?;
    let mut doc: toml::Value = toml::from_str(&content)?;

    // Get server.services
    if let Some(server) = doc.get_mut("server") {
        if let Some(server_table) = server.as_table_mut() {
            if let Some(services) = server_table.get_mut("services") {
                if let Some(services_table) = services.as_table_mut() {
                    services_table.remove(service_name);
                }
            }
        }
    }

    // Write back
    let new_content = toml::to_string_pretty(&doc)?;
    fs::write(config_path, new_content).await?;

    Ok(())
}

pub fn create_router(state: ApiState) -> Router {
    Router::new()
        .route("/api/health", get(health_check))
        .route("/api/services", get(list_services))
        .route("/api/services", post(create_service))
        .route("/api/services/{name}", get(get_service))
        .route("/api/services/{name}", delete(delete_service))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

pub async fn run_api_server(
    config: ApiConfig,
    services: Arc<RwLock<HashMap<ServiceDigest, ServerServiceConfig>>>,
    config_path: Option<PathBuf>,
    event_tx: mpsc::UnboundedSender<ConfigChange>,
    mut shutdown_rx: broadcast::Receiver<bool>,
) -> Result<()> {
    let state = ApiState::new(config.clone(), services, config_path, event_tx);
    let app = create_router(state);

    let addr: SocketAddr = config.bind_addr.parse()?;
    let listener = TcpListener::bind(addr).await?;

    info!("API server listening at {}", addr);

    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = shutdown_rx.recv().await;
            info!("API server shutting down");
        })
        .await?;

    Ok(())
}
