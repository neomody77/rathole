mod cli;
mod config;
mod config_watcher;
mod constants;
mod helper;
mod multi_map;
mod protocol;
mod transport;

#[cfg(feature = "api")]
mod api;

#[cfg(feature = "api")]
mod api_client;

pub use cli::Cli;
use cli::KeypairType;
pub use config::Config;
pub use constants::UDP_BUFFER_SIZE;

use anyhow::{bail, Result};
use std::collections::HashMap;
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, info};

#[cfg(feature = "client")]
mod client;
#[cfg(feature = "client")]
use client::run_client;

#[cfg(feature = "server")]
mod server;
#[cfg(all(feature = "server", not(feature = "api")))]
use server::run_server;
#[cfg(all(feature = "server", feature = "api"))]
use server::run_server_with_api;

use crate::config_watcher::{ConfigChange, ConfigWatcherHandle};

const DEFAULT_CURVE: KeypairType = KeypairType::X25519;

/// Run in CLI mode - build config from command line arguments
#[cfg(feature = "client")]
async fn run_cli_mode(
    args: Cli,
    remote: String,
    shutdown_rx: broadcast::Receiver<bool>,
) -> Result<()> {
    use config::{AutoRegisterConfig, ClientConfig, ClientServiceConfig, MaskedString, ServiceType};

    // Token is required
    let token = args.token.ok_or_else(|| {
        anyhow::anyhow!("--token is required when using --remote")
    })?;

    // Build services from -L arguments
    let mut services = HashMap::new();
    for svc in &args.services {
        let service_config = ClientServiceConfig {
            service_type: ServiceType::Tcp,
            name: svc.name.clone(),
            local_addr: svc.local_addr.clone(),
            prefer_ipv6: false,
            token: Some(MaskedString::from(token.as_str())),
            nodelay: None,
            retry_interval: Some(1), // Default retry interval
            bind_addr: svc.bind_addr.clone(),
        };
        services.insert(svc.name.clone(), service_config);
    }

    if services.is_empty() {
        bail!("At least one service is required. Use -L name:local_addr[:bind_addr]");
    }

    // Build auto_register config if API is specified
    let auto_register = if let Some(api_addr) = args.api {
        Some(AutoRegisterConfig {
            enabled: true,
            api_addr: Some(api_addr),
            api_token: args.api_token.map(|t| MaskedString::from(t.as_str())),
        })
    } else {
        None
    };

    let client_config = ClientConfig {
        remote_addr: remote,
        default_token: Some(MaskedString::from(token.as_str())),
        prefer_ipv6: None,
        services,
        transport: Default::default(),
        heartbeat_timeout: 40,
        retry_interval: 1,
        auto_register,
    };

    let config = Config {
        server: None,
        client: Some(client_config),
    };

    info!("Running in CLI mode");
    debug!("Config: {:?}", config);

    // Create a dummy update channel (no hot-reload in CLI mode)
    let (_update_tx, update_rx) = mpsc::channel(1);

    run_client(config, shutdown_rx, update_rx).await
}

#[cfg(not(feature = "client"))]
async fn run_cli_mode(
    _args: Cli,
    _remote: String,
    _shutdown_rx: broadcast::Receiver<bool>,
) -> Result<()> {
    crate::helper::feature_not_compile("client")
}

/// Run in config pull mode - fetch config from server
#[cfg(all(feature = "client", feature = "api"))]
async fn run_config_pull_mode(
    args: Cli,
    config_server: String,
    shutdown_rx: broadcast::Receiver<bool>,
) -> Result<()> {
    use api_client::ApiClient;
    use config::{ClientConfig, ClientServiceConfig, MaskedString, ServiceType};

    // Token is required
    let token = args.token.ok_or_else(|| {
        anyhow::anyhow!("--token is required when using --config-server")
    })?;

    info!("Fetching configuration from server: {}", config_server);

    // Create API client and pull config
    let api_client = ApiClient::new(&config_server, "");
    let pulled_config = api_client.pull_config(&token)?;

    // Convert pulled config to ClientConfig
    let mut services = HashMap::new();
    for (name, svc) in pulled_config.services {
        let service_type = match svc.service_type.as_str() {
            "udp" => ServiceType::Udp,
            _ => ServiceType::Tcp,
        };
        let service_config = ClientServiceConfig {
            service_type,
            name: name.clone(),
            local_addr: svc.local_addr,
            prefer_ipv6: false,
            token: Some(MaskedString::from(token.as_str())),
            nodelay: None,
            retry_interval: Some(1),
            bind_addr: svc.bind_addr,
        };
        services.insert(name, service_config);
    }

    if services.is_empty() {
        bail!("No services configured for token: {}", token);
    }

    let client_config = ClientConfig {
        remote_addr: pulled_config.remote_addr,
        default_token: Some(MaskedString::from(token.as_str())),
        prefer_ipv6: None,
        services,
        transport: Default::default(),
        heartbeat_timeout: 40,
        retry_interval: 1,
        auto_register: None,
    };

    let config = Config {
        server: None,
        client: Some(client_config),
    };

    info!("Running in config pull mode");
    debug!("Config: {:?}", config);

    let (_update_tx, update_rx) = mpsc::channel(1);
    run_client(config, shutdown_rx, update_rx).await
}

#[cfg(not(all(feature = "client", feature = "api")))]
async fn run_config_pull_mode(
    _args: Cli,
    _config_server: String,
    _shutdown_rx: broadcast::Receiver<bool>,
) -> Result<()> {
    crate::helper::feature_not_compile("client and api")
}

/// Run server in CLI mode
#[cfg(all(feature = "server", feature = "api"))]
async fn run_server_cli_mode(
    args: Cli,
    bind: String,
    shutdown_rx: broadcast::Receiver<bool>,
) -> Result<()> {
    use config::{ApiConfig, MaskedString, ServerConfig};

    // Token is required
    let token = args.token.ok_or_else(|| {
        anyhow::anyhow!("--token is required when using --bind")
    })?;

    // Build API config if specified
    let api = if let Some(api_bind) = args.api_bind {
        Some(ApiConfig {
            enabled: true,
            bind_addr: api_bind,
            token: args.api_token.map(|t| MaskedString::from(t.as_str())),
        })
    } else {
        None
    };

    let server_config = ServerConfig {
        bind_addr: bind,
        default_token: Some(MaskedString::from(token.as_str())),
        services: HashMap::new(),
        transport: Default::default(),
        heartbeat_interval: 30,
        api,
        client_configs: HashMap::new(),
    };

    let config = Config {
        server: Some(server_config),
        client: None,
    };

    info!("Running server in CLI mode");
    debug!("Config: {:?}", config);

    let (_update_tx, update_rx) = mpsc::channel(1);
    run_server_with_api(config, None, shutdown_rx, update_rx).await
}

#[cfg(all(feature = "server", not(feature = "api")))]
async fn run_server_cli_mode(
    args: Cli,
    bind: String,
    shutdown_rx: broadcast::Receiver<bool>,
) -> Result<()> {
    use config::{MaskedString, ServerConfig};

    let token = args.token.ok_or_else(|| {
        anyhow::anyhow!("--token is required when using --bind")
    })?;

    let server_config = ServerConfig {
        bind_addr: bind,
        default_token: Some(MaskedString::from(token.as_str())),
        services: HashMap::new(),
        transport: Default::default(),
        heartbeat_interval: 30,
        api: None,
        client_configs: HashMap::new(),
    };

    let config = Config {
        server: Some(server_config),
        client: None,
    };

    info!("Running server in CLI mode");
    let (_update_tx, update_rx) = mpsc::channel(1);
    run_server(config, shutdown_rx, update_rx).await
}

#[cfg(not(feature = "server"))]
async fn run_server_cli_mode(
    _args: Cli,
    _bind: String,
    _shutdown_rx: broadcast::Receiver<bool>,
) -> Result<()> {
    crate::helper::feature_not_compile("server")
}

fn get_str_from_keypair_type(curve: KeypairType) -> &'static str {
    match curve {
        KeypairType::X25519 => "25519",
        KeypairType::X448 => "448",
    }
}

#[cfg(feature = "noise")]
fn genkey(curve: Option<KeypairType>) -> Result<()> {
    let curve = curve.unwrap_or(DEFAULT_CURVE);
    let builder = snowstorm::Builder::new(
        format!(
            "Noise_KK_{}_ChaChaPoly_BLAKE2s",
            get_str_from_keypair_type(curve)
        )
        .parse()?,
    );
    let keypair = builder.generate_keypair()?;

    println!("Private Key:\n{}\n", base64::encode(keypair.private));
    println!("Public Key:\n{}", base64::encode(keypair.public));
    Ok(())
}

#[cfg(not(feature = "noise"))]
fn genkey(curve: Option<KeypairType>) -> Result<()> {
    crate::helper::feature_not_compile("nosie")
}

pub async fn run(args: Cli, shutdown_rx: broadcast::Receiver<bool>) -> Result<()> {
    if args.genkey.is_some() {
        return genkey(args.genkey.unwrap());
    }

    // Raise `nofile` limit on linux and mac
    fdlimit::raise_fd_limit();

    // Check if running in CLI mode
    // Client CLI mode: --remote specified
    if let Some(ref remote) = args.remote {
        return run_cli_mode(args.clone(), remote.clone(), shutdown_rx).await;
    }
    // Server CLI mode: --bind specified
    if let Some(ref bind) = args.bind {
        return run_server_cli_mode(args.clone(), bind.clone(), shutdown_rx).await;
    }
    // Config pull mode: --config-server specified
    if let Some(ref config_server) = args.config_server {
        return run_config_pull_mode(args.clone(), config_server.clone(), shutdown_rx).await;
    }

    // Spawn a config watcher. The watcher will send a initial signal to start the instance with a config
    let config_path = args.config_path.as_ref().unwrap().clone();
    let mut cfg_watcher = ConfigWatcherHandle::new(&config_path, shutdown_rx).await?;

    // shutdown_tx owns the instance
    let (shutdown_tx, _) = broadcast::channel(1);

    // (The join handle of the last instance, The service update channel sender)
    let mut last_instance: Option<(tokio::task::JoinHandle<_>, mpsc::Sender<ConfigChange>)> = None;

    while let Some(e) = cfg_watcher.event_rx.recv().await {
        match e {
            ConfigChange::General(config) => {
                if let Some((i, _)) = last_instance {
                    info!("General configuration change detected. Restarting...");
                    shutdown_tx.send(true)?;
                    i.await??;
                }

                debug!("{:?}", config);

                let (service_update_tx, service_update_rx) = mpsc::channel(1024);

                last_instance = Some((
                    tokio::spawn(run_instance(
                        *config,
                        args.clone(),
                        config_path.clone(),
                        shutdown_tx.subscribe(),
                        service_update_rx,
                    )),
                    service_update_tx,
                ));
            }
            ev => {
                info!("Service change detected. {:?}", ev);
                if let Some((_, service_update_tx)) = &last_instance {
                    let _ = service_update_tx.send(ev).await;
                }
            }
        }
    }

    let _ = shutdown_tx.send(true);

    Ok(())
}

async fn run_instance(
    config: Config,
    args: Cli,
    config_path: std::path::PathBuf,
    shutdown_rx: broadcast::Receiver<bool>,
    service_update: mpsc::Receiver<ConfigChange>,
) -> Result<()> {
    match determine_run_mode(&config, &args) {
        RunMode::Undetermine => panic!("Cannot determine running as a server or a client"),
        RunMode::Client => {
            #[cfg(not(feature = "client"))]
            crate::helper::feature_not_compile("client");
            #[cfg(feature = "client")]
            run_client(config, shutdown_rx, service_update).await
        }
        RunMode::Server => {
            #[cfg(not(feature = "server"))]
            crate::helper::feature_not_compile("server");
            #[cfg(all(feature = "server", feature = "api"))]
            {
                run_server_with_api(config, Some(config_path), shutdown_rx, service_update).await
            }
            #[cfg(all(feature = "server", not(feature = "api")))]
            {
                let _ = config_path; // Suppress unused warning
                run_server(config, shutdown_rx, service_update).await
            }
        }
    }
}

#[derive(PartialEq, Eq, Debug)]
enum RunMode {
    Server,
    Client,
    Undetermine,
}

fn determine_run_mode(config: &Config, args: &Cli) -> RunMode {
    use RunMode::*;
    if args.client && args.server {
        Undetermine
    } else if args.client {
        Client
    } else if args.server {
        Server
    } else if config.client.is_some() && config.server.is_none() {
        Client
    } else if config.server.is_some() && config.client.is_none() {
        Server
    } else {
        Undetermine
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_determine_run_mode() {
        use config::*;
        use RunMode::*;

        struct T {
            cfg_s: bool,
            cfg_c: bool,
            arg_s: bool,
            arg_c: bool,
            run_mode: RunMode,
        }

        let tests = [
            T {
                cfg_s: false,
                cfg_c: false,
                arg_s: false,
                arg_c: false,
                run_mode: Undetermine,
            },
            T {
                cfg_s: true,
                cfg_c: false,
                arg_s: false,
                arg_c: false,
                run_mode: Server,
            },
            T {
                cfg_s: false,
                cfg_c: true,
                arg_s: false,
                arg_c: false,
                run_mode: Client,
            },
            T {
                cfg_s: true,
                cfg_c: true,
                arg_s: false,
                arg_c: false,
                run_mode: Undetermine,
            },
            T {
                cfg_s: true,
                cfg_c: true,
                arg_s: true,
                arg_c: false,
                run_mode: Server,
            },
            T {
                cfg_s: true,
                cfg_c: true,
                arg_s: false,
                arg_c: true,
                run_mode: Client,
            },
            T {
                cfg_s: true,
                cfg_c: true,
                arg_s: true,
                arg_c: true,
                run_mode: Undetermine,
            },
        ];

        for t in tests {
            let config = Config {
                server: match t.cfg_s {
                    true => Some(ServerConfig::default()),
                    false => None,
                },
                client: match t.cfg_c {
                    true => Some(ClientConfig::default()),
                    false => None,
                },
            };

            let args = Cli {
                config_path: Some(std::path::PathBuf::new()),
                server: t.arg_s,
                client: t.arg_c,
                ..Default::default()
            };

            assert_eq!(determine_run_mode(&config, &args), t.run_mode);
        }
    }
}
