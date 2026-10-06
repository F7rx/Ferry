//! `ferry-signal` binary: configuration from the environment (plus
//! `--addr`), logging, graceful shutdown and a `healthcheck` subcommand for
//! container probes.

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::process::ExitCode;
use std::time::Duration;

use ferry_signal::Config;
use tracing_subscriber::EnvFilter;

/// Loopback only: exposing the server is an explicit choice (usually behind
/// a TLS-terminating reverse proxy, or `0.0.0.0:3000` in a container).
const DEFAULT_ADDR: &str = "127.0.0.1:3000";

const USAGE: &str = "\
ferry-signal: WebRTC signaling server for Ferry (LocalSend /v1/ws compatible).
Relays signaling messages between peers; never sees or stores files.

USAGE:
    ferry-signal [--addr <IP:PORT>]               run the server
    ferry-signal healthcheck [--addr <IP:PORT>]   exit 0 if the server answers GET /healthz

OPTIONS:
    -a, --addr <IP:PORT>   listen address (overrides FERRY_SIGNAL_ADDR)
    -h, --help             print this help
    -V, --version          print the version

ENVIRONMENT:
    FERRY_SIGNAL_ADDR              listen address (default 127.0.0.1:3000, loopback only;
                                   0.0.0.0:3000 or [::]:3000 accepts remote clients)
    FERRY_SIGNAL_ALLOWED_ORIGINS   comma-separated browser origins allowed to connect
                                   (default: any origin)
    FERRY_SIGNAL_TRUSTED_PROXIES   comma-separated IPs/CIDRs whose X-Forwarded-For is trusted
                                   (default: none)
    FERRY_SIGNAL_MAX_CONNS_PER_IP  concurrent connections per IPv4 address or IPv6 /64
                                   (default 16)
    FERRY_SIGNAL_MAX_CONNS         concurrent connections in total (default 10000)
    FERRY_TURN_SECRET              coturn static-auth-secret; enables GET /v1/turn
    FERRY_TURN_URLS                comma-separated turn:/turns: URLs (set with the secret)
    RUST_LOG                       log filter (default info)

Docs: crates/ferry-signal/README.md
";

/// What the command line asked for.
enum Command {
    Serve,
    Healthcheck,
}

/// Parses the arguments. `Err(code)` means: exit now (help, version or a
/// usage error, already printed).
fn parse_args(mut args: impl Iterator<Item = std::ffi::OsString>) -> Result<(Command, Option<String>), ExitCode> {
    let usage_error = |message: String| {
        eprintln!("ferry-signal: {message}\n\n{USAGE}");
        Err(ExitCode::from(2))
    };
    let mut command = Command::Serve;
    let mut addr = None;
    while let Some(arg) = args.next() {
        let Some(arg) = arg.to_str() else {
            return usage_error(format!("argument {arg:?} is not valid UTF-8"));
        };
        match arg {
            "healthcheck" => command = Command::Healthcheck,
            "-a" | "--addr" => match args.next().as_ref().and_then(|v| v.to_str()) {
                Some(value) => addr = Some(value.to_owned()),
                None => return usage_error(format!("{arg} needs a value")),
            },
            "-h" | "--help" | "help" => {
                print!("{USAGE}");
                return Err(ExitCode::SUCCESS);
            }
            "-V" | "--version" => {
                println!("ferry-signal {}", env!("CARGO_PKG_VERSION"));
                return Err(ExitCode::SUCCESS);
            }
            other => match other.strip_prefix("--addr=") {
                Some(value) => addr = Some(value.to_owned()),
                None => return usage_error(format!("unknown argument `{other}`")),
            },
        }
    }
    Ok((command, addr))
}

fn main() -> ExitCode {
    let (command, addr_flag) = match parse_args(std::env::args_os().skip(1)) {
        Ok(parsed) => parsed,
        Err(code) => return code,
    };
    let addr = addr_flag
        .or_else(|| std::env::var("FERRY_SIGNAL_ADDR").ok())
        .map(|a| a.trim().to_owned())
        .filter(|a| !a.is_empty())
        .unwrap_or_else(|| DEFAULT_ADDR.to_owned());
    if let Command::Healthcheck = command {
        return healthcheck(&addr);
    }

    tracing_subscriber::fmt().with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))).init();

    let config = match Config::from_env() {
        Ok(config) => config,
        Err(err) => {
            tracing::error!("invalid configuration: {err}");
            return ExitCode::FAILURE;
        }
    };
    match &config.allowed_origins {
        Some(origins) => tracing::info!(count = origins.len(), "origin allowlist enabled"),
        None => tracing::warn!("FERRY_SIGNAL_ALLOWED_ORIGINS is not set: any website may connect from a visitor's browser"),
    }
    tracing::info!(trusted_proxies = config.trusted_proxies.len(), turn = config.turn.is_some(), "configuration loaded");

    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(err) => {
            tracing::error!("cannot start the async runtime: {err}");
            return ExitCode::FAILURE;
        }
    };
    let result = runtime.block_on(async {
        let listener = tokio::net::TcpListener::bind(&addr).await?;
        let local = listener.local_addr()?;
        if local.ip().is_loopback() {
            tracing::info!("listening on {local} (loopback only; use --addr 0.0.0.0:{} to accept remote clients)", local.port());
        } else {
            tracing::info!("listening on {local}");
        }
        ferry_signal::serve_with_shutdown(listener, config, shutdown_signal()).await
    });
    match result {
        Ok(()) => {
            tracing::info!("shut down");
            ExitCode::SUCCESS
        }
        Err(err) => {
            tracing::error!("server error on {addr}: {err}");
            ExitCode::FAILURE
        }
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
    tracing::info!("shutdown signal received");
}

/// `GET /healthz` against the configured address (loopback if it is a
/// wildcard). Used as the container health check (the runtime image has no
/// shell or curl).
fn healthcheck(addr: &str) -> ExitCode {
    let Ok(mut target) = addr.parse::<SocketAddr>() else {
        eprintln!("healthcheck: listen address `{addr}` is not ip:port");
        return ExitCode::FAILURE;
    };
    if target.ip().is_unspecified() {
        target.set_ip(match target.ip() {
            IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::LOCALHOST),
        });
    }
    let check = || -> std::io::Result<bool> {
        let timeout = Duration::from_secs(3);
        let mut stream = TcpStream::connect_timeout(&target, timeout)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        stream.write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")?;
        let mut response = Vec::new();
        stream.take(4096).read_to_end(&mut response)?;
        Ok(response.starts_with(b"HTTP/1.1 200"))
    };
    match check() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => {
            eprintln!("healthcheck: unexpected response");
            ExitCode::FAILURE
        }
        Err(err) => {
            eprintln!("healthcheck: {err}");
            ExitCode::FAILURE
        }
    }
}
