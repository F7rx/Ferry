//! `ferry`: send and receive from the terminal. Talks to Ferry and LocalSend
//! devices alike. `--json` prints one engine event per line for scripting.

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use ferry_core::events::EngineEvent;
use ferry_core::model::{Decision, DeviceSummary, Direction, Protocol, TransferState, TransferSummary};
use ferry_core::util::format_bytes;
use ferry_core::{Engine, EngineConfig, SendItem, Settings, Target};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[derive(Parser)]
#[command(name = "ferry", version, about = "Fast, private device-to-device sharing (LocalSend compatible)")]
struct Cli {
    /// Name shown to other devices.
    #[arg(long, global = true, env = "FERRY_ALIAS")]
    alias: Option<String>,
    /// Port to receive on (default 53317).
    #[arg(long, global = true, env = "FERRY_PORT")]
    port: Option<u16>,
    /// Where identity, settings and history live.
    #[arg(long, global = true, env = "FERRY_DATA_DIR")]
    data_dir: Option<PathBuf>,
    /// Throwaway identity and settings (nothing written to disk except received files).
    #[arg(long, global = true)]
    ephemeral: bool,
    /// Print engine events as JSON lines.
    #[arg(long, global = true)]
    json: bool,
    /// Signaling server for WebRTC transfers with browsers and devices on
    /// other networks (e.g. wss://signal.example/v1/ws).
    #[arg(long = "signal", global = true, env = "FERRY_SIGNAL_URL", value_name = "WS URL")]
    signal: Option<String>,
    /// Join a private link (…#room=<secret>) through the signaling server.
    #[arg(long = "room", global = true, value_name = "LINK")]
    rooms: Vec<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Receive files until interrupted.
    Receive {
        /// Save folder (default: Downloads/Ferry).
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Which requests to accept without asking.
        #[arg(long, value_enum, default_value_t = AcceptMode::Ask)]
        accept: AcceptMode,
        /// Require this PIN from senders.
        #[arg(long)]
        pin: Option<String>,
        /// Exit after this many completed transfers.
        #[arg(long)]
        count: Option<u32>,
        /// Also accept uploads from web browsers (prints a link and QR code).
        #[arg(long)]
        browser: bool,
    },
    /// Share files with any browser on this network via a link.
    Share {
        /// Files or folders.
        paths: Vec<PathBuf>,
        /// Require this PIN in the browser.
        #[arg(long)]
        pin: Option<String>,
    },
    /// Send files, folders or a text message.
    Send {
        /// Files or folders.
        paths: Vec<PathBuf>,
        /// Device name, id prefix, or address (ip, ip:port).
        #[arg(long)]
        to: Vec<String>,
        /// Send this text as a message.
        #[arg(long)]
        text: Option<String>,
        /// PIN, if the receiver asks for one.
        #[arg(long)]
        pin: Option<String>,
        /// Seconds to look for named devices.
        #[arg(long, default_value_t = 5)]
        wait: u64,
    },
    /// List nearby devices.
    Devices {
        #[arg(long, default_value_t = 3)]
        wait: u64,
    },
    /// Show this device's identity and addresses.
    Info,
    /// Create a private link (needs --signal) and receive through it until interrupted.
    Link {
        /// Save folder (default: Downloads/Ferry).
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Which requests to accept without asking.
        #[arg(long, value_enum, default_value_t = AcceptMode::Ask)]
        accept: AcceptMode,
        /// Exit after this many completed transfers.
        #[arg(long)]
        count: Option<u32>,
    },
    /// Make another device one of your devices (trusted both ways, no prompts).
    Pair {
        #[command(subcommand)]
        how: PairCommand,
    },
}

#[derive(Subcommand)]
enum PairCommand {
    /// Show a single-use pairing code (QR and link) for your other device.
    Show,
    /// Use the pairing link another device shows (ferry://pair?…).
    Link { uri: String },
    /// Pair with a nearby device by comparing a 6-digit code on both screens.
    With {
        /// Device name, id prefix, or address (ip, ip:port).
        device: String,
        /// Seconds to look for a named device.
        #[arg(long, default_value_t = 5)]
        wait: u64,
    },
}

#[derive(Clone, Copy, ValueEnum, PartialEq, Eq)]
enum AcceptMode {
    /// Ask on the terminal for every request.
    Ask,
    /// Accept everything (use only on networks you trust).
    All,
    /// Accept requests from trusted devices, decline others.
    Trusted,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_env("FERRY_LOG").unwrap_or_else(|_| {
            // The WebRTC stack logs routine ICE noise (unusable interfaces) as warnings.
            "warn,webrtc=error,webrtc_ice=error,webrtc_sctp=error,webrtc_dtls=error,webrtc_data=error,webrtc_mdns=error,webrtc_srtp=error,dtls=error,sctp=error,ice=error,interceptor=error".into()
        }))
        .with_writer(std::io::stderr)
        .init();
    if let Err(err) = run(cli).await {
        eprintln!("ferry: {err:#}");
        std::process::exit(1);
    }
}

async fn start(cli: &Cli, tweak: impl FnOnce(&mut Settings)) -> Result<Arc<Engine>> {
    let config = if cli.ephemeral {
        let mut s = Settings::default();
        apply_common(cli, &mut s);
        tweak(&mut s);
        EngineConfig { data_dir: None, settings_override: Some(s), discovery: true }
    } else {
        let dir = match &cli.data_dir {
            Some(d) => d.clone(),
            None => directories::ProjectDirs::from("app", "Ferry", "Ferry").context("no home directory")?.data_dir().join("cli"),
        };
        let mut config = EngineConfig::persistent(dir.clone());
        // Merge flags over the stored settings.
        let mut s = ferry_core::settings::SettingsStore::load(dir.join("settings.json"))?.get();
        apply_common(cli, &mut s);
        tweak(&mut s);
        config.settings_override = Some(s);
        config
    };
    let engine = Engine::start(config).await.map_err(|e| anyhow::anyhow!(e.info().message))?;
    for link in &cli.rooms {
        let room = engine.join_room(link).map_err(|e| anyhow::anyhow!(e.info().message))?;
        if !cli.json {
            eprintln!("Joined private link {}", room.id);
        }
    }
    Ok(engine)
}

fn apply_common(cli: &Cli, s: &mut Settings) {
    if let Some(url) = &cli.signal {
        s.signaling_url = Some(url.clone()).filter(|u| !u.trim().is_empty());
    }
    if let Some(alias) = &cli.alias {
        s.alias = alias.clone();
    }
    if let Some(port) = cli.port {
        s.port = port;
    }
}

async fn run(cli: Cli) -> Result<()> {
    match &cli.command {
        Command::Info => {
            let engine = start(&cli, |_| {}).await?;
            let me = engine.local_device();
            if cli.json {
                println!("{}", serde_json::to_string(&me)?);
            } else {
                println!("{} ({:?})", me.alias, me.device_kind);
                println!("  id        {}", me.short_id);
                println!("  receiving {}:{} ({})", me.addresses.first().map(String::as_str).unwrap_or("?"), me.port, me.protocol.as_str());
                for a in me.addresses.iter().skip(1) {
                    println!("            {a}:{}", me.port);
                }
            }
            engine.shutdown().await;
        }
        Command::Devices { wait } => {
            let engine = start(&cli, |s| s.receive_enabled = false).await?;
            tokio::time::sleep(Duration::from_secs(*wait)).await;
            let devices = engine.devices();
            if cli.json {
                println!("{}", serde_json::to_string(&devices)?);
            } else if devices.is_empty() {
                println!("No devices found. Make sure they're on the same network and Ferry or LocalSend is open.");
                if let Some(err) = engine.multicast_error() {
                    println!("(multicast unavailable: {err})");
                }
            } else {
                for d in devices.iter().filter(|d| d.online) {
                    print_device(d);
                }
            }
            engine.shutdown().await;
        }
        Command::Share { paths, pin } => {
            if paths.is_empty() {
                bail!("nothing to share: give files or folders");
            }
            let engine = start(&cli, |s| s.receive_enabled = false).await?;
            let items = paths.iter().map(|p| SendItem::Path { path: p.clone() }).collect();
            let link = engine.share_with_browsers(items, pin.clone()).await.map_err(|e| anyhow::anyhow!(e.info().message))?;
            print_link(&cli, &link, "Open this link on any device on this network to download")?;
            let mut events = engine.subscribe();
            loop {
                tokio::select! {
                    e = events.recv() => match e {
                        Ok(EngineEvent::BrowserShareUpdated { share }) if !cli.json => {
                            eprintln!("{} download(s) · {} active{}", share.downloads, share.active,
                                share.recent_clients.last().map(|c| format!(" · last: {c}")).unwrap_or_default());
                        }
                        Ok(e) if cli.json => println!("{}", serde_json::to_string(&e)?),
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        _ => {}
                    },
                    _ = tokio::signal::ctrl_c() => break,
                }
            }
            engine.shutdown().await;
        }
        Command::Receive { dir, accept, pin, count, browser } => {
            let dir = dir.clone();
            let pin = pin.clone();
            let engine = start(&cli, |s| {
                if let Some(d) = dir {
                    s.save_dir = Some(d);
                }
                if pin.is_some() {
                    s.pin = pin;
                }
            })
            .await?;
            if *browser {
                let link = engine.receive_from_browsers(None).await.map_err(|e| anyhow::anyhow!(e.info().message))?;
                print_link(&cli, &link, "Browsers can send to this device with this link")?;
            }
            receive(&cli, engine, *accept, *count).await?;
        }
        Command::Link { dir, accept, count } => {
            let dir = dir.clone();
            let engine = start(&cli, |s| {
                if let Some(d) = dir {
                    s.save_dir = Some(d);
                }
            })
            .await?;
            if engine.signaling_status().url.is_none() {
                bail!("private links need a signaling server: pass --signal <ws url> (or set FERRY_SIGNAL_URL)");
            }
            let room = engine.create_room();
            if cli.json {
                println!("{}", serde_json::to_string(&serde_json::json!({ "type": "room", "room": room }))?);
            } else {
                println!(
                    "Private link. Whoever opens it can see this device, on any network:

  {}
",
                    room.link
                );
                if let Ok(code) = qrcode::QrCode::new(room.link.as_bytes()) {
                    println!("{}", code.render::<qrcode::render::unicode::Dense1x2>().quiet_zone(true).build());
                }
                println!("  The secret after # never reaches the server. Press Ctrl+C to stop.");
            }
            receive(&cli, engine, *accept, *count).await?;
        }
        Command::Pair { how } => {
            if cli.ephemeral && !cli.json {
                eprintln!("Note: --ephemeral forgets this identity on exit, so the pairing won't last.");
            }
            let engine = start(&cli, |_| {}).await?;
            let result = pair(&cli, &engine, how).await;
            engine.shutdown().await;
            result?;
        }
        Command::Send { paths, to, text, pin, wait } => {
            if to.is_empty() {
                bail!("say where to send with --to <device name | ip[:port]>");
            }
            let mut items: Vec<SendItem> = paths.iter().map(|p| SendItem::Path { path: p.clone() }).collect();
            if let Some(t) = text {
                items.push(SendItem::Text { text: t.clone() });
            }
            if items.is_empty() {
                bail!("nothing to send: give paths or --text");
            }
            let engine = start(&cli, |s| s.receive_enabled = false).await?;
            let targets = resolve_targets(&engine, to, Duration::from_secs(*wait)).await?;
            send(&cli, engine, targets, items, pin.clone()).await?;
        }
    }
    Ok(())
}

fn print_link(cli: &Cli, link: &ferry_core::browser::BrowserShareInfo, title: &str) -> Result<()> {
    if cli.json {
        println!("{}", serde_json::to_string(&serde_json::json!({ "type": "browserLink", "link": link }))?);
        return Ok(());
    }
    let Some(url) = link.urls.first() else { bail!("no network address to share a link on") };
    println!(
        "{title}:

  {url}
"
    );
    if let Ok(code) = qrcode::QrCode::new(url.as_bytes()) {
        let art = code.render::<qrcode::render::unicode::Dense1x2>().quiet_zone(true).build();
        println!("{art}");
    }
    for other in link.urls.iter().skip(1) {
        println!("  also: {other}");
    }
    println!("  Not encrypted on this network. Press Ctrl+C to stop sharing.");
    Ok(())
}

async fn pair(cli: &Cli, engine: &Arc<Engine>, how: &PairCommand) -> Result<()> {
    let user_err = |e: ferry_core::FerryError| {
        let info = e.info();
        anyhow::anyhow!("{}{}", info.message, info.hint.map(|h| format!(" ({h})")).unwrap_or_default())
    };
    let mut events = engine.subscribe();
    match how {
        PairCommand::Show => {
            let offer = engine.create_pairing_offer().map_err(user_err)?;
            if cli.json {
                println!("{}", serde_json::to_string(&serde_json::json!({ "type": "pairingOffer", "offer": offer }))?);
            } else {
                println!("Scan this with Ferry on your other device, or paste the link into it:\n\n  {}\n", offer.uri);
                if let Ok(code) = qrcode::QrCode::new(offer.uri.as_bytes()) {
                    println!("{}", code.render::<qrcode::render::unicode::Dense1x2>().quiet_zone(true).build());
                }
                println!("  Works once, for 5 minutes. Press Ctrl+C to cancel.");
            }
            loop {
                tokio::select! {
                    e = events.recv() => match e {
                        Ok(EngineEvent::PairingOfferClosed { id, device }) if id == offer.id => {
                            if cli.json {
                                println!("{}", serde_json::to_string(&serde_json::json!({ "type": "pairingOfferClosed", "id": id, "device": device }))?);
                            }
                            match device {
                                Some(d) => {
                                    if !cli.json {
                                        println!("✓ Paired with {}. Files between you now arrive without asking.", d.custom_alias.unwrap_or(d.alias));
                                    }
                                    return Ok(());
                                }
                                None => bail!("the pairing code expired"),
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => bail!("engine stopped"),
                        _ => {}
                    },
                    _ = tokio::signal::ctrl_c() => {
                        engine.cancel_pairing_offer(&offer.id);
                        bail!("cancelled");
                    }
                }
            }
        }
        PairCommand::Link { uri } => {
            let d = engine.pair_with_uri(uri).await.map_err(user_err)?;
            if cli.json {
                println!("{}", serde_json::to_string(&serde_json::json!({ "type": "paired", "device": d }))?);
            } else {
                println!("✓ Paired with {}. Files between you now arrive without asking.", d.custom_alias.unwrap_or(d.alias));
            }
            Ok(())
        }
        PairCommand::With { device, wait } => {
            let id = match resolve_targets(engine, std::slice::from_ref(device), Duration::from_secs(*wait)).await?.pop() {
                Some(Target::Device { id }) => id,
                Some(Target::Address { host, port, .. }) => engine.add_device(&host, port).await.map_err(user_err)?.id,
                None => bail!("couldn't find {device}"),
            };
            let pairing = engine.start_code_pairing(&id).await.map_err(user_err)?;
            if cli.json {
                println!("{}", serde_json::to_string(&serde_json::json!({ "type": "outgoingPairing", "pairing": pairing }))?);
            } else {
                println!("Check that {} shows this code, then confirm there:\n\n      {}\n", pairing.peer.alias, pairing.code);
                println!("  Waiting… (Ctrl+C to cancel)");
            }
            loop {
                tokio::select! {
                    e = events.recv() => match e {
                        Ok(EngineEvent::PairingFinished { id, outcome, device, error }) if id == pairing.id => {
                            if cli.json {
                                println!("{}", serde_json::to_string(&serde_json::json!({ "type": "pairingFinished", "outcome": outcome, "device": device, "error": error }))?);
                            }
                            return match outcome {
                                ferry_core::pairing::PairingOutcome::Paired => {
                                    if !cli.json {
                                        println!("✓ Paired with {}. Files between you now arrive without asking.", pairing.peer.alias);
                                    }
                                    Ok(())
                                }
                                ferry_core::pairing::PairingOutcome::Declined => bail!("{} didn't confirm the code", pairing.peer.alias),
                                ferry_core::pairing::PairingOutcome::Cancelled => bail!("cancelled"),
                                ferry_core::pairing::PairingOutcome::Failed => {
                                    bail!("{}", error.map(|e| e.message).unwrap_or_else(|| "pairing failed".into()))
                                }
                            };
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => bail!("engine stopped"),
                        _ => {}
                    },
                    _ = tokio::signal::ctrl_c() => {
                        engine.cancel_code_pairing(&pairing.id);
                    }
                }
            }
        }
    }
}

fn print_device(d: &DeviceSummary) {
    let flags = [(d.mine, "mine"), (d.trusted && !d.mine, "trusted"), (d.is_ferry, "ferry"), (!d.verified, "NOT ENCRYPTED")]
        .iter()
        .filter(|(on, _)| *on)
        .map(|(_, f)| *f)
        .collect::<Vec<_>>()
        .join(", ");
    println!(
        "{:<24} {:<22} {:<10} {}  {}",
        d.custom_alias.as_deref().unwrap_or(&d.alias),
        d.address.as_deref().unwrap_or("-"),
        format!("{:?}", d.device_kind).to_lowercase(),
        &d.id[..d.id.len().min(8)],
        flags
    );
}

async fn resolve_targets(engine: &Engine, specs: &[String], wait: Duration) -> Result<Vec<Target>> {
    let mut targets = Vec::new();
    let mut names = Vec::new();
    for spec in specs {
        if let Some(addr) = parse_address(spec) {
            targets.push(addr);
        } else {
            names.push(spec.to_lowercase());
        }
    }
    if names.is_empty() {
        return Ok(targets);
    }
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let devices = engine.devices();
        let found: Vec<Option<&DeviceSummary>> = names
            .iter()
            .map(|n| {
                devices.iter().find(|d| {
                    d.online
                        && (d.alias.to_lowercase() == *n
                            || d.custom_alias.as_deref().is_some_and(|c| c.to_lowercase() == *n)
                            || (n.len() >= 6 && d.id.to_lowercase().starts_with(n.as_str())))
                })
            })
            .collect();
        if found.iter().all(Option::is_some) {
            targets.extend(found.into_iter().flatten().map(|d| Target::Device { id: d.id.clone() }));
            return Ok(targets);
        }
        if tokio::time::Instant::now() >= deadline {
            let missing: Vec<&String> = names.iter().zip(found).filter(|(_, f)| f.is_none()).map(|(n, _)| n).collect();
            bail!("couldn't find {:?} on the network (try `ferry devices`, or pass an IP address)", missing);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

fn parse_address(spec: &str) -> Option<Target> {
    let (host, port) = if let Ok(sa) = spec.parse::<std::net::SocketAddr>() {
        (sa.ip().to_string(), sa.port())
    } else if let Ok(ip) = spec.parse::<std::net::IpAddr>() {
        (ip.to_string(), 53317)
    } else {
        return None;
    };
    Some(Target::Address { host, port, protocol: Protocol::Https, fingerprint: None })
}

async fn send(cli: &Cli, engine: Arc<Engine>, targets: Vec<Target>, items: Vec<SendItem>, pin: Option<String>) -> Result<()> {
    let mut events = engine.subscribe();
    let ids = engine.send(targets, items).await.map_err(|e| anyhow::anyhow!(e.info().message))?;
    let mut remaining: std::collections::HashSet<String> = ids.iter().cloned().collect();
    let mut failed = false;
    let mut last_print = std::time::Instant::now();
    let mut pin_tried: std::collections::HashSet<String> = Default::default();
    while !remaining.is_empty() {
        let event = match events.recv().await {
            Ok(e) => e,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(_) => break,
        };
        if cli.json {
            println!("{}", serde_json::to_string(&event)?);
        }
        if let EngineEvent::TransferUpdated { transfer: t } = &event {
            if !remaining.contains(&t.id) {
                continue;
            }
            if t.state == TransferState::PinRequired && !pin_tried.contains(&t.id) {
                pin_tried.insert(t.id.clone());
                match &pin {
                    Some(p) => {
                        engine.submit_pin(&t.id, Some(p.clone()));
                    }
                    None => {
                        eprintln!("{} requires a PIN. Pass --pin", t.peer.alias);
                        engine.submit_pin(&t.id, None);
                    }
                }
            }
            if t.state.is_final() {
                remaining.remove(&t.id);
                if !cli.json {
                    print_final(t);
                }
                failed |= t.state != TransferState::Completed;
            } else if !cli.json && last_print.elapsed() > Duration::from_millis(500) {
                last_print = std::time::Instant::now();
                print_progress(t);
            }
        }
    }
    engine.shutdown().await;
    if failed {
        std::process::exit(1);
    }
    Ok(())
}

fn print_progress(t: &TransferSummary) {
    let pct = (t.bytes_done * 100).checked_div(t.total_bytes).unwrap_or(0);
    let eta = t.eta_secs.map(|s| format!(", {s}s left")).unwrap_or_default();
    eprintln!(
        "{} {}: {pct}% ({} of {}, {}/s{eta})",
        if t.direction == Direction::Send { "→" } else { "←" },
        t.peer.alias,
        format_bytes(t.bytes_done),
        format_bytes(t.total_bytes),
        format_bytes(t.speed_bps)
    );
}

fn print_final(t: &TransferSummary) {
    let what = if t.file_count == 1 { t.title.clone() } else { format!("{} files", t.file_count) };
    match t.state {
        TransferState::Completed => println!(
            "✓ {what} {} {} ({})",
            if t.direction == Direction::Send { "sent to" } else { "received from" },
            t.peer.alias,
            format_bytes(t.total_bytes)
        ),
        state => {
            println!("✗ {what} {}: {state:?}{}", t.peer.alias, t.error.as_ref().map(|e| format!(": {}", e.message)).unwrap_or_default())
        }
    }
}

async fn receive(cli: &Cli, engine: Arc<Engine>, accept: AcceptMode, count: Option<u32>) -> Result<()> {
    let me = engine.local_device();
    if !cli.json {
        println!("Receiving as “{}” on port {}: {}", me.alias, me.port, me.addresses.join(", "));
        println!("Files are saved to {}", engine.settings().save_dir().display());
        let signaling = engine.signaling_status();
        if let Some(url) = &signaling.url {
            println!("Also reachable over WebRTC through {url}");
        }
        if let Some(err) = engine.multicast_error() {
            println!("Note: discovery by broadcast is unavailable ({err}); senders can still use this device's address.");
        }
    } else {
        println!("{}", serde_json::to_string(&serde_json::json!({"type": "ready", "device": me}))?);
    }
    let mut events = engine.subscribe();
    let mut completed = 0u32;
    let stdin = Arc::new(tokio::sync::Mutex::new(tokio::io::BufReader::new(tokio::io::stdin())));
    loop {
        let event = tokio::select! {
            e = events.recv() => e,
            _ = tokio::signal::ctrl_c() => break,
        };
        let event = match event {
            Ok(e) => e,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(_) => break,
        };
        if cli.json {
            println!("{}", serde_json::to_string(&event)?);
        }
        match event {
            EngineEvent::IncomingRequest { request } => {
                if let Some(text) = &request.text {
                    if !cli.json {
                        println!("💬 {}: {text}", request.peer.alias);
                    }
                    continue;
                }
                let decision = match accept {
                    AcceptMode::All => Decision::accept_all(),
                    AcceptMode::Trusted if request.trusted => Decision::accept_all(),
                    AcceptMode::Trusted => Decision::decline(),
                    AcceptMode::Ask => {
                        println!(
                            "{}{} wants to send {} file(s), {}. Accept? [y/N]",
                            request.peer.alias,
                            if request.peer.verified { "" } else { " (not verified)" },
                            request.files.len(),
                            format_bytes(request.total_bytes)
                        );
                        for f in request.files.iter().take(10) {
                            println!("    {} ({})", f.name, format_bytes(f.size));
                        }
                        let line = {
                            use tokio::io::AsyncBufReadExt;
                            let mut line = String::new();
                            let _ = stdin.lock().await.read_line(&mut line).await;
                            line
                        };
                        if line.trim().eq_ignore_ascii_case("y") { Decision::accept_all() } else { Decision::decline() }
                    }
                };
                engine.respond(&request.id, decision);
            }
            EngineEvent::TransferUpdated { transfer: t } if t.direction == Direction::Receive => {
                if t.state.is_final() {
                    if !cli.json {
                        print_final(&t);
                    }
                    if t.state == TransferState::Completed {
                        completed += 1;
                        if count.is_some_and(|c| completed >= c) {
                            break;
                        }
                    }
                }
            }
            EngineEvent::PairingRequest { request } => {
                // Lasting trust: only ever granted by a person, never by --accept.
                let accept_pair = if accept == AcceptMode::Ask {
                    println!(
                        "{} wants to add this device to its devices.\n  Code: {}\n  Same code on {}? [y/N]",
                        request.peer.alias, request.code, request.peer.alias
                    );
                    let line = {
                        use tokio::io::AsyncBufReadExt;
                        let mut line = String::new();
                        let _ = stdin.lock().await.read_line(&mut line).await;
                        line
                    };
                    line.trim().eq_ignore_ascii_case("y")
                } else {
                    if !cli.json {
                        println!("{} asked to pair; declined (answer pairing requests with --accept ask).", request.peer.alias);
                    }
                    false
                };
                engine.respond_pairing(&request.id, accept_pair);
            }
            EngineEvent::Notice { message, .. } if !cli.json => eprintln!("! {message}"),
            EngineEvent::ServerStatus { running: false, error: Some(e), .. } => bail!(e),
            _ => {}
        }
    }
    engine.shutdown().await;
    Ok(())
}
