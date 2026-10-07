# Platform limitations

What each platform fundamentally allows, and how Ferry behaves at the edge.
The UI reads these as capability flags (the `Capabilities` interface in
`apps/app/src/platform/types.ts`, filled in by `native.ts` and `web.ts`)
instead of assuming parity.

**Status.** The desktop app is verified by hand on Windows. The release
workflow builds macOS (universal) and Linux (AppImage, `.deb`, `.rpm`)
installers, and CI compiles and tests the desktop shell on both, but neither is
tested by hand yet. The browser app is tested end to end in Chrome, in CI and
by hand. Android and iOS apps are Planned: no mobile project is
generated yet, so their columns below describe the intended design.

## Matrix

| Capability | Windows | macOS | Linux | Android (Planned) | iOS / iPadOS (Planned) | Browser (PWA) |
|---|---|---|---|---|---|---|
| LAN discovery (multicast) | ✅ | ✅ | ✅ | needs Local Network permission on 17+ | needs Local Network permission + multicast entitlement | ❌ browsers cannot do UDP |
| Receive in background | ✅ tray | ✅ menu bar | ✅ tray | foreground service while receiving is on | only while foregrounded (+ ~30 s finishing time) | ⚠️ only while the tab is open |
| Host a server | ✅ | ✅ | ✅ | ✅ | while active | ❌ |
| Send to LocalSend devices | ✅ | ✅ | ✅ | ✅ | ✅ | ❌ (needs HTTPS with self-signed certs) |
| WebRTC | ✅ (Rust) | ✅ | ✅ | ✅ | ✅ | ✅ |
| Share sheet *into* Ferry | ✅ Explorer "Send with Ferry" verb + *Send to* shortcut (NSIS installer) | Planned: share extension + Services | Planned: `.desktop` MIME handler / Nautilus script | ACTION_SEND | share extension | ⚠️ Web Share Target (Chromium, installed PWA only) |
| Save anywhere | ✅ | ✅ | ✅ | Downloads or SAF folder | app container (visible in Files) | ⚠️ files stay in browser storage; *Save* downloads a copy |
| Clipboard read | ✅ on demand | ✅ on demand | ✅ X11; ⚠️ Wayland needs focus | only while focused (10+) | paste prompt each time | ⚠️ user gesture + permission |
| Tray / menu bar | ✅ | ✅ | ⚠️ needs AppIndicator (GNOME extension) | n/a | n/a | n/a |

## Per platform

### Windows
- **Firewall:** a new binary listening on 53317 triggers the Windows Defender prompt on first run; on the *Public* profile inbound traffic is blocked silently. The installer is per-user (no admin rights), so it can't add firewall rules itself. Diagnostics detects a Public network and explains the fix (switch the network to Private, or allow Ferry when Windows asks).
- **Explorer:** the NSIS installer adds "Send with Ferry" for files and folders and a *Send to › Ferry* shortcut (per user, removed on uninstall); the MSI does not. Windows 11 shows classic verbs under *Show more options*. A top-level entry or a Share target needs a signed sparse package (MSIX), which needs a code-signing certificate this repo doesn't have (Planned).
- **Many virtual adapters** (VMware, Hyper-V, WSL, Docker, VPNs such as NordLynx/WireGuard) are common. Discovery skips adapters that are down, loopback, link-local IPv4 (169.254/16) and known virtual/VPN interfaces by default (configurable), so we don't announce our identity into VPN subnets.
- WebView2 is preinstalled on Windows 11 and evergreen on Windows 10; the installer bootstraps it when missing.
- Release builds are x64 only (ARM64 Planned) and unsigned; signing requires the project's own code-signing certificate.

### macOS
- The release workflow builds an unsigned universal app and DMG; it is not tested by hand yet.
- Local Network permission prompt (macOS 15+) on first discovery. Diagnostics can't detect a denial; it points to *System Settings › Privacy & Security › Local Network*.
- Planned: App Sandbox (required for the App Store) with security-scoped bookmarks for the save location, a Finder share extension and Services menu entry (these need a signed app with a team id).

### Linux
- The release workflow builds AppImage, `.deb` and `.rpm` packages on Ubuntu 22.04; they are not tested by hand yet. A Flatpak manifest is Planned.
- WebKitGTK's `backdrop-filter` is slow on some GPUs/drivers. Settings › Appearance › Transparency can be set to Reduced, and by default the app follows the system's reduced-transparency preference. An automatic fallback based on measured frame times is Planned.
- Tray icons need AppIndicator support (absent on stock GNOME).
- Wayland: no global clipboard reads without focus; no window positioning.

### Android (Planned)
Not built yet. The design:
- Android 17 introduces `ACCESS_LOCAL_NETWORK`; without it discovery and receiving fail. Request it in context, with an explanation and a fallback (QR/manual address).
- Receiving in the background requires a foreground service (`dataSync`/`connectedDevice` type) with a persistent notification. "Ready to receive" is an explicit user choice, not a silent default.
- Doze can suspend networking when the service isn't running.
- Scoped storage: default destination `Download/Ferry` via MediaStore; other folders via SAF (`ACTION_OPEN_DOCUMENT_TREE`) with file descriptors handed to Rust.
- Use the system Photo Picker instead of broad media permissions.
- Verification needs a real phone: an emulator covers transfer logic but not multicast.

### iOS / iPadOS (Planned)
Not built yet (needs a Mac with Xcode). The design:
- iOS suspends networking shortly after the app leaves the foreground. Transfers in progress get ~30 s via `beginBackgroundTask`; after that the transfer pauses and resumes when the app returns (Ferry's resume protocol makes this tolerable).
- Requires `NSLocalNetworkUsageDescription`, `NSBonjourServices`, and the `com.apple.developer.networking.multicast` entitlement (Apple approval).
- Received files live in the app container, exposed through the Files app.
- Share extension memory limit (~120 MB) → the extension only hands file URLs to the app via an app group; it never reads file contents.

### Browsers / PWA
- No UDP and no listening sockets: discovery uses the signaling server (peers sharing a public IP or a room); transfers use WebRTC.
- **Browsers cannot talk to LocalSend devices directly**: LocalSend requires HTTPS with self-signed certificates *and* client certificates, which browsers can't present. Bridge: open a LAN browser link served by a native Ferry device (`ferry share`, `ferry receive --browser`, or the desktop app). Relaying through the native app is Planned.
- Local Network Access (Chromium 141+) prompts before a public page can reach private IPs; the LAN browser link avoids this because it is itself a local page.
- Receiving: files stream into the origin-private file system (OPFS) chunk by chunk, so memory stays bounded; the Inbox's *Save* downloads a copy. Browsers without OPFS writers keep the whole file as a Blob in IndexedDB, capped at 256 MB per file. Storage counts against the browser's quota (Diagnostics shows usage) and Ferry asks for persistent storage so received files aren't evicted.
- A transfer lives in its tab: closing the tab during a transfer asks for confirmation, and background tabs may be throttled. If the connection drops, the sender reconnects and both sides continue from the bytes already stored (the receiver keeps partial files for 24 hours; leftovers are deleted the next time the app opens); a sender that reloads the page loses its file handles, so that transfer starts over.
- Throughput is bounded by the browser's data channel: about 14 MB/s browser ↔ browser in our measurements (both on one machine), versus 300+ MB/s between native apps on a LAN.
- Web Share Target only works for an *installed* PWA in Chromium. File handlers are Planned.
