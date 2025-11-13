#![forbid(unsafe_code)]
#![deny(clippy::all, clippy::unwrap_used, clippy::expect_used)]
#![warn(rust_2018_idioms, rust_2021_compatibility)]

use anyhow::{Context as AnyhowContext, Result};
use clap::Parser;
use futures::future::BoxFuture;
use futures::FutureExt;

use serde_json::json;
use std::collections::HashMap;

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio::time::timeout;
use tokio_modbus::{
    client::{tcp::connect, Context as ClientContext, Reader, Writer},
    prelude::SlaveContext,
    server::{
        tcp::{accept_tcp_connection, Server},
        Service,
    },
    slave::Slave,
    ExceptionCode, Request, Response, SlaveRequest,
};
use tracing::{error, info, instrument, trace, warn};
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::{
    fmt::time::LocalTime,
    layer::{Layer, SubscriberExt},
    util::SubscriberInitExt,
    EnvFilter,
};

/// Guard that logs when a client connection is dropped/disconnected
/// Uses Arc to ensure only one log message is produced even if cloned
#[derive(Clone)]
struct ConnectionGuard {
    inner: Arc<ConnectionGuardInner>,
}

struct ConnectionGuardInner {
    peer_addr: SocketAddr,
    target: SocketAddr,
    logged: AtomicBool,
    metrics: Option<Arc<Metrics>>,
}

impl ConnectionGuard {
    #[instrument(skip_all, fields(peer_addr = %peer_addr, target = %target))]
    #[allow(dead_code)]
    fn new(peer_addr: SocketAddr, target: SocketAddr) -> Self {
        info!("Creating connection guard");
        Self {
            inner: Arc::new(ConnectionGuardInner {
                peer_addr,
                target,
                logged: AtomicBool::new(false),
                metrics: None,
            }),
        }
    }

    fn with_metrics(peer_addr: SocketAddr, target: SocketAddr, metrics: Arc<Metrics>) -> Self {
        Self {
            inner: Arc::new(ConnectionGuardInner {
                peer_addr,
                target,
                logged: AtomicBool::new(false),
                metrics: Some(metrics),
            }),
        }
    }
}

impl Drop for ConnectionGuard {
    #[instrument(skip_all, fields(peer_addr = %self.inner.peer_addr, target = %self.inner.target))]
    fn drop(&mut self) {
        trace!("ConnectionGuard::drop() called");
        // Only log if this is the last reference and we haven't logged yet
        if Arc::strong_count(&self.inner) == 1 && !self.inner.logged.swap(true, Ordering::SeqCst) {
            info!(
                client_disconnected = true,
                client_addr = %self.inner.peer_addr,
                target_addr = %self.inner.target,
                "Client disconnected from proxy"
            );

            // Update metrics if available
            if let Some(metrics) = &self.inner.metrics {
                metrics.decrement_client_connection();
                let active_connections = metrics.get_active_connections();
                info!(target: "metrics::connections", "Active client connections after disconnect: {}", active_connections);
            }
        }
    }
}

/// Initialize structured tracing with JSON output and file/console separation
fn init_tracing(log_dir: &str, target_addr: &str, no_console: bool) -> Result<()> {
    // Use RUST_LOG environment variable or default to "info"
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    // Sanitize target address for use in log filename
    let sanitized_target = target_addr.replace(':', "-");
    let log_file_prefix = format!("modbus-proxy.{}", sanitized_target);
    let metrics_file_prefix = format!("modbus-proxy.{}.metrics", sanitized_target);

    // Create rolling file appenders (daily rotation)
    let regular_file_appender = RollingFileAppender::new(Rotation::DAILY, log_dir, log_file_prefix);

    let metrics_file_appender =
        RollingFileAppender::new(Rotation::DAILY, log_dir, metrics_file_prefix);

    // Create regular file layer (for all logs except metrics)
    let regular_file_layer = tracing_subscriber::fmt::layer()
        .with_timer(LocalTime::rfc_3339())
        .with_writer(regular_file_appender)
        .json() // Enable JSON output for structured logs
        .with_current_span(true)
        .with_span_list(true)
        .with_filter(tracing_subscriber::filter::filter_fn(|meta| {
            // Exclude logs that contain "metrics" in the target
            !meta.target().contains("metrics") && !meta.name().contains("metrics")
        }));

    // Create metrics file layer (for metrics logs only)
    let metrics_file_layer = tracing_subscriber::fmt::layer()
        .with_timer(LocalTime::rfc_3339())
        .with_writer(metrics_file_appender)
        .json()
        .with_current_span(true)
        .with_filter(tracing_subscriber::filter::LevelFilter::INFO)
        .with_filter(tracing_subscriber::filter::filter_fn(|meta| {
            meta.target().contains("metrics") || meta.name().contains("metrics")
        }));

    // Build the subscriber based on console preference
    if no_console {
        tracing_subscriber::registry()
            .with(env_filter)
            .with(regular_file_layer)
            .with(metrics_file_layer)
            .try_init()
            .context("Failed to set tracing subscriber")?;
    } else {
        let console_layer = tracing_subscriber::fmt::layer()
            .with_timer(LocalTime::rfc_3339())
            .with_writer(std::io::stdout)
            .pretty() // Pretty formatting for console
            .with_filter(env_filter.clone());

        tracing_subscriber::registry()
            .with(env_filter)
            .with(regular_file_layer)
            .with(metrics_file_layer)
            .with(console_layer)
            .try_init()
            .context("Failed to set tracing subscriber")?;
    }

    info!(target: "tracing::init", "Structured tracing initialized with JSON formatting");
    Ok(())
}

/// A strict Modbus TCP proxy: forwards requests to a target and relays responses/exceptions.
/// Unit‑ID is **passed through** from the client request.
#[derive(Parser, Debug)]
#[command(name = "modbus-proxy")]
#[command(about = "Strict Modbus TCP proxy with Unit‑ID passthrough")]
#[command(version)]
struct Cli {
    /// Target Modbus server address, e.g. 192.168.1.100:502
    #[arg(
        short = 't',
        long = "target",
        value_name = "ADDR",
        required_unless_present = "metrics_json"
    )]
    target: Option<String>,

    /// Listen address for the proxy (the server side).
    #[arg(
        short = 'l',
        long = "listen",
        default_value = "0.0.0.0:5020",
        value_name = "ADDR"
    )]
    listen_addr: String,

    /// Strict mode: map transport/protocol errors to GatewayPathUnavailable (0x0A).
    #[arg(long = "strict", default_value_t = true)]
    strict: bool,

    /// Log directory for file output (default: current directory)
    #[arg(long = "log-dir", default_value = "./logs", value_name = "DIR")]
    log_dir: String,

    /// Backend timeout in seconds for upstream operations (default: 30)
    #[arg(long = "backend-timeout", default_value = "30", value_name = "SECS")]
    backend_timeout: u64,

    /// Output JSON metrics snapshot and exit (useful for monitoring scripts)
    #[arg(long = "metrics-json", conflicts_with_all = ["target", "listen_addr"])]
    metrics_json: bool,

    /// Disable console logging (only write to log files)
    #[arg(long = "no-console", default_value_t = false)]
    no_console: bool,
}

/// Metrics tracking for proxy operations
#[derive(Clone)]
struct Metrics {
    /// Total client connections
    client_connections_total: Arc<AtomicU64>,
    /// Currently active client connections
    client_connections_active: Arc<AtomicU64>,
    /// Total connections to target server
    target_connections_total: Arc<AtomicU64>,
    /// Request counters per function code (0x01-0x06, 0x0F, 0x10)
    request_counters: Arc<[AtomicU64; 8]>,
    /// Target server status (connected/disconnected)
    target_status: Arc<Mutex<HashMap<SocketAddr, bool>>>,
}

impl Metrics {
    fn new() -> Self {
        Self {
            client_connections_total: Arc::new(AtomicU64::new(0)),
            client_connections_active: Arc::new(AtomicU64::new(0)),
            target_connections_total: Arc::new(AtomicU64::new(0)),
            request_counters: Arc::new([
                AtomicU64::new(0), // 0x01: ReadCoils
                AtomicU64::new(0), // 0x02: ReadDiscreteInputs
                AtomicU64::new(0), // 0x03: ReadHoldingRegisters
                AtomicU64::new(0), // 0x04: ReadInputRegisters
                AtomicU64::new(0), // 0x05: WriteSingleCoil
                AtomicU64::new(0), // 0x06: WriteSingleRegister
                AtomicU64::new(0), // 0x0F: WriteMultipleCoils
                AtomicU64::new(0), // 0x10: WriteMultipleRegisters
            ]),
            target_status: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn increment_client_connection(&self) {
        self.client_connections_total
            .fetch_add(1, Ordering::Relaxed);
        self.client_connections_active
            .fetch_add(1, Ordering::Relaxed);
    }

    fn decrement_client_connection(&self) {
        self.client_connections_active
            .fetch_sub(1, Ordering::Relaxed);
    }

    fn increment_target_connection(&self) {
        self.target_connections_total
            .fetch_add(1, Ordering::Relaxed);
    }

    fn increment_request_counter(&self, function_code: u8) {
        let index = match function_code {
            0x01 => 0,
            0x02 => 1,
            0x03 => 2,
            0x04 => 3,
            0x05 => 4,
            0x06 => 5,
            0x0F => 6,
            0x10 => 7,
            _ => return, // Ignore unsupported function codes
        };
        self.request_counters[index].fetch_add(1, Ordering::Relaxed);
    }

    fn get_active_connections(&self) -> u64 {
        self.client_connections_active.load(Ordering::Relaxed)
    }

    #[allow(dead_code)]
    fn get_total_connections(&self) -> u64 {
        self.client_connections_total.load(Ordering::Relaxed)
    }

    #[allow(dead_code)]
    fn get_request_count(&self, function_code: u8) -> u64 {
        let index = match function_code {
            0x01 => 0,
            0x02 => 1,
            0x03 => 2,
            0x04 => 3,
            0x05 => 4,
            0x06 => 5,
            0x0F => 6,
            0x10 => 7,
            _ => return 0,
        };
        self.request_counters[index].load(Ordering::Relaxed)
    }

    /// Generate a JSON snapshot of current metrics
    async fn to_json_snapshot(&self) -> serde_json::Value {
        let target_status = self.target_status.lock().await;

        json!({
            "timestamp": chrono::Local::now().to_rfc3339(),
            "connections": {
                "client": {
                    "total": self.client_connections_total.load(Ordering::Relaxed),
                    "active": self.client_connections_active.load(Ordering::Relaxed)
                },
                "target": {
                    "total": self.target_connections_total.load(Ordering::Relaxed),
                    "status": target_status.iter().map(|(addr, connected)| {
                        json!({
                            "address": addr.to_string(),
                            "connected": connected
                        })
                    }).collect::<Vec<_>>()
                }
            },
            "requests": {
                "read_coils": self.request_counters[0].load(Ordering::Relaxed),
                "read_discrete_inputs": self.request_counters[1].load(Ordering::Relaxed),
                "read_holding_registers": self.request_counters[2].load(Ordering::Relaxed),
                "read_input_registers": self.request_counters[3].load(Ordering::Relaxed),
                "write_single_coil": self.request_counters[4].load(Ordering::Relaxed),
                "write_single_register": self.request_counters[5].load(Ordering::Relaxed),
                "write_multiple_coils": self.request_counters[6].load(Ordering::Relaxed),
                "write_multiple_registers": self.request_counters[7].load(Ordering::Relaxed)
            }
        })
    }
}

/// Per-connection service that forwards requests to the configured target.
/// Reuses a single upstream TCP connection guarded by a mutex.
struct ProxyService {
    target: SocketAddr,
    strict: bool,
    backend_timeout: Duration,
    client: Arc<Mutex<Option<ClientContext>>>,
    _guard: ConnectionGuard,
    metrics: Arc<Metrics>,
}

impl std::fmt::Debug for ProxyService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyService")
            .field("target", &self.target)
            .field("strict", &self.strict)
            .field("client", &"<Mutex<Option<ClientContext>>>")
            .finish()
    }
}

impl ProxyService {
    #[instrument(skip(guard, metrics), fields(target = %target, strict = %strict))]
    fn new(
        target: SocketAddr,
        strict: bool,
        backend_timeout: Duration,
        guard: ConnectionGuard,
        metrics: Arc<Metrics>,
    ) -> Self {
        Self {
            target,
            client: Arc::new(Mutex::new(None)),
            strict,
            backend_timeout,
            _guard: guard,
            metrics,
        }
    }

    #[instrument(skip(self), fields(target = %self.target))]
    async fn ensure_connected(&self) -> std::result::Result<(), ExceptionCode> {
        trace!("Checking connection to target");
        {
            let guard = self.client.lock().await;
            if guard.is_some() {
                return Ok(());
            }
            // drop guard here to avoid holding mutex while dialing
        }

        // Dial without holding the mutex.
        info!(target_connecting = %self.target, "Connecting to target");
        match timeout(self.backend_timeout, connect(self.target)).await {
            Ok(Ok(ctx)) => {
                let mut guard = self.client.lock().await;
                // Another task may have set it meanwhile — only set if still None.
                if guard.is_none() {
                    *guard = Some(ctx);
                    info!(target: "metrics::target", "Connected to target: {}", self.target);

                    // Update metrics
                    self.metrics.increment_target_connection();
                    self.metrics
                        .target_status
                        .lock()
                        .await
                        .insert(self.target, true);
                    info!(target: "metrics::target", "Target marked as connected: {}", self.target);
                } else {
                    info!("Upstream connection already established by another task");
                    // ctx will be dropped here (closing the extra connection).
                }
                Ok(())
            }
            Ok(Err(e)) => {
                warn!(target_addr = %self.target, error = %e, "Failed to connect to target");
                // Update metrics - mark target as disconnected
                self.metrics
                    .target_status
                    .lock()
                    .await
                    .insert(self.target, false);
                info!(target: "metrics::target", "Target {} marked as disconnected", self.target);
                Err(ExceptionCode::GatewayPathUnavailable)
            }
            Err(_) => {
                warn!(
                    "Connection to target {} timed out after {:?}",
                    self.target, self.backend_timeout
                );
                // Update metrics - mark target as disconnected
                self.metrics
                    .target_status
                    .lock()
                    .await
                    .insert(self.target, false);
                info!(target: "metrics::target", "Target {} marked as disconnected due to timeout", self.target);
                Err(ExceptionCode::GatewayPathUnavailable)
            }
        }
    }

    fn map_client_error(&self, _e: tokio_modbus::Error) -> ExceptionCode {
        if self.strict {
            ExceptionCode::GatewayPathUnavailable
        } else {
            ExceptionCode::ServerDeviceFailure
        }
    }

    #[instrument(skip(self, req), fields(unit_id = unit.0, function_code))]
    async fn forward(
        &self,
        unit: Slave,
        req: Request<'static>,
    ) -> std::result::Result<Response, ExceptionCode> {
        self.ensure_connected().await?;

        // Hold the client lock during the call to serialize requests on one TCP connection.
        let mut guard = self.client.lock().await;
        let ctx = guard.as_mut().ok_or_else(|| {
            error!("Client disconnected unexpectedly");
            ExceptionCode::GatewayPathUnavailable
        })?;

        // Set the Unit‑ID from the client request (passthrough).
        ctx.set_slave(unit);

        // Forward by matching request and calling the appropriate client method.
        // On any transport/protocol error, drop the connection to force reconnect next time.

        // Increment request counter for metrics (based on function code)
        let function_code = match req {
            Request::ReadCoils(_, _) => 0x01,
            Request::ReadDiscreteInputs(_, _) => 0x02,
            Request::ReadHoldingRegisters(_, _) => 0x03,
            Request::ReadInputRegisters(_, _) => 0x04,
            Request::WriteSingleCoil(_, _) => 0x05,
            Request::WriteSingleRegister(_, _) => 0x06,
            Request::WriteMultipleCoils(_, _) => 0x0F,
            Request::WriteMultipleRegisters(_, _) => 0x10,
            _ => 0x00, // Unsupported function code
        };

        // Add function code to the span
        tracing::Span::current().record("function_code", format!("0x{:02X}", function_code));

        info!(
            request_function_code = format!("0x{:02X}", function_code),
            request_unit_id = unit.0,
            "Processing Modbus request"
        );

        if function_code != 0x00 {
            self.metrics.increment_request_counter(function_code);
            info!(target: "metrics::requests", "Request counter incremented for function code 0x{:02X}", function_code);
        }

        match req {
            Request::ReadCoils(addr, cnt) => {
                match timeout(self.backend_timeout, ctx.read_coils(addr, cnt)).await {
                    Ok(Ok(Ok(data))) => Ok(Response::ReadCoils(data)),
                    Ok(Ok(Err(exc))) => Err(exc),
                    Ok(Err(e)) => {
                        warn!("Upstream transport error on read_coils: {}", e);
                        *guard = None;
                        Err(self.map_client_error(e))
                    }
                    Err(_) => {
                        warn!(
                            "Upstream request read_coils timed out after {:?}",
                            self.backend_timeout
                        );
                        *guard = None;
                        Err(self.map_client_error(tokio_modbus::Error::Transport(
                            std::io::Error::new(std::io::ErrorKind::TimedOut, "request timeout"),
                        )))
                    }
                }
            }
            Request::ReadDiscreteInputs(addr, cnt) => {
                match timeout(self.backend_timeout, ctx.read_discrete_inputs(addr, cnt)).await {
                    Ok(Ok(Ok(data))) => Ok(Response::ReadDiscreteInputs(data)),
                    Ok(Ok(Err(exc))) => Err(exc),
                    Ok(Err(e)) => {
                        warn!("Upstream transport error on read_discrete_inputs: {}", e);
                        *guard = None;
                        Err(self.map_client_error(e))
                    }
                    Err(_) => {
                        warn!(
                            "Upstream request read_discrete_inputs timed out after {:?}",
                            self.backend_timeout
                        );
                        *guard = None;
                        Err(self.map_client_error(tokio_modbus::Error::Transport(
                            std::io::Error::new(std::io::ErrorKind::TimedOut, "request timeout"),
                        )))
                    }
                }
            }
            Request::ReadInputRegisters(addr, cnt) => {
                match timeout(self.backend_timeout, ctx.read_input_registers(addr, cnt)).await {
                    Ok(Ok(Ok(data))) => Ok(Response::ReadInputRegisters(data)),
                    Ok(Ok(Err(exc))) => Err(exc),
                    Ok(Err(e)) => {
                        warn!("Upstream transport error on read_input_registers: {}", e);
                        *guard = None;
                        Err(self.map_client_error(e))
                    }
                    Err(_) => {
                        warn!(
                            "Upstream request read_input_registers timed out after {:?}",
                            self.backend_timeout
                        );
                        *guard = None;
                        Err(self.map_client_error(tokio_modbus::Error::Transport(
                            std::io::Error::new(std::io::ErrorKind::TimedOut, "request timeout"),
                        )))
                    }
                }
            }
            Request::ReadHoldingRegisters(addr, cnt) => {
                match timeout(self.backend_timeout, ctx.read_holding_registers(addr, cnt)).await {
                    Ok(Ok(Ok(data))) => Ok(Response::ReadHoldingRegisters(data)),
                    Ok(Ok(Err(exc))) => Err(exc),
                    Ok(Err(e)) => {
                        warn!("Upstream transport error on read_holding_registers: {}", e);
                        *guard = None;
                        Err(self.map_client_error(e))
                    }
                    Err(_) => {
                        warn!(
                            "Upstream request read_holding_registers timed out after {:?}",
                            self.backend_timeout
                        );
                        *guard = None;
                        Err(self.map_client_error(tokio_modbus::Error::Transport(
                            std::io::Error::new(std::io::ErrorKind::TimedOut, "request timeout"),
                        )))
                    }
                }
            }
            Request::WriteSingleCoil(addr, value) => {
                match timeout(self.backend_timeout, ctx.write_single_coil(addr, value)).await {
                    Ok(Ok(Ok(()))) => Ok(Response::WriteSingleCoil(addr, value)),
                    Ok(Ok(Err(exc))) => Err(exc),
                    Ok(Err(e)) => {
                        warn!("Upstream transport error on write_single_coil: {}", e);
                        *guard = None;
                        Err(self.map_client_error(e))
                    }
                    Err(_) => {
                        warn!(
                            "Upstream request write_single_coil timed out after {:?}",
                            self.backend_timeout
                        );
                        *guard = None;
                        Err(self.map_client_error(tokio_modbus::Error::Transport(
                            std::io::Error::new(std::io::ErrorKind::TimedOut, "request timeout"),
                        )))
                    }
                }
            }
            Request::WriteSingleRegister(addr, value) => {
                match timeout(self.backend_timeout, ctx.write_single_register(addr, value)).await {
                    Ok(Ok(Ok(()))) => Ok(Response::WriteSingleRegister(addr, value)),
                    Ok(Ok(Err(exc))) => Err(exc),
                    Ok(Err(e)) => {
                        warn!("Upstream transport error on write_single_register: {}", e);
                        *guard = None;
                        Err(self.map_client_error(e))
                    }
                    Err(_) => {
                        warn!(
                            "Upstream request write_single_register timed out after {:?}",
                            self.backend_timeout
                        );
                        *guard = None;
                        Err(self.map_client_error(tokio_modbus::Error::Transport(
                            std::io::Error::new(std::io::ErrorKind::TimedOut, "request timeout"),
                        )))
                    }
                }
            }
            Request::WriteMultipleCoils(addr, values) => {
                // Convert Cow<'_, [bool]> to &[bool]
                match timeout(
                    self.backend_timeout,
                    ctx.write_multiple_coils(addr, values.as_ref()),
                )
                .await
                {
                    Ok(Ok(Ok(()))) => Ok(Response::WriteMultipleCoils(addr, values.len() as u16)),
                    Ok(Ok(Err(exc))) => Err(exc),
                    Ok(Err(e)) => {
                        warn!("Upstream transport error on write_multiple_coils: {}", e);
                        *guard = None;
                        Err(self.map_client_error(e))
                    }
                    Err(_) => {
                        warn!(
                            "Upstream request write_multiple_coils timed out after {:?}",
                            self.backend_timeout
                        );
                        *guard = None;
                        Err(self.map_client_error(tokio_modbus::Error::Transport(
                            std::io::Error::new(std::io::ErrorKind::TimedOut, "request timeout"),
                        )))
                    }
                }
            }
            Request::WriteMultipleRegisters(addr, values) => {
                // Convert Cow<'_, [u16]> to &[u16]
                match timeout(
                    self.backend_timeout,
                    ctx.write_multiple_registers(addr, values.as_ref()),
                )
                .await
                {
                    Ok(Ok(Ok(()))) => {
                        Ok(Response::WriteMultipleRegisters(addr, values.len() as u16))
                    }
                    Ok(Ok(Err(exc))) => Err(exc),
                    Ok(Err(e)) => {
                        warn!(
                            "Upstream transport error on write_multiple_registers: {}",
                            e
                        );
                        *guard = None;
                        Err(self.map_client_error(e))
                    }
                    Err(_) => {
                        warn!(
                            "Upstream request write_multiple_registers timed out after {:?}",
                            self.backend_timeout
                        );
                        *guard = None;
                        Err(self.map_client_error(tokio_modbus::Error::Transport(
                            std::io::Error::new(std::io::ErrorKind::TimedOut, "request timeout"),
                        )))
                    }
                }
            }
            // Strict proxy: reject function codes we don't explicitly forward.
            other => {
                warn!("Illegal/unsupported function from client: {:?}", other);
                Err(ExceptionCode::IllegalFunction)
            }
        }
    }
}

impl Clone for ProxyService {
    fn clone(&self) -> Self {
        Self {
            target: self.target,
            strict: self.strict,
            backend_timeout: self.backend_timeout,
            client: Arc::clone(&self.client),
            _guard: self._guard.clone(),
            metrics: Arc::clone(&self.metrics),
        }
    }
}

impl Service for ProxyService {
    // We accept SlaveRequest to get the client's Unit‑ID.
    type Request = SlaveRequest<'static>;
    type Response = Response;
    type Exception = ExceptionCode;
    type Future = BoxFuture<'static, std::result::Result<Self::Response, Self::Exception>>;

    fn call(&self, req: Self::Request) -> Self::Future {
        let unit = Slave(req.slave);
        let this = self.clone();
        async move { this.forward(unit, req.request).await }.boxed()
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Handle --metrics-json flag: output snapshot and exit
    if cli.metrics_json {
        let metrics = Metrics::new();
        let snapshot = metrics.to_json_snapshot().await;
        match serde_json::to_string_pretty(&snapshot) {
            Ok(json) => println!("{}", json),
            Err(e) => {
                eprintln!("Failed to serialize metrics snapshot: {}", e);
                return Err(anyhow::anyhow!("Failed to serialize metrics snapshot"));
            }
        }
        return Ok(());
    }

    // Create log directory if it doesn't exist
    std::fs::create_dir_all(&cli.log_dir)
        .with_context(|| format!("Failed to create log directory: {}", cli.log_dir))?;

    // Get target address string
    let target_str = cli
        .target
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Target address is required"))?;

    // Initialize structured tracing with JSON output
    init_tracing(&cli.log_dir, target_str, cli.no_console)
        .context("Failed to initialize tracing")?;

    let listen_addr: SocketAddr = cli
        .listen_addr
        .parse()
        .with_context(|| format!("Invalid listen address: {}", cli.listen_addr))?;

    let target_addr: SocketAddr = target_str
        .parse()
        .with_context(|| format!("Invalid target address: {}", target_str))?;

    info!("Starting Modbus TCP proxy on {}", listen_addr);
    info!("Forwarding to target {}", target_addr);
    info!("Unit‑ID passthrough: enabled (from client)");

    // Bind server listener
    let listener = TcpListener::bind(listen_addr).await?;
    let server = Server::new(listener);

    let strict = cli.strict;
    let backend_timeout = Duration::from_secs(cli.backend_timeout);

    // Create metrics tracker (wrap in Arc for sharing across closures)
    let metrics = Arc::new(Metrics::new());

    // Clone metrics for the background task before moving into the closure
    let metrics_for_logger = metrics.clone();

    let on_connected = move |stream, peer_addr| {
        // Clone metrics for this closure
        let metrics_clone = metrics.clone();

        async move {
            info!("Client connected from {}", peer_addr);

            // Update metrics
            metrics_clone.increment_client_connection();
            let active_connections = metrics_clone.get_active_connections();
            info!(target: "metrics::connections", "Active client connections: {}", active_connections);

            // Create a connection guard that logs when the client disconnects
            let guard =
                ConnectionGuard::with_metrics(peer_addr, target_addr, metrics_clone.clone());

            accept_tcp_connection(stream, peer_addr, move |_peer| {
                let service = ProxyService::new(
                    target_addr,
                    strict,
                    backend_timeout,
                    guard.clone(),
                    metrics_clone.clone(),
                );
                Ok::<_, std::io::Error>(Some(service))
            })
        }
    };

    let on_process_error = |err| {
        // Transport-level server errors
        // Note: tokio-modbus logs errors with peer addresses internally
        error!("Server error: {}", err);
    };

    // Spawn a background task to periodically output metrics JSON snapshots
    let metrics_logger = tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(30));
        loop {
            interval.tick().await;
            let snapshot = metrics_for_logger.to_json_snapshot().await;
            match serde_json::to_string_pretty(&snapshot) {
                Ok(json) => info!(target: "metrics::snapshot", "{}", json),
                Err(e) => error!("Failed to serialize metrics snapshot: {}", e),
            }
        }
    });

    // Run the server
    let serve_result = server.serve(&on_connected, on_process_error).await;

    // Clean up the metrics logger task
    metrics_logger.abort();

    serve_result?;
    Ok(())
}

// Helper function to create ProxyService for tests (with dummy guard)
#[cfg(test)]
fn create_test_service(target: SocketAddr, strict: bool) -> ProxyService {
    let dummy_metrics = Arc::new(Metrics::new());
    let dummy_guard = ConnectionGuard::with_metrics(target, target, dummy_metrics.clone());
    ProxyService::new(
        target,
        strict,
        Duration::from_secs(30),
        dummy_guard,
        dummy_metrics,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};
    use tokio_modbus::{ExceptionCode, ProtocolError, Request, Response};

    // Helper function to create a test SocketAddr
    fn test_target_addr() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 100)), 502)
    }

    // Helper function to create a test listen address
    fn test_listen_addr() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 5020)
    }

    #[test]
    fn test_cli_default_values() {
        let cli = Cli::parse_from(&["modbus-proxy", "--target", "192.168.1.100:502"]);

        assert_eq!(cli.target.unwrap(), "192.168.1.100:502");
        assert_eq!(cli.listen_addr, "0.0.0.0:5020");
        assert_eq!(cli.strict, true);
        assert_eq!(cli.log_dir, "./logs");
    }

    #[test]
    fn test_cli_custom_values() {
        let cli = Cli::parse_from(&[
            "modbus-proxy",
            "--target",
            "10.0.0.1:502",
            "--listen",
            "127.0.0.1:8080", // Note: --strict is omitted to test default behavior (true)
        ]);

        assert_eq!(cli.target.unwrap(), "10.0.0.1:502");
        assert_eq!(cli.listen_addr, "127.0.0.1:8080");
        assert_eq!(cli.strict, true); // Should be true by default
        assert_eq!(cli.log_dir, "./logs");
    }

    #[test]
    fn test_cli_strict_false() {
        // To test strict=false, we need to use a different approach
        // Since --strict is a flag that defaults to true, we can't easily set it to false
        // This tests the workaround of using an environment variable or config file approach
        let cli = Cli {
            target: Some("10.0.0.1:502".to_string()),
            listen_addr: "127.0.0.1:8080".to_string(),
            strict: false,
            log_dir: "./logs".to_string(),
            backend_timeout: 30,
            metrics_json: false,
            no_console: false,
        };

        assert_eq!(cli.target.unwrap(), "10.0.0.1:502");
        assert_eq!(cli.listen_addr, "127.0.0.1:8080");
        assert_eq!(cli.strict, false);
        assert_eq!(cli.log_dir, "./logs");
        assert_eq!(cli.backend_timeout, 30);
    }

    #[test]
    fn test_cli_target_required() {
        let result = Cli::try_parse_from(&["modbus-proxy"]);
        assert!(result.is_err(), "Target should be required");
    }

    #[test]
    fn test_cli_invalid_listen_address() {
        let cli = Cli::parse_from(&[
            "modbus-proxy",
            "--target",
            "192.168.1.100:502",
            "--listen",
            "invalid-address",
        ]);

        let result: Result<SocketAddr, _> = cli.listen_addr.parse();
        assert!(
            result.is_err(),
            "Invalid listen address should fail parsing"
        );
    }

    #[test]
    fn test_cli_invalid_target_address() {
        let cli = Cli::parse_from(&["modbus-proxy", "--target", "invalid-address"]);

        let result: Result<SocketAddr, _> = cli.target.unwrap().parse();
        assert!(
            result.is_err(),
            "Invalid target address should fail parsing"
        );
    }

    #[test]
    fn test_proxy_service_creation() {
        let target = test_target_addr();
        let service = create_test_service(target, true);

        assert_eq!(service.target, target);
        assert_eq!(service.strict, true);
    }

    #[test]
    fn test_proxy_service_creation_non_strict() {
        let target = test_target_addr();
        let service = create_test_service(target, false);

        assert_eq!(service.target, target);
        assert_eq!(service.strict, false);
    }

    #[test]
    fn test_proxy_service_clone() {
        let target = test_target_addr();
        let service1 = create_test_service(target, true);
        let service2 = service1.clone();

        assert_eq!(service1.target, service2.target);
        assert_eq!(service1.strict, service2.strict);
        // The client Arc should point to the same Mutex
        assert!(Arc::ptr_eq(&service1.client, &service2.client));
    }

    #[test]
    fn test_proxy_service_debug() {
        let target = test_target_addr();
        let service = create_test_service(target, true);

        let debug_str = format!("{:?}", service);
        assert!(debug_str.contains("ProxyService"));
        assert!(debug_str.contains("target"));
        assert!(debug_str.contains("strict"));
        assert!(debug_str.contains("<Mutex<Option<ClientContext>>>"));
    }

    #[test]
    fn test_error_mapping_strict_mode() {
        let target = test_target_addr();
        let service = create_test_service(target, true);

        let mock_error = tokio_modbus::Error::Protocol(ProtocolError::HeaderMismatch {
            message: "test".to_string(),
            result: Ok(tokio_modbus::Response::ReadCoils(vec![])),
        });
        let mapped = service.map_client_error(mock_error);

        assert_eq!(mapped, ExceptionCode::GatewayPathUnavailable);
    }

    #[test]
    fn test_error_mapping_non_strict_mode() {
        let target = test_target_addr();
        let service = create_test_service(target, false);

        let mock_error = tokio_modbus::Error::Transport(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "test",
        ));
        let mapped = service.map_client_error(mock_error);

        assert_eq!(mapped, ExceptionCode::ServerDeviceFailure);
    }

    #[test]
    fn test_supported_function_codes() {
        // Test that all supported function codes are properly handled
        let supported_functions = vec![
            ("ReadCoils", Request::ReadCoils(0, 1)),
            ("ReadDiscreteInputs", Request::ReadDiscreteInputs(0, 1)),
            ("ReadInputRegisters", Request::ReadInputRegisters(0, 1)),
            ("ReadHoldingRegisters", Request::ReadHoldingRegisters(0, 1)),
            ("WriteSingleCoil", Request::WriteSingleCoil(0, true)),
            ("WriteSingleRegister", Request::WriteSingleRegister(0, 42)),
            (
                "WriteMultipleCoils",
                Request::WriteMultipleCoils(0, std::borrow::Cow::Owned(vec![true, false])),
            ),
            (
                "WriteMultipleRegisters",
                Request::WriteMultipleRegisters(0, std::borrow::Cow::Owned(vec![1, 2, 3])),
            ),
        ];

        for (name, request) in supported_functions {
            // Just verify the request types are constructed correctly
            match request {
                Request::ReadCoils(addr, cnt) => {
                    assert_eq!(addr, 0);
                    assert_eq!(cnt, 1);
                }
                Request::ReadDiscreteInputs(addr, cnt) => {
                    assert_eq!(addr, 0);
                    assert_eq!(cnt, 1);
                }
                Request::ReadInputRegisters(addr, cnt) => {
                    assert_eq!(addr, 0);
                    assert_eq!(cnt, 1);
                }
                Request::ReadHoldingRegisters(addr, cnt) => {
                    assert_eq!(addr, 0);
                    assert_eq!(cnt, 1);
                }
                Request::WriteSingleCoil(addr, val) => {
                    assert_eq!(addr, 0);
                    assert_eq!(val, true);
                }
                Request::WriteSingleRegister(addr, val) => {
                    assert_eq!(addr, 0);
                    assert_eq!(val, 42);
                }
                Request::WriteMultipleCoils(addr, values) => {
                    assert_eq!(addr, 0);
                    assert_eq!(values.len(), 2);
                }
                Request::WriteMultipleRegisters(addr, values) => {
                    assert_eq!(addr, 0);
                    assert_eq!(values.len(), 3);
                }
                _ => panic!("Unexpected request type for {}", name),
            }
        }
    }

    #[test]
    fn test_unsupported_function_code() {
        // Test that unsupported function codes would be rejected
        // This tests the pattern matching logic in the forward method
        let unsupported = vec![
            Request::Custom(0x08, std::borrow::Cow::Borrowed(&[])), // Diagnostic
            Request::Custom(0x11, std::borrow::Cow::Borrowed(&[])), // Report Server ID
        ];

        for req in unsupported {
            match req {
                Request::Custom(code, _) => {
                    // These should be caught as "IllegalFunction" in the forward method
                    assert!(
                        code != 0x01
                            && code != 0x02
                            && code != 0x03
                            && code != 0x04
                            && code != 0x05
                            && code != 0x06
                            && code != 0x0F
                            && code != 0x10
                    );
                }
                _ => panic!("Expected custom request"),
            }
        }
    }

    #[test]
    fn test_socket_addr_parsing() {
        let valid_addresses = vec![
            ("192.168.1.100:502", true),
            ("127.0.0.1:8080", true),
            ("0.0.0.0:5020", true),
            ("[::1]:502", true), // IPv6
            ("invalid", false),
            ("192.168.1.100", false),       // Missing port
            ("192.168.1.100:99999", false), // Invalid port
        ];

        for (addr, should_succeed) in valid_addresses {
            let result: Result<SocketAddr, _> = addr.parse();
            assert_eq!(
                result.is_ok(),
                should_succeed,
                "Address '{}' parsing should {} but didn't",
                addr,
                if should_succeed { "succeed" } else { "fail" }
            );
        }
    }

    #[test]
    fn test_slave_creation() {
        let slave = Slave(1);
        assert_eq!(slave.0, 1);

        let slave_255 = Slave(255);
        assert_eq!(slave_255.0, 255);

        let slave_0 = Slave(0);
        assert_eq!(slave_0.0, 0);
    }

    #[test]
    fn test_exception_code_variants() {
        // Test that we can create all relevant exception codes
        let exceptions = vec![
            ExceptionCode::IllegalFunction,
            ExceptionCode::ServerDeviceFailure,
            ExceptionCode::GatewayPathUnavailable,
        ];

        for exc in exceptions {
            match exc {
                ExceptionCode::IllegalFunction => {
                    // Used for unsupported function codes
                }
                ExceptionCode::ServerDeviceFailure => {
                    // Used in non-strict mode for errors
                }
                ExceptionCode::GatewayPathUnavailable => {
                    // Used in strict mode for errors
                }
                _ => {}
            }
        }
    }

    #[test]
    fn test_response_types() {
        // Test that we can construct all response types
        let responses = vec![
            Response::ReadCoils(vec![true, false, true]),
            Response::ReadDiscreteInputs(vec![false, true]),
            Response::ReadInputRegisters(vec![1, 2, 3, 4]),
            Response::ReadHoldingRegisters(vec![10, 20, 30]),
            Response::WriteSingleCoil(0, true),
            Response::WriteSingleRegister(0, 42),
            Response::WriteMultipleCoils(0, 5),
            Response::WriteMultipleRegisters(0, 3),
        ];

        assert_eq!(responses.len(), 8);
    }

    #[tokio::test]
    async fn test_multiple_connections_share_client() {
        let target = test_target_addr();
        let service1 = create_test_service(target, true);
        let service2 = service1.clone();

        // Both services should share the same client Arc
        assert!(Arc::ptr_eq(&service1.client, &service2.client));

        // The client should start as None
        let guard = service1.client.lock().await;
        assert!(guard.is_none());
        // Drop the guard to release the lock
        drop(guard);

        // service2 should also see None
        let guard2 = service2.client.lock().await;
        assert!(guard2.is_none());
    }

    #[test]
    fn test_service_trait_implementation() {
        // Verify that ProxyService implements the Service trait
        use tokio_modbus::server::Service;

        fn assert_service_impl<T: Service>() {}

        // This will only compile if ProxyService implements Service
        assert_service_impl::<ProxyService>();
    }

    #[test]
    fn test_service_associated_types() {
        // Verify the associated types of the Service implementation
        use tokio_modbus::server::Service;

        let target = test_target_addr();
        let service = create_test_service(target, true);

        // Test that we can create a Future from the service
        let slave_request = SlaveRequest {
            slave: 1,
            request: Request::ReadCoils(0, 1),
        };

        let future = service.call(slave_request);

        // The future should be a BoxFuture
        // We can't easily test the future without a running server, but we can verify it compiles
        let _future_type: BoxFuture<'static, Result<Response, ExceptionCode>> = future;
    }

    #[test]
    fn test_cow_conversions() {
        // Test the Cow conversions used in write operations
        use std::borrow::Cow;

        // Test Cow::Owned for coils
        let coils_owned: Cow<'_, [bool]> = Cow::Owned(vec![true, false, true]);
        let coils_ref: &[bool] = coils_owned.as_ref();
        assert_eq!(coils_ref, &[true, false, true]);

        // Test Cow::Owned for registers
        let registers_owned: Cow<'_, [u16]> = Cow::Owned(vec![1u16, 2, 3, 4]);
        let registers_ref: &[u16] = registers_owned.as_ref();
        assert_eq!(registers_ref, &[1, 2, 3, 4]);

        // Test Cow::Borrowed for coils
        let coils_borrowed: Cow<'_, [bool]> = Cow::Borrowed(&[true, false][..]);
        let coils_ref: &[bool] = coils_borrowed.as_ref();
        assert_eq!(coils_ref, &[true, false]);

        // Test Cow::Borrowed for registers
        let registers_borrowed: Cow<'_, [u16]> = Cow::Borrowed(&[10u16, 20][..]);
        let registers_ref: &[u16] = registers_borrowed.as_ref();
        assert_eq!(registers_ref, &[10, 20]);
    }

    #[test]
    fn test_version_info() {
        // Test that version info is available
        let _cli = Cli::parse_from(&["modbus-proxy", "--target", "192.168.1.100:502"]);
        let version = env!("CARGO_PKG_VERSION");
        assert!(!version.is_empty());
    }

    // Integration test pattern example (would require a mock Modbus server)
    // This shows how integration tests could be structured
    #[tokio::test]
    #[ignore] // Ignored because it requires a running Modbus server
    async fn test_integration_proxy_service() {
        // This is an example of how an integration test would work
        // It requires a running Modbus server to test against

        let target_addr: SocketAddr = "127.0.0.1:5502".parse().unwrap();
        let service = create_test_service(target_addr, true);

        // Test connection establishment
        let result = service.ensure_connected().await;

        // This would succeed if there's a Modbus server running
        // For now, we expect it to fail with GatewayPathUnavailable
        match result {
            Ok(()) => {
                // Connection successful - can test forwarding
                let slave_request = SlaveRequest {
                    slave: 1,
                    request: Request::ReadHoldingRegisters(0, 1),
                };

                let response_future = service.call(slave_request);
                let response = response_future.await;

                match response {
                    Ok(Response::ReadHoldingRegisters(data)) => {
                        assert_eq!(data.len(), 1);
                    }
                    Ok(_) => panic!("Unexpected response type"),
                    Err(e) => {
                        // Server might return an exception, which is valid Modbus behavior
                        info!("Modbus exception (valid behavior): {:?}", e);
                    }
                }
            }
            Err(ExceptionCode::GatewayPathUnavailable) => {
                // Expected when no server is running
                info!("No Modbus server available for integration test");
            }
            Err(e) => panic!("Unexpected error: {:?}", e),
        }
    }
}

#[test]
fn test_cli_log_dir_default() {
    let cli = Cli::parse_from(&["modbus-proxy", "--target", "192.168.1.100:502"]);
    assert_eq!(cli.log_dir, "./logs");
}

#[test]
fn test_cli_log_dir_custom() {
    let cli = Cli::parse_from(&[
        "modbus-proxy",
        "--target",
        "192.168.1.100:502",
        "--log-dir",
        "/tmp/custom-logs",
    ]);
    assert_eq!(cli.log_dir, "/tmp/custom-logs");
}

#[test]
fn test_process_specific_logging() {
    // Test that process-specific log files would work
    let process_id = std::process::id();
    let log_file_name = format!("modbus-proxy.{}", process_id);

    assert!(log_file_name.starts_with("modbus-proxy."));
    assert!(log_file_name.len() > "modbus-proxy.".len());

    // In a real implementation, each process would have its own log file
    let temp_dir = std::env::temp_dir().join("modbus-proxy-multi-test");
    std::fs::create_dir_all(&temp_dir).ok();

    let process_log_file = temp_dir.join(format!("modbus-proxy.{process_id}.log"));

    // Verify we can create a file for this process
    std::fs::write(&process_log_file, "test log entry").expect("Failed to write test log");
    assert!(process_log_file.exists());

    // Cleanup
    std::fs::remove_file(&process_log_file).ok();
    std::fs::remove_dir(&temp_dir).ok();
}

#[test]
fn test_target_based_log_filename() {
    // Test that target addresses are properly sanitized for log filenames
    let test_cases = vec![
        ("192.168.1.100:502", "192.168.1.100-502"),
        ("10.0.0.50:502", "10.0.0.50-502"),
        ("127.0.0.1:8080", "127.0.0.1-8080"),
        ("[::1]:502", "[--1]-502"), // IPv6 with :: gets converted to --
    ];

    for (target, expected) in test_cases {
        let sanitized = target.replace(':', "-");
        assert_eq!(
            sanitized, expected,
            "Target '{}' should sanitize to '{}'",
            target, expected
        );
    }
}

#[test]
fn test_unique_log_files_per_target() {
    // Verify that different targets produce different log filenames
    let targets = vec![
        "192.168.1.100:502",
        "192.168.1.101:502",
        "10.0.0.50:502",
        "192.168.1.100:503", // Same IP, different port
    ];

    let mut log_files = std::collections::HashSet::new();

    for target in &targets {
        let sanitized = target.replace(':', "-");
        let log_file = format!("modbus-proxy.{}", sanitized);

        // Each target should produce a unique log file name
        assert!(
            log_files.insert(log_file.clone()),
            "Log file '{}' should be unique but was duplicated",
            log_file
        );
    }

    // We should have exactly as many unique log files as targets
    assert_eq!(log_files.len(), targets.len());
}

#[test]
fn test_connection_guard_creation() {
    let peer_addr: SocketAddr = "192.168.1.100:502".parse().unwrap();
    let target_addr: SocketAddr = "192.168.1.101:502".parse().unwrap();

    let guard = ConnectionGuard::new(peer_addr, target_addr);

    // Verify the guard can be created successfully
    // (We can't access the inner fields directly anymore, but we can verify it clones correctly)
    let _guard2 = guard.clone();
}

#[test]
fn test_connection_guard_clone() {
    let peer_addr: SocketAddr = "192.168.1.100:502".parse().unwrap();
    let target_addr: SocketAddr = "192.168.1.101:502".parse().unwrap();

    let guard1 = ConnectionGuard::new(peer_addr, target_addr);
    let guard2 = guard1.clone();

    // Cloned guard should work (we can't access fields directly but can verify it's cloneable)
    let _guard3 = guard2.clone();
}
