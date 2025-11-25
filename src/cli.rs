use clap::{AppSettings, ArgGroup, Parser};
use lazy_static::lazy_static;

#[derive(clap::ArgEnum, Clone, Debug, Copy)]
pub enum KeypairType {
    X25519,
    X448,
}

/// Parsed service definition from CLI
#[derive(Debug, Clone)]
pub struct ServiceDef {
    pub name: String,
    pub local_addr: String,
    pub bind_addr: Option<String>,
}

impl std::str::FromStr for ServiceDef {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // Format: name:local_addr[:bind_addr]
        // Example: my_svc:127.0.0.1:8080 or my_svc:127.0.0.1:8080:0.0.0.0:0
        let parts: Vec<&str> = s.splitn(2, ':').collect();
        if parts.len() < 2 {
            return Err("Service format: name:local_addr[:bind_addr]".to_string());
        }

        let name = parts[0].to_string();
        let rest = parts[1];

        // Parse local_addr (host:port) and optional bind_addr
        // Try to find the pattern: we need to extract local_addr which is host:port
        // If there are more colons after host:port, that's bind_addr
        let addr_parts: Vec<&str> = rest.split(':').collect();
        if addr_parts.len() < 2 {
            return Err("local_addr must be in host:port format".to_string());
        }

        let local_addr = format!("{}:{}", addr_parts[0], addr_parts[1]);
        let bind_addr = if addr_parts.len() >= 4 {
            Some(format!("{}:{}", addr_parts[2], addr_parts[3]))
        } else {
            None
        };

        Ok(ServiceDef {
            name,
            local_addr,
            bind_addr,
        })
    }
}

lazy_static! {
    static ref VERSION: &'static str =
        option_env!("VERGEN_GIT_SEMVER_LIGHTWEIGHT").unwrap_or(env!("VERGEN_BUILD_SEMVER"));
    static ref LONG_VERSION: String = format!(
        "
Build Timestamp:     {}
Build Version:       {}
Commit SHA:          {:?}
Commit Date:         {:?}
Commit Branch:       {:?}
cargo Target Triple: {}
cargo Profile:       {}
cargo Features:      {}
",
        env!("VERGEN_BUILD_TIMESTAMP"),
        env!("VERGEN_BUILD_SEMVER"),
        option_env!("VERGEN_GIT_SHA"),
        option_env!("VERGEN_GIT_COMMIT_TIMESTAMP"),
        option_env!("VERGEN_GIT_BRANCH"),
        env!("VERGEN_CARGO_TARGET_TRIPLE"),
        env!("VERGEN_CARGO_PROFILE"),
        env!("VERGEN_CARGO_FEATURES")
    );
}

#[derive(Parser, Debug, Default, Clone)]
#[clap(
    about,
    version(*VERSION),
    long_version(LONG_VERSION.as_str()),
    setting(AppSettings::DeriveDisplayOrder)
)]
#[clap(group(
            ArgGroup::new("cmds")
                .required(true)
                .args(&["CONFIG", "genkey", "remote", "bind"]),
        ))]
pub struct Cli {
    /// The path to the configuration file
    ///
    /// Running as a client or a server is automatically determined
    /// according to the configuration file.
    #[clap(parse(from_os_str), name = "CONFIG")]
    pub config_path: Option<std::path::PathBuf>,

    /// Run as a server
    #[clap(long, short, group = "mode")]
    pub server: bool,

    /// Run as a client
    #[clap(long, short, group = "mode")]
    pub client: bool,

    /// Generate a keypair for the use of the noise protocol
    ///
    /// The DH function to use is x25519
    #[clap(long, arg_enum, value_name = "CURVE")]
    pub genkey: Option<Option<KeypairType>>,

    // ===== CLI mode arguments for client =====

    /// Remote server address (enables CLI mode)
    ///
    /// Example: --remote n100:2333
    #[clap(long, value_name = "ADDR")]
    pub remote: Option<String>,

    /// Authentication token
    #[clap(long, value_name = "TOKEN")]
    pub token: Option<String>,

    /// API server address for auto-registration
    ///
    /// Example: --api n100:3000
    #[clap(long, value_name = "ADDR")]
    pub api: Option<String>,

    /// API authentication token
    #[clap(long, value_name = "TOKEN")]
    pub api_token: Option<String>,

    /// Service to forward (can be specified multiple times)
    ///
    /// Format: name:local_addr[:bind_addr]
    /// Examples:
    ///   -L web:127.0.0.1:8080
    ///   -L ssh:127.0.0.1:22:0.0.0.0:2222
    ///   -L db:127.0.0.1:3306:0.0.0.0:0
    #[clap(short = 'L', long = "local", value_name = "SERVICE", multiple_occurrences = true)]
    pub services: Vec<ServiceDef>,

    // ===== Server mode arguments =====

    /// Bind address for server mode
    ///
    /// Example: --bind 0.0.0.0:2333
    #[clap(long, value_name = "ADDR")]
    pub bind: Option<String>,

    /// API bind address for server mode
    ///
    /// Example: --api-bind 0.0.0.0:3000
    #[clap(long, value_name = "ADDR")]
    pub api_bind: Option<String>,
}
