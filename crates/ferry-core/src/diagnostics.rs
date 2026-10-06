//! Troubleshooting checks with explanations people can act on.

use crate::engine::Engine;
use crate::model::Protocol;
use crate::net::interfaces::{self, is_link_local_v6};
use serde::Serialize;
use std::net::IpAddr;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    Ok,
    Warning,
    Error,
    Unknown,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticCheck {
    pub id: String,
    pub label: String,
    pub status: CheckStatus,
    pub value: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

fn check(id: &str, label: &str, status: CheckStatus, value: impl Into<String>, detail: Option<String>) -> DiagnosticCheck {
    DiagnosticCheck { id: id.into(), label: label.into(), status, value: value.into(), detail }
}

pub async fn run(engine: &Engine) -> Vec<DiagnosticCheck> {
    let mut out = Vec::new();
    let settings = engine.settings();
    let ifaces = engine.interfaces();
    let physical: Vec<_> = ifaces.iter().filter(|i| !i.is_virtual).collect();

    out.push(match physical.first() {
        Some(i) => check("network", "Local network", CheckStatus::Ok, format!("Connected ({}, {})", i.name, i.addr), None),
        None => check(
            "network",
            "Local network",
            CheckStatus::Error,
            "Not connected",
            Some("Connect to Wi-Fi or Ethernet. Devices must be on the same network to find each other.".into()),
        ),
    });

    out.push(match engine.multicast_error() {
        None => check("discovery", "Discovery", CheckStatus::Ok, "Working", None),
        Some(err) => check(
            "discovery",
            "Discovery",
            CheckStatus::Warning,
            "Limited",
            Some(format!(
                "Ferry can't use network broadcasts ({err}). Nearby devices may not appear automatically. Use “Add by address” or a QR code instead. Another app using port {} can cause this.",
                settings.port
            )),
        ),
    });

    let port = engine.port();
    out.push(if port == settings.port {
        check("port", "Listening port", CheckStatus::Ok, port.to_string(), None)
    } else {
        check(
            "port",
            "Listening port",
            CheckStatus::Warning,
            port.to_string(),
            Some(format!(
                "Port {} is used by another app, so Ferry listens on {port}. Ferry and LocalSend devices still find it automatically.",
                settings.port
            )),
        )
    });

    out.push(if settings.encryption {
        check("encryption", "Encryption", CheckStatus::Ok, "On (HTTPS, mutual TLS)", None)
    } else {
        check(
            "encryption",
            "Encryption",
            CheckStatus::Warning,
            "Off",
            Some(
                "Transfers are not encrypted. Turn encryption back on in Settings › Network unless you need it off for an old device."
                    .into(),
            ),
        )
    });

    let v4 = physical.iter().find(|i| matches!(i.addr, IpAddr::V4(_)));
    out.push(match v4 {
        Some(i) => check("ipv4", "IPv4", CheckStatus::Ok, i.addr.to_string(), None),
        None => check("ipv4", "IPv4", CheckStatus::Warning, "Unavailable", None),
    });
    let v6 = physical.iter().find(|i| matches!(i.addr, IpAddr::V6(v6) if !is_link_local_v6(&v6)));
    let v6_ll = physical.iter().find(|i| matches!(i.addr, IpAddr::V6(_)));
    out.push(match (v6, v6_ll) {
        (Some(i), _) => check("ipv6", "IPv6", CheckStatus::Ok, i.addr.to_string(), None),
        (None, Some(_)) => check("ipv6", "IPv6", CheckStatus::Ok, "Link-local only", None),
        (None, None) => check("ipv6", "IPv6", CheckStatus::Unknown, "Unavailable", Some("Not needed: IPv4 works on every network.".into())),
    });

    let virtuals: Vec<_> = interfaces::list(true).into_iter().filter(|i| i.is_virtual).collect();
    if !virtuals.is_empty() && !settings.include_virtual_interfaces {
        out.push(check(
            "virtual",
            "VPN & virtual adapters",
            CheckStatus::Ok,
            format!("{} ignored", virtuals.len()),
            Some(format!(
                "Ferry doesn't announce itself on {} so your device isn't exposed on VPN or virtual-machine networks.",
                virtuals.iter().map(|i| i.name.as_str()).collect::<Vec<_>>().join(", ")
            )),
        ));
    }

    let devices = engine.devices();
    let online = devices.iter().filter(|d| d.online).count();
    out.push(if online > 0 {
        check("devices", "Devices nearby", CheckStatus::Ok, online.to_string(), None)
    } else {
        check(
            "devices",
            "Devices nearby",
            CheckStatus::Warning,
            "None found",
            Some("Open Ferry (or LocalSend) on the other device and make sure both are on the same Wi-Fi. Guest networks and some routers isolate devices from each other (“AP isolation”).".into()),
        )
    });

    out.extend(platform_checks(port).await);

    let signaling = engine.signaling_status();
    let note = "only connection setup goes through it; files go device to device.";
    out.push(match (signaling.url.as_deref(), signaling.state.as_str()) {
        (None, _) => check(
            "webrtc",
            "Browser & remote transfers",
            CheckStatus::Unknown,
            "Not set up",
            Some("Needed only to reach browsers and devices outside this network: set a signaling server in Settings › Network.".into()),
        ),
        (Some(url), "open") => check("webrtc", "Browser & remote transfers", CheckStatus::Ok, "Connected", Some(format!("{url}. {note}"))),
        (Some(url), "connecting") => {
            check("webrtc", "Browser & remote transfers", CheckStatus::Warning, "Connecting…", Some(format!("{url}. {note}")))
        }
        (Some(url), _) => check(
            "webrtc",
            "Browser & remote transfers",
            CheckStatus::Error,
            "Not reachable",
            Some(format!("{url}{}", signaling.error.as_deref().map(|e| format!(": {e}")).unwrap_or_default())),
        ),
    });
    let _ = Protocol::Https;
    out
}

#[cfg(windows)]
async fn platform_checks(port: u16) -> Vec<DiagnosticCheck> {
    let mut out = Vec::new();
    // Public networks block incoming connections unless the app was allowed.
    let profile = powershell("(Get-NetConnectionProfile | Where-Object { $_.IPv4Connectivity -ne 'Disconnected' } | Select-Object -ExpandProperty NetworkCategory) -join ','").await;
    match profile.as_deref() {
        Some(p) if p.contains("Public") => out.push(check(
            "firewall",
            "Firewall",
            CheckStatus::Warning,
            "Possible issue",
            Some(format!(
                "This network is set to Public, where Windows Firewall blocks incoming transfers unless Ferry is allowed. Set the network to Private (Settings › Network & internet › Wi-Fi › your network), or allow Ferry when Windows asks. Ferry needs TCP and UDP port {port}."
            )),
        )),
        Some(p) if !p.is_empty() => out.push(check("firewall", "Firewall", CheckStatus::Ok, format!("OK ({p} network)"), None)),
        _ => out.push(check("firewall", "Firewall", CheckStatus::Unknown, "Couldn't check", None)),
    }
    out.push(check("permission", "Local network permission", CheckStatus::Ok, "Not required on Windows", None));
    out
}

#[cfg(windows)]
async fn powershell(script: &str) -> Option<String> {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let output = tokio::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    let output = tokio::time::timeout(std::time::Duration::from_secs(8), output).await.ok()?.ok()?;
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(not(windows))]
async fn platform_checks(_port: u16) -> Vec<DiagnosticCheck> {
    let mut out = Vec::new();
    if cfg!(target_os = "macos") || cfg!(target_os = "ios") {
        out.push(check(
            "permission",
            "Local network permission",
            CheckStatus::Unknown,
            "See system settings",
            Some("If devices don't appear, allow Ferry under System Settings › Privacy & Security › Local Network.".into()),
        ));
    }
    out
}
