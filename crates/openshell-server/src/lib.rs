// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `OpenShell` Server library.
//!
//! This crate provides the server implementation for `OpenShell`, including:
//! - gRPC service implementation
//! - HTTP health endpoints
//! - Protocol multiplexing (gRPC + HTTP on same port)
//! - mTLS support

mod auth;
mod compute;
mod grpc;
mod http;
mod inference;
mod multiplex;
mod persistence;
mod sandbox_index;
mod sandbox_watch;
mod ssh_tunnel;
mod tls;
pub mod tracing_bus;
mod ws_tunnel;

use openshell_core::{Config, Error, Result};
use std::collections::HashMap;
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;
use tracing::{debug, error, info};
use url::Url;

use compute::ComputeRuntime;
pub use grpc::OpenShellService;
pub use http::{health_router, http_router};
pub use multiplex::{MultiplexService, MultiplexedService};
use openshell_driver_kubernetes::KubernetesComputeConfig;
use persistence::Store;
use sandbox_index::SandboxIndex;
use sandbox_watch::SandboxWatchBus;
pub use tls::TlsAcceptor;
use tracing_bus::TracingLogBus;

const DEFAULT_COMPUTE_DRIVER_ENDPOINT: &str = "http://127.0.0.1:50061";

/// Server state shared across handlers.
#[derive(Debug)]
pub struct ServerState {
    /// Server configuration.
    pub config: Config,

    /// Persistence store.
    pub store: Arc<Store>,

    /// Compute orchestration over the configured driver.
    pub compute: ComputeRuntime,

    /// In-memory sandbox correlation index.
    pub sandbox_index: SandboxIndex,

    /// In-memory bus for sandbox update notifications.
    pub sandbox_watch_bus: SandboxWatchBus,

    /// In-memory bus for server process logs.
    pub tracing_log_bus: TracingLogBus,

    /// Active SSH tunnel connection counts per session token.
    pub ssh_connections_by_token: Mutex<HashMap<String, u32>>,

    /// Active SSH tunnel connection counts per sandbox id.
    pub ssh_connections_by_sandbox: Mutex<HashMap<String, u32>>,

    /// Serializes settings mutations (global and sandbox) to prevent
    /// read-modify-write races. Held for the duration of any setting
    /// set/delete operation, including the precedence check on sandbox
    /// mutations that reads global state.
    pub settings_mutex: tokio::sync::Mutex<()>,
}

fn is_benign_tls_handshake_failure(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        ErrorKind::UnexpectedEof | ErrorKind::ConnectionReset
    )
}

impl ServerState {
    /// Create new server state.
    #[must_use]
    pub fn new(
        config: Config,
        store: Arc<Store>,
        compute: ComputeRuntime,
        sandbox_index: SandboxIndex,
        sandbox_watch_bus: SandboxWatchBus,
        tracing_log_bus: TracingLogBus,
    ) -> Self {
        Self {
            config,
            store,
            compute,
            sandbox_index,
            sandbox_watch_bus,
            tracing_log_bus,
            ssh_connections_by_token: Mutex::new(HashMap::new()),
            ssh_connections_by_sandbox: Mutex::new(HashMap::new()),
            settings_mutex: tokio::sync::Mutex::new(()),
        }
    }
}

#[derive(Debug)]
struct ManagedComputeDriver {
    child: Child,
}

impl ManagedComputeDriver {
    fn spawn(binary: &Path, bind_address: SocketAddr, config: &Config) -> Result<Self> {
        let mut command = Command::new(binary);
        command
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .env("OPENSHELL_COMPUTE_DRIVER_BIND", bind_address.to_string())
            .env("OPENSHELL_GRPC_ENDPOINT", &config.grpc_endpoint)
            .env("OPENSHELL_LOG_LEVEL", &config.log_level)
            .env(
                "OPENSHELL_SSH_HANDSHAKE_SECRET",
                &config.ssh_handshake_secret,
            )
            .env(
                "OPENSHELL_SSH_HANDSHAKE_SKEW_SECS",
                config.ssh_handshake_skew_secs.to_string(),
            );

        if let Some(tls) = &config.tls {
            command
                .env("OPENSHELL_TLS_CA", &tls.client_ca_path)
                .env("OPENSHELL_TLS_CERT", &tls.cert_path)
                .env("OPENSHELL_TLS_KEY", &tls.key_path);
        }

        let child = command.spawn().map_err(|e| {
            Error::execution(format!(
                "failed to spawn compute driver '{}': {e}",
                binary.display()
            ))
        })?;
        Ok(Self { child })
    }
}

impl Drop for ManagedComputeDriver {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Run the `OpenShell` server.
///
/// This starts a multiplexed gRPC/HTTP server on the configured bind address.
///
/// # Errors
///
/// Returns an error if the server fails to start or encounters a fatal error.
pub async fn run_server(config: Config, tracing_log_bus: TracingLogBus) -> Result<()> {
    let database_url = config.database_url.trim();
    if database_url.is_empty() {
        return Err(Error::config("database_url is required"));
    }
    if config.ssh_handshake_secret.is_empty() {
        return Err(Error::config(
            "ssh_handshake_secret is required. Set --ssh-handshake-secret or OPENSHELL_SSH_HANDSHAKE_SECRET",
        ));
    }

    let store = Arc::new(Store::connect(database_url).await?);

    let sandbox_index = SandboxIndex::new();
    let sandbox_watch_bus = SandboxWatchBus::new();
    let compute_driver_endpoint = effective_compute_driver_endpoint(&config)?;
    let _managed_compute_driver = if let Some(binary) = config.compute_driver_bin.as_deref() {
        let bind_address = compute_driver_bind_address(
            compute_driver_endpoint
                .as_deref()
                .expect("compute driver endpoint is set when launching a managed driver"),
        )?;
        Some(ManagedComputeDriver::spawn(binary, bind_address, &config)?)
    } else {
        None
    };

    let compute = if let Some(endpoint) = compute_driver_endpoint.as_deref() {
        ComputeRuntime::new_grpc(
            endpoint,
            store.clone(),
            sandbox_index.clone(),
            sandbox_watch_bus.clone(),
            tracing_log_bus.clone(),
        )
        .await
        .map_err(|e| Error::execution(format!("failed to create compute runtime: {e}")))?
    } else {
        ComputeRuntime::new_kubernetes(
            KubernetesComputeConfig {
                namespace: config.sandbox_namespace.clone(),
                default_image: config.sandbox_image.clone(),
                image_pull_policy: config.sandbox_image_pull_policy.clone(),
                grpc_endpoint: config.grpc_endpoint.clone(),
                ssh_listen_addr: format!("0.0.0.0:{}", config.sandbox_ssh_port),
                ssh_port: config.sandbox_ssh_port,
                ssh_handshake_secret: config.ssh_handshake_secret.clone(),
                ssh_handshake_skew_secs: config.ssh_handshake_skew_secs,
                client_tls_secret_name: config.client_tls_secret_name.clone(),
                host_gateway_ip: config.host_gateway_ip.clone(),
            },
            store.clone(),
            sandbox_index.clone(),
            sandbox_watch_bus.clone(),
            tracing_log_bus.clone(),
        )
        .await
        .map_err(|e| Error::execution(format!("failed to create compute runtime: {e}")))?
    };
    let state = Arc::new(ServerState::new(
        config.clone(),
        store.clone(),
        compute,
        sandbox_index,
        sandbox_watch_bus,
        tracing_log_bus,
    ));

    state.compute.spawn_watchers();
    ssh_tunnel::spawn_session_reaper(store.clone(), std::time::Duration::from_secs(3600));

    // Create the multiplexed service
    let service = MultiplexService::new(state.clone());

    // Bind the TCP listener
    let listener = TcpListener::bind(config.bind_address)
        .await
        .map_err(|e| Error::transport(format!("failed to bind to {}: {e}", config.bind_address)))?;

    info!(address = %config.bind_address, "Server listening");

    // Build TLS acceptor when TLS is configured; otherwise serve plaintext.
    let tls_acceptor = if let Some(tls) = &config.tls {
        Some(TlsAcceptor::from_files(
            &tls.cert_path,
            &tls.key_path,
            &tls.client_ca_path,
            tls.allow_unauthenticated,
        )?)
    } else {
        info!("TLS disabled — accepting plaintext connections");
        None
    };

    // Accept connections
    loop {
        let (stream, addr) = match listener.accept().await {
            Ok(conn) => conn,
            Err(e) => {
                error!(error = %e, "Failed to accept connection");
                continue;
            }
        };

        let service = service.clone();

        if let Some(ref acceptor) = tls_acceptor {
            let tls_acceptor = acceptor.clone();
            tokio::spawn(async move {
                match tls_acceptor.inner().accept(stream).await {
                    Ok(tls_stream) => {
                        if let Err(e) = service.serve(tls_stream).await {
                            error!(error = %e, client = %addr, "Connection error");
                        }
                    }
                    Err(e) => {
                        if is_benign_tls_handshake_failure(&e) {
                            debug!(error = %e, client = %addr, "TLS handshake closed early");
                        } else {
                            error!(error = %e, client = %addr, "TLS handshake failed");
                        }
                    }
                }
            });
        } else {
            tokio::spawn(async move {
                if let Err(e) = service.serve(stream).await {
                    error!(error = %e, client = %addr, "Connection error");
                }
            });
        }
    }
}

fn effective_compute_driver_endpoint(config: &Config) -> Result<Option<String>> {
    if !config.compute_driver_endpoint.trim().is_empty() {
        return Ok(Some(config.compute_driver_endpoint.clone()));
    }
    if config.compute_driver_bin.is_some() {
        return Ok(Some(DEFAULT_COMPUTE_DRIVER_ENDPOINT.to_string()));
    }
    Ok(None)
}

fn compute_driver_bind_address(endpoint: &str) -> Result<SocketAddr> {
    let parsed = Url::parse(endpoint)
        .map_err(|e| Error::config(format!("invalid compute driver endpoint '{endpoint}': {e}")))?;
    let host = parsed.host_str().ok_or_else(|| {
        Error::config(format!(
            "compute driver endpoint '{endpoint}' must include a host"
        ))
    })?;
    let port = parsed.port_or_known_default().ok_or_else(|| {
        Error::config(format!(
            "compute driver endpoint '{endpoint}' must include a port"
        ))
    })?;
    format!("{host}:{port}")
        .parse()
        .map_err(|e| Error::config(format!("invalid compute driver bind address: {e}")))
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_COMPUTE_DRIVER_ENDPOINT, compute_driver_bind_address,
        effective_compute_driver_endpoint, is_benign_tls_handshake_failure,
    };
    use openshell_core::Config;
    use std::io::{Error, ErrorKind};
    use std::path::PathBuf;

    #[test]
    fn classifies_probe_style_tls_disconnects_as_benign() {
        for kind in [ErrorKind::UnexpectedEof, ErrorKind::ConnectionReset] {
            let error = Error::new(kind, "probe disconnected");
            assert!(is_benign_tls_handshake_failure(&error));
        }
    }

    #[test]
    fn preserves_real_tls_failures_as_errors() {
        for kind in [
            ErrorKind::InvalidData,
            ErrorKind::PermissionDenied,
            ErrorKind::Other,
        ] {
            let error = Error::new(kind, "real tls failure");
            assert!(!is_benign_tls_handshake_failure(&error));
        }
    }

    #[test]
    fn managed_compute_driver_defaults_endpoint() {
        let config = Config::new(None).with_compute_driver_bin(PathBuf::from("/tmp/driver"));
        assert_eq!(
            effective_compute_driver_endpoint(&config).unwrap(),
            Some(DEFAULT_COMPUTE_DRIVER_ENDPOINT.to_string())
        );
    }

    #[test]
    fn compute_driver_bind_address_uses_host_and_port() {
        assert_eq!(
            compute_driver_bind_address("http://127.0.0.1:50061").unwrap(),
            "127.0.0.1:50061".parse().unwrap()
        );
    }
}
