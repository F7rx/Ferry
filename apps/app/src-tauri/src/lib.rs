//! Ferry's desktop shell: hosts the engine, exposes it to the UI, and owns the
//! OS integrations (tray, notifications, single instance, drag-and-drop paths).

use ferry_core::diagnostics::DiagnosticCheck;
use ferry_core::events::EngineEvent;
use ferry_core::model::*;
use ferry_core::pairing::{OutgoingPairing, PairingOffer};
use ferry_core::{Engine, EngineConfig, EngineSnapshot, ErrorInfo, SendItem, Settings, Target};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, State, WindowEvent};
use tauri_plugin_notification::NotificationExt;

pub mod preview;

const EVENT: &str = "ferry://event";
/// Events were dropped (the UI fell behind): it reloads the snapshot.
const RESYNC: &str = "ferry://resync";
/// Received files the UI may now preview (paths as history spells them).
const PREVIEWABLE: &str = "ferry://previewable";

struct AppState {
    engine: Arc<Engine>,
    /// Paths handed to us on the command line (this launch or a later one),
    /// taken once by the UI.
    pending_paths: Mutex<Vec<String>>,
}

/// A page of history, plus which of its files the UI may preview.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HistoryPage {
    entries: Vec<HistoryEntry>,
    previewable: Vec<String>,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
enum ItemDto {
    Path { path: String },
    Text { text: String },
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
enum TargetDto {
    Device { id: String },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PathInfo {
    path: String,
    name: String,
    size: Option<u64>,
    is_dir: bool,
}

type CmdResult<T> = Result<T, ErrorInfo>;

fn info(err: ferry_core::FerryError) -> ErrorInfo {
    err.info()
}

/// The engine's whole state, read on startup and after `ferry://resync`. It
/// never carries launch paths, so reloading it can't take or repeat them.
#[tauri::command]
fn snapshot(state: State<'_, AppState>) -> EngineSnapshot {
    state.engine.snapshot()
}

#[tauri::command]
async fn send(state: State<'_, AppState>, targets: Vec<TargetDto>, items: Vec<ItemDto>) -> CmdResult<Vec<String>> {
    let targets = targets.into_iter().map(|t| match t {
        TargetDto::Device { id } => Target::Device { id },
    });
    let items = items.into_iter().map(|i| match i {
        ItemDto::Path { path } => SendItem::Path { path: PathBuf::from(path) },
        ItemDto::Text { text } => SendItem::Text { text },
    });
    state.engine.send(targets.collect(), items.collect()).await.map_err(info)
}

#[tauri::command]
fn respond(state: State<'_, AppState>, request_id: String, decision: Decision) -> bool {
    state.engine.respond(&request_id, decision)
}

#[tauri::command]
fn cancel(state: State<'_, AppState>, id: String) -> bool {
    state.engine.cancel(&id)
}

#[tauri::command]
fn pause(state: State<'_, AppState>, id: String) -> bool {
    state.engine.pause(&id)
}

#[tauri::command]
fn resume(state: State<'_, AppState>, id: String) -> bool {
    state.engine.resume(&id)
}

#[tauri::command]
fn submit_pin(state: State<'_, AppState>, id: String, pin: Option<String>) -> bool {
    state.engine.submit_pin(&id, pin)
}

#[tauri::command]
fn dismiss(state: State<'_, AppState>, id: String) -> bool {
    state.engine.dismiss(&id)
}

#[tauri::command]
fn transfer_files(state: State<'_, AppState>, id: String) -> Vec<TransferFile> {
    state.engine.transfer_files(&id).unwrap_or_default()
}

#[tauri::command]
async fn refresh_devices(state: State<'_, AppState>) -> CmdResult<()> {
    state.engine.refresh_devices().await;
    Ok(())
}

#[tauri::command]
async fn add_device(state: State<'_, AppState>, host: String, port: u16) -> CmdResult<DeviceSummary> {
    state.engine.add_device(&host, port).await.map_err(info)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FlagsDto {
    trusted: Option<bool>,
    favorite: Option<bool>,
    mine: Option<bool>,
    /// Present = change it (null clears the custom name).
    #[serde(default, with = "double_option")]
    custom_alias: Option<Option<String>>,
}

mod double_option {
    use serde::{Deserialize, Deserializer};
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Option<String>>, D::Error> {
        Ok(Some(Option::<String>::deserialize(d)?))
    }
}

#[tauri::command]
fn set_device_flags(state: State<'_, AppState>, id: String, flags: FlagsDto) -> CmdResult<Option<DeviceSummary>> {
    state.engine.set_device_flags(&id, flags.trusted, flags.favorite, flags.mine, flags.custom_alias).map_err(info)
}

#[tauri::command]
fn create_pairing_offer(state: State<'_, AppState>) -> CmdResult<PairingOffer> {
    state.engine.create_pairing_offer().map_err(info)
}

#[tauri::command]
fn cancel_pairing_offer(state: State<'_, AppState>, id: String) -> bool {
    state.engine.cancel_pairing_offer(&id)
}

#[tauri::command]
async fn pair_with_uri(state: State<'_, AppState>, uri: String) -> CmdResult<DeviceSummary> {
    state.engine.pair_with_uri(&uri).await.map_err(info)
}

// Async so the engine's background task starts inside the Tokio runtime.
#[tauri::command]
async fn start_code_pairing(state: State<'_, AppState>, device_id: String) -> CmdResult<OutgoingPairing> {
    state.engine.start_code_pairing(&device_id).await.map_err(info)
}

#[tauri::command]
fn cancel_code_pairing(state: State<'_, AppState>, id: String) -> bool {
    state.engine.cancel_code_pairing(&id)
}

#[tauri::command]
fn respond_pairing(state: State<'_, AppState>, request_id: String, accept: bool) -> bool {
    state.engine.respond_pairing(&request_id, accept)
}

#[tauri::command]
async fn unpair_device(state: State<'_, AppState>, id: String) -> CmdResult<Option<DeviceSummary>> {
    state.engine.unpair_device(&id).map_err(info)
}

// ── WebRTC: signaling status and private links ─────────────────────────────

#[tauri::command]
fn signaling_status(state: State<'_, AppState>) -> SignalingStatus {
    state.engine.signaling_status()
}

// Async so anything the engine spawns runs inside the Tokio runtime.
#[tauri::command]
async fn create_room(state: State<'_, AppState>) -> CmdResult<RoomInfo> {
    Ok(state.engine.create_room())
}

#[tauri::command]
async fn join_room(state: State<'_, AppState>, link: String) -> CmdResult<RoomInfo> {
    state.engine.join_room(&link).map_err(info)
}

#[tauri::command]
async fn leave_room(state: State<'_, AppState>, id: String) -> CmdResult<bool> {
    Ok(state.engine.leave_room(&id))
}

#[tauri::command]
fn rooms(state: State<'_, AppState>) -> Vec<RoomInfo> {
    state.engine.rooms()
}

#[tauri::command]
fn forget_device(state: State<'_, AppState>, id: String) -> CmdResult<()> {
    state.engine.forget_device(&id).map_err(info)
}

// Async: checking each file on disk stays off the main thread.
#[tauri::command]
async fn history(
    app: AppHandle,
    state: State<'_, AppState>,
    limit: u32,
    before_id: Option<i64>,
    direction: Option<Direction>,
) -> CmdResult<HistoryPage> {
    let entries = state.engine.history(limit, before_id, direction).map_err(info)?;
    let previewable = grant_previews(&app.asset_protocol_scope(), &state.engine.transfers(), &entries);
    Ok(HistoryPage { entries, previewable })
}

/// Lets the webview load the received files among `entries` through the
/// asset protocol, one validated file at a time (never a folder). A file
/// whose transfer is still listed must lie inside that transfer's save
/// folder. Returns the granted paths as the entries spell them.
pub fn grant_previews(scope: &tauri::scope::fs::Scope, transfers: &[TransferSummary], entries: &[HistoryEntry]) -> Vec<String> {
    entries
        .iter()
        .filter_map(|entry| {
            let path = preview::received_file(entry)?;
            let root = transfers.iter().find(|t| t.id == entry.transfer_id).and_then(|t| t.save_dir.as_deref()).map(Path::new);
            let canonical = preview::previewable_file(Path::new(path), root)?;
            scope.allow_file(&canonical).ok()?;
            Some(path.to_string())
        })
        .collect()
}

#[tauri::command]
fn delete_history(state: State<'_, AppState>, id: i64) -> CmdResult<bool> {
    state.engine.delete_history(id).map_err(info)
}

#[tauri::command]
fn clear_history(state: State<'_, AppState>) -> CmdResult<()> {
    state.engine.clear_history().map_err(info)
}

#[tauri::command]
async fn update_settings(app: AppHandle, state: State<'_, AppState>, settings: Settings) -> CmdResult<Settings> {
    let updated = state.engine.update_settings(settings).await.map_err(info)?;
    rebuild_tray(&app);
    Ok(updated)
}

#[tauri::command]
async fn diagnostics(state: State<'_, AppState>) -> CmdResult<Vec<DiagnosticCheck>> {
    Ok(state.engine.diagnostics().await)
}

#[tauri::command]
async fn share_with_browsers(
    state: State<'_, AppState>,
    items: Vec<ItemDto>,
    pin: Option<String>,
) -> CmdResult<ferry_core::browser::BrowserShareInfo> {
    let items = items
        .into_iter()
        .filter_map(|i| match i {
            ItemDto::Path { path } => Some(SendItem::Path { path: PathBuf::from(path) }),
            ItemDto::Text { .. } => None,
        })
        .collect();
    state.engine.share_with_browsers(items, pin).await.map_err(info)
}

#[tauri::command]
async fn receive_from_browsers(state: State<'_, AppState>, pin: Option<String>) -> CmdResult<ferry_core::browser::BrowserShareInfo> {
    state.engine.receive_from_browsers(pin).await.map_err(info)
}

#[tauri::command]
fn stop_browser_link(state: State<'_, AppState>, id: String) -> bool {
    state.engine.stop_browser_link(&id)
}

#[tauri::command]
fn browser_links(state: State<'_, AppState>) -> Vec<ferry_core::browser::BrowserShareInfo> {
    state.engine.browser_links()
}

/// Describes OS paths (drag-and-drop, pickers) without reading file contents.
#[tauri::command]
fn path_info(paths: Vec<String>) -> Vec<PathInfo> {
    paths
        .into_iter()
        .filter_map(|p| {
            let meta = std::fs::metadata(&p).ok()?;
            let name = std::path::Path::new(&p).file_name()?.to_string_lossy().into_owned();
            Some(PathInfo { name, size: meta.is_file().then_some(meta.len()), is_dir: meta.is_dir(), path: p })
        })
        .collect()
}

/// Opens a file Ferry handled (received/sent, or inside the save folder).
#[tauri::command]
fn open_path(app: AppHandle, state: State<'_, AppState>, path: String) -> CmdResult<()> {
    use tauri_plugin_opener::OpenerExt;
    let p = PathBuf::from(&path);
    if !state.engine.is_known_path(&p) {
        return Err(ErrorInfo::new("not_allowed", "Ferry only opens files it received or sent."));
    }
    app.opener().open_path(path, None::<&str>).map_err(|e| ErrorInfo::new("open_failed", e.to_string()))
}

#[tauri::command]
fn reveal_path(app: AppHandle, state: State<'_, AppState>, path: String) -> CmdResult<()> {
    use tauri_plugin_opener::OpenerExt;
    let p = PathBuf::from(&path);
    if !state.engine.is_known_path(&p) {
        return Err(ErrorInfo::new("not_allowed", "Ferry only reveals files it received or sent."));
    }
    app.opener().reveal_item_in_dir(p).map_err(|e| ErrorInfo::new("reveal_failed", e.to_string()))
}

#[tauri::command]
fn take_pending_paths(state: State<'_, AppState>) -> Vec<String> {
    std::mem::take(&mut *state.pending_paths.lock().unwrap())
}

/// Paths passed as arguments (`ferry-desktop file1 file2`, Explorer "Send with Ferry").
fn arg_paths(args: &[String]) -> Vec<String> {
    args.iter().skip(1).filter(|a| !a.starts_with('-')).filter(|a| std::path::Path::new(a).exists()).cloned().collect()
}

fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

fn window_focused(app: &AppHandle) -> bool {
    app.get_webview_window("main").and_then(|w| w.is_focused().ok()).unwrap_or(false)
}

fn notify(app: &AppHandle, title: &str, body: &str) {
    let _ = app.notification().builder().title(title).body(body).show();
}

/// Side effects of engine events outside the webview.
fn on_engine_event(app: &AppHandle, event: &EngineEvent) {
    match event {
        EngineEvent::IncomingRequest { request } if !window_focused(app) => {
            let body = match &request.text {
                Some(text) => text.chars().take(140).collect(),
                None => format!(
                    "wants to send {} {} ({})",
                    request.files.len(),
                    if request.files.len() == 1 { "item" } else { "items" },
                    ferry_core::util::format_bytes(request.total_bytes)
                ),
            };
            notify(app, &request.peer.alias, &body);
        }
        EngineEvent::PairingRequest { request } if !window_focused(app) => {
            notify(app, &request.peer.alias, &format!("wants to add this device to its devices. Code {}", request.code));
        }
        EngineEvent::TransferUpdated { transfer: t }
            if t.direction == Direction::Receive && t.state == TransferState::Completed && !window_focused(app) =>
        {
            let what = if t.file_count == 1 { t.title.clone() } else { format!("{} files", t.file_count) };
            notify(app, "Received", &format!("{what} from {}", t.peer.alias));
        }
        // Granted before the entry reaches the UI, which then shows its preview.
        EngineEvent::HistoryAdded { entry } => {
            if let Some(state) = app.try_state::<AppState>() {
                let granted = grant_previews(&app.asset_protocol_scope(), &state.engine.transfers(), std::slice::from_ref(entry));
                if !granted.is_empty() {
                    let _ = app.emit(PREVIEWABLE, &granted);
                }
            }
        }
        EngineEvent::DeviceUpdated { device } if device.favorite || device.mine => rebuild_tray(app),
        _ => {}
    }
}

const TRAY_ID: &str = "ferry-tray";

fn build_tray_menu(app: &AppHandle) -> tauri::Result<Menu<tauri::Wry>> {
    let state = app.state::<AppState>();
    let engine = &state.engine;
    let open = MenuItem::with_id(app, "open", "Open Ferry", true, None::<&str>)?;
    let quick: Vec<DeviceSummary> = engine.devices().into_iter().filter(|d| (d.favorite || d.mine) && d.online).collect();
    let send_menu = Submenu::with_id(app, "send", "Quick Send", true)?;
    if quick.is_empty() {
        send_menu.append(&MenuItem::with_id(app, "send-none", "No favorite devices online", false, None::<&str>)?)?;
    }
    for d in &quick {
        let label = format!("{}…", d.custom_alias.as_deref().unwrap_or(&d.alias));
        send_menu.append(&MenuItem::with_id(app, format!("send:{}", d.id), label, true, None::<&str>)?)?;
    }
    let receiving = CheckMenuItem::with_id(app, "receiving", "Receiving", true, engine.settings().receive_enabled, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit Ferry", true, None::<&str>)?;
    Menu::with_items(app, &[&open, &send_menu, &receiving, &PredefinedMenuItem::separator(app)?, &quit])
}

fn rebuild_tray(app: &AppHandle) {
    if let (Some(tray), Ok(menu)) = (app.tray_by_id(TRAY_ID), build_tray_menu(app)) {
        let _ = tray.set_menu(Some(menu));
    }
}

fn setup_tray(app: &AppHandle) -> tauri::Result<()> {
    let menu = build_tray_menu(app)?;
    let mut builder = TrayIconBuilder::with_id(TRAY_ID).tooltip("Ferry").menu(&menu).show_menu_on_left_click(false);
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    builder
        .on_menu_event(|app, event| {
            let id = event.id.as_ref();
            match id {
                "open" => show_main(app),
                "quit" => app.exit(0),
                "receiving" => {
                    let state = app.state::<AppState>();
                    let engine = state.engine.clone();
                    let app = app.clone();
                    tauri::async_runtime::spawn(async move {
                        let mut s = engine.settings();
                        s.receive_enabled = !s.receive_enabled;
                        let _ = engine.update_settings(s).await;
                        rebuild_tray(&app);
                    });
                }
                _ if id.starts_with("send:") => {
                    let device_id = id["send:".len()..].to_string();
                    let app = app.clone();
                    // Pick files, then send without opening the main window.
                    std::thread::spawn(move || {
                        use tauri_plugin_dialog::DialogExt;
                        let Some(files) = app.dialog().file().set_title("Quick Send").blocking_pick_files() else { return };
                        let items: Vec<SendItem> =
                            files.into_iter().filter_map(|f| f.into_path().ok()).map(|path| SendItem::Path { path }).collect();
                        let engine = app.state::<AppState>().engine.clone();
                        tauri::async_runtime::spawn(async move {
                            let _ = engine.send(vec![Target::Device { id: device_id }], items).await;
                        });
                    });
                }
                _ => {}
            }
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                show_main(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_env("FERRY_LOG").unwrap_or_else(|_| "info".into()))
        .init();

    let mut builder = tauri::Builder::default();
    #[cfg(desktop)]
    {
        builder = builder.plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            // A second launch (e.g. "Send with Ferry" in Explorer) hands over its files.
            let paths = arg_paths(&args);
            if !paths.is_empty() {
                // Queued, then announced: the UI takes the queue, so paths that
                // arrive before its listener is up wait there instead of being lost.
                if let Some(state) = app.try_state::<AppState>() {
                    state.pending_paths.lock().unwrap().extend(paths.iter().cloned());
                }
                let _ = app.emit("ferry://paths", &paths);
            }
            show_main(app);
        }));
        builder =
            builder.plugin(tauri_plugin_autostart::init(tauri_plugin_autostart::MacosLauncher::LaunchAgent, Some(vec!["--background"])));
    }
    builder
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let data_dir = app.path().app_data_dir()?;
            let engine = tauri::async_runtime::block_on(Engine::start(EngineConfig::persistent(data_dir)))
                .map_err(|e| anyhow::anyhow!(e.info().message))?;

            let args: Vec<String> = std::env::args().collect();
            let background = args.iter().any(|a| a == "--background");
            app.manage(AppState { engine: engine.clone(), pending_paths: Mutex::new(arg_paths(&args)) });

            let handle = app.handle().clone();
            let mut rx = engine.subscribe();
            tauri::async_runtime::spawn(async move {
                loop {
                    match rx.recv().await {
                        Ok(event) => {
                            on_engine_event(&handle, &event);
                            let _ = handle.emit(EVENT, &event);
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            let _ = handle.emit(RESYNC, ());
                        }
                        Err(_) => break,
                    }
                }
            });

            #[cfg(desktop)]
            setup_tray(app.handle())?;
            if background && let Some(w) = app.get_webview_window("main") {
                let _ = w.hide();
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing the window keeps Ferry receiving in the tray.
            #[cfg(desktop)]
            if let WindowEvent::CloseRequested { api, .. } = event {
                let _ = window.hide();
                api.prevent_close();
            }
        })
        .invoke_handler(tauri::generate_handler![
            snapshot,
            send,
            respond,
            cancel,
            pause,
            resume,
            submit_pin,
            dismiss,
            transfer_files,
            refresh_devices,
            add_device,
            set_device_flags,
            forget_device,
            history,
            delete_history,
            clear_history,
            update_settings,
            diagnostics,
            path_info,
            open_path,
            reveal_path,
            take_pending_paths,
            share_with_browsers,
            receive_from_browsers,
            stop_browser_link,
            browser_links,
            create_pairing_offer,
            cancel_pairing_offer,
            pair_with_uri,
            start_code_pairing,
            cancel_code_pairing,
            respond_pairing,
            unpair_device,
            signaling_status,
            create_room,
            join_room,
            leave_room,
            rooms,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Ferry");
}
