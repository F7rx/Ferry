# Platform limitations

What each platform fundamentally allows, and how Ferry behaves at the edge.
The UI reads these as capability flags (`capabilities.ts` / `Capabilities` in
ferry-core) instead of assuming parity.

## Matrix

| Capability | Windows | macOS | Linux | Android | iOS / iPadOS | Browser (PWA) |
|---|---|---|---|---|---|---|
| LAN discovery (multicast) | ✅ | ✅ | ✅ | ✅ (Local Network permission on 17+) | ✅ after Local Network permission + multicast entitlement | ❌ browsers cannot do UDP |
| Receive in background | ✅ tray | ✅ menu bar | ✅ tray | ✅ foreground service while receiving is on | ⚠️ only while foregrounded (+ ~30 s finishing time) | ⚠️ only while the tab is open |
| Host a server | ✅ | ✅ | ✅ | ✅ | ✅ while active | ❌ |
| Send to LocalSend devices | ✅ | ✅ | ✅ | ✅ | ✅ | ❌ (needs HTTPS with self-signed certs) |
| WebRTC | ✅ (Rust) | ✅ | ✅ | ✅ | ✅ | ✅ |
| Share sheet *into* Ferry | ✅ Share target (sparse MSIX) + Explorer verb | ✅ Share extension + Services | ⚠️ `.desktop` MIME handler / Nautilus script | ✅ ACTION_SEND | ✅ Share extension | ⚠️ Web Share Target (Chromium, installed PWA only) |
| Save anywhere | ✅ | ✅ (sandbox: user-picked folders) | ✅ | ⚠️ Downloads or SAF folder | ⚠️ app container (visible in Files) | ⚠️ File System Access (Chromium) / download prompt |
| Clipboard read | ✅ on demand | ✅ on demand | ✅ X11; ⚠️ Wayland needs focus | ⚠️ only while focused (10+) | ⚠️ paste prompt each time | ⚠️ user gesture + permission |
| Tray / menu bar | ✅ | ✅ | ⚠️ needs AppIndicator (GNOME extension) | n/a | n/a | n/a |

## Per platform

### Windows
- **Firewall:** a new binary listening on 53317 triggers the Windows Defender prompt on first run; on the *Public* profile inbound traffic is blocked silently. The installer is per-user (no admin rights), so it can't add firewall rules itself. Diagnostics detects a Public network and explains the fix (switch the network to Private, or allow Ferry when Windows asks).
- **Explorer:** the installer adds "Send with Ferry" for files and folders and a *Send to › Ferry* shortcut (per user, removed on uninstall). Windows 11 shows classic verbs under *Show more options*; a top-level entry needs an `IExplorerCommand` shell extension in a signed sparse package, which needs a code-signing certificate this repo doesn't have.
- **Many virtual adapters** (VMware, Hyper-V, WSL, Docker, VPNs such as NordLynx/WireGuard) are common. Discovery skips adapters that are down, loopback, link-local IPv4 (169.254/16) and known virtual/VPN interfaces by default (configurable), so we don't announce our identity into VPN subnets.
- WebView2 is preinstalled on Windows 11 and evergreen on Windows 10; the installer bootstraps it when missing.
- ARM64 and x64 builds; signing requires the project's own code-signing certificate (not available in this repo).

### macOS
- App Sandbox (required for the App Store; recommended for DMG) limits writes to user-chosen folders → security-scoped bookmarks for the save location.
- Local Network permission prompt (macOS 15+) on first discovery; Diagnostics explains a denial.
- Finder share extension and Services menu need a signed app with a team id; the build pipeline is documented but cannot run on Windows CI.
- **Cannot be built or tested from this Windows workstation.** Needs a Mac for build and verification.

### Linux
- WebKitGTK's `backdrop-filter` is slow on some GPUs/drivers → the app measures frame time during the first animations and switches to reduced transparency automatically.
- Tray icons need AppIndicator support (absent on stock GNOME).
- Wayland: no global clipboard reads without focus; no window positioning.
- Packaging: AppImage, `.deb`, `.rpm` (Tauri bundler); Flatpak manifest later.

### Android
- Android 17 introduces `ACCESS_LOCAL_NETWORK`; without it discovery and receiving fail. We request it in context, with an explanation and a fallback (QR/manual address).
- Receiving in the background requires a foreground service (`dataSync`/`connectedDevice` type) with a persistent notification. "Ready to receive" is an explicit user choice, not a silent default.
- Doze can suspend networking when the service isn't running.
- Scoped storage: default destination `Download/Ferry` via MediaStore; other folders via SAF (`ACTION_OPEN_DOCUMENT_TREE`) with file descriptors handed to Rust.
- Use the system Photo Picker instead of broad media permissions.
- **Cannot be verified on a real device from this workstation without a connected phone**; an emulator covers transfer logic but not multicast.

### iOS / iPadOS
- iOS suspends networking shortly after the app leaves the foreground. Transfers in progress get ~30 s via `beginBackgroundTask`; after that the transfer pauses and **resumes automatically** when the app returns (Ferry's resume protocol makes this tolerable).
- Requires `NSLocalNetworkUsageDescription`, `NSBonjourServices`, and the `com.apple.developer.networking.multicast` entitlement (Apple approval).
- Received files live in the app container, exposed through the Files app.
- Share extension memory limit (~120 MB) → the extension only hands file URLs to the app via an app group; it never reads file contents.
- **Needs a Mac with Xcode to build.** Not buildable here.

### Browsers / PWA
- No UDP and no listening sockets: discovery uses the signaling server (peers sharing a public IP or a room); transfers use WebRTC.
- **Browsers cannot talk to LocalSend devices directly**: LocalSend requires HTTPS with self-signed certificates *and* client certificates, which browsers can't present. Bridge: the native Ferry app can relay, or the user opens a LAN browser link served by a native device.
- Local Network Access (Chromium 141+) prompts before a public page can reach private IPs; the LAN browser link avoids this because it is itself a local page.
- Receiving: files stream into the origin-private file system (OPFS) chunk by chunk, so memory stays bounded; the Inbox's *Save* downloads a copy. Browsers without OPFS writers fall back to IndexedDB, capped at 256 MB per file. Storage counts against the browser's quota (Diagnostics shows usage) and Ferry asks for persistent storage so received files aren't evicted.
- A transfer lives in its tab: closing the tab during a transfer asks for confirmation, and background tabs may be throttled. If the connection drops, the sender reconnects and both sides continue from the bytes already stored (the receiver keeps partial files for 24 hours); a sender that reloads the page loses its file handles, so that transfer starts over.
- Throughput is bounded by the browser's data channel: about 14 MB/s browser ↔ browser in our measurements (both on one machine), versus 300+ MB/s between native apps on a LAN.
- Web Share Target and file handlers only work for an *installed* PWA in Chromium.
