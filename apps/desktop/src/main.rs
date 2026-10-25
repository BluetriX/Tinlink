// Release builds run without a console window.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod account;
mod audio;
mod backend;
mod border;
mod muted_speech;
mod ptt;
mod screen;
mod session;
mod stats;
mod viewer;
mod voice;

use serde::Serialize;
use tauri::{AppHandle, Manager, State};
use tracing_subscriber::EnvFilter;

use crate::audio::{AudioDevices, AudioPrefs, DeviceKind};
use crate::session::Session;
use crate::voice::{Voice, VoiceMode};

struct AppState {
    /// The logged-in server connection, if any.
    backend: tokio::sync::Mutex<Option<backend::Backend>>,
    /// The active room connection, if any.
    session: tokio::sync::Mutex<Option<Session>>,
    voice: Voice,
    audio_prefs: AudioPrefs,
    ptt_key: ptt::PttKey,
    /// Participants (by identity) this client does not want to hear.
    locally_muted: std::sync::Mutex<std::collections::HashSet<String>>,
}

/// Errors cross the IPC boundary as plain strings.
type CmdResult<T> = Result<T, String>;

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct JoinInfo {
    identity: String,
    livekit_url: String,
}

/// Command line options that prefill the form, e.g.
/// `tinlink-desktop --name bob --room lobby --voice-activation --join --share`.
#[derive(Debug, Default, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct StartupOptions {
    server: Option<String>,
    name: Option<String>,
    room: Option<String>,
    voice_activation: bool,
    join: bool,
    /// Share the first screen right after joining.
    share: bool,
    /// Separate saved login, e.g. for a second instance with another account.
    profile: Option<String>,
}

impl StartupOptions {
    fn parse(mut args: impl Iterator<Item = String>) -> Self {
        let mut opts = Self::default();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--server" => opts.server = args.next(),
                "--name" => opts.name = args.next(),
                "--room" => opts.room = args.next(),
                "--voice-activation" => opts.voice_activation = true,
                "--join" => opts.join = true,
                "--share" => opts.share = true,
                "--profile" => {
                    opts.profile = args.next().filter(|p| {
                        !p.is_empty()
                            && p.chars()
                                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                    })
                }
                other => tracing::warn!("ignoring unknown argument {other}"),
            }
        }
        opts
    }
}

#[tauri::command]
fn startup_options(opts: State<'_, StartupOptions>) -> StartupOptions {
    opts.inner().clone()
}

/// Joins a LiveKit room with `token`, leaving any current room first.
async fn connect_voice(
    app: AppHandle,
    state: &AppState,
    token: session::TokenResponse,
) -> CmdResult<JoinInfo> {
    let mut slot = state.session.lock().await;
    if let Some(old) = slot.take() {
        old.leave(&state.voice).await;
    }
    let session = Session::join(app, token, &state.voice, &state.audio_prefs)
        .await
        .map_err(|e| format!("{e:#}"))?;
    let info = JoinInfo {
        identity: session.identity().to_owned(),
        livekit_url: session.livekit_url().to_owned(),
    };
    *slot = Some(session);
    Ok(info)
}

/// Joins a room with an unauthenticated development token
/// (`TINLINK_DEV_INSECURE_TOKENS` on the server).
#[tauri::command]
async fn dev_join(
    app: AppHandle,
    state: State<'_, AppState>,
    server: String,
    name: String,
    room: String,
) -> CmdResult<JoinInfo> {
    let token = session::fetch_token(&server, &name, &room)
        .await
        .map_err(|e| format!("{e:#}"))?;
    connect_voice(app, &state, token).await
}

#[tauri::command]
async fn leave(state: State<'_, AppState>) -> CmdResult<()> {
    if let Some(session) = state.session.lock().await.take() {
        session.leave(&state.voice).await;
    }
    Ok(())
}

#[tauri::command]
async fn audio_devices(state: State<'_, AppState>) -> CmdResult<AudioDevices> {
    if let Some(session) = state.session.lock().await.as_ref() {
        return Ok(audio::list(session.audio()));
    }
    let platform = livekit::PlatformAudio::new().map_err(err)?;
    Ok(audio::list(&platform))
}

/// Selects a microphone or output device; `None` restores the system
/// default from the next join on.
#[tauri::command]
async fn set_audio_device(
    state: State<'_, AppState>,
    kind: DeviceKind,
    id: Option<String>,
) -> CmdResult<()> {
    state.audio_prefs.set(kind, id.clone());
    if let (Some(session), Some(id)) = (state.session.lock().await.as_ref(), id) {
        audio::switch(session.audio(), kind, &id).map_err(|e| format!("{e:#}"))?;
    }
    Ok(())
}

#[tauri::command]
fn set_voice_mode(state: State<'_, AppState>, mode: VoiceMode) {
    state.voice.set_mode(mode);
}

/// Sets the push-to-talk key, e.g. "F8", "Mouse4" or "Ctrl+Shift+Space".
#[tauri::command]
fn set_ptt_key(state: State<'_, AppState>, key: String) -> CmdResult<String> {
    let hotkey: tinlink_winput::Hotkey = key.parse()?;
    let name = hotkey.to_string();
    state.ptt_key.set(Some(hotkey));
    Ok(name)
}

/// Waits up to 10 s for the user to press the new push-to-talk key or
/// mouse button and uses it.
#[tauri::command]
async fn record_ptt_key(state: State<'_, AppState>) -> CmdResult<String> {
    let hotkey = tokio::task::spawn_blocking(|| ptt::capture(std::time::Duration::from_secs(10)))
        .await
        .map_err(err)?
        .ok_or("no key pressed")?;
    let name = hotkey.to_string();
    state.ptt_key.set(Some(hotkey));
    Ok(name)
}

/// Mutes or unmutes a participant for this client only.
#[tauri::command]
async fn set_local_mute(
    state: State<'_, AppState>,
    identity: String,
    muted: bool,
) -> CmdResult<()> {
    {
        let mut set = state.locally_muted.lock().unwrap();
        if muted {
            set.insert(identity.clone());
        } else {
            set.remove(&identity);
        }
    }
    if let Some(session) = state.session.lock().await.as_ref() {
        session.set_local_mute(&identity, muted);
    }
    Ok(())
}

/// Opens the viewer of a participant's screen share again.
#[tauri::command]
async fn watch_screen(state: State<'_, AppState>, identity: String) -> CmdResult<()> {
    let slot = state.session.lock().await;
    let session = slot.as_ref().ok_or("not connected")?;
    session.watch(&identity).map_err(|e| format!("{e:#}"))
}

#[tauri::command]
fn list_screens() -> Vec<screen::ScreenSource> {
    screen::list_sources()
}

#[tauri::command]
async fn start_share(
    state: State<'_, AppState>,
    source_id: Option<u64>,
) -> CmdResult<screen::ShareInfo> {
    let mut slot = state.session.lock().await;
    let session = slot.as_mut().ok_or("not connected")?;
    session
        .start_share(source_id)
        .await
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
async fn stop_share(state: State<'_, AppState>) -> CmdResult<()> {
    if let Some(session) = state.session.lock().await.as_mut() {
        session.stop_share().await;
    }
    Ok(())
}

/// Creates the main window from the config. A `--profile` gets its own
/// WebView data folder (separate settings); debug builds open a DevTools
/// port when `TINLINK_DEVTOOLS_PORT` is set, for UI automation.
fn create_main_window(app: &tauri::App) -> tauri::Result<()> {
    let config = app
        .config()
        .app
        .windows
        .iter()
        .find(|w| w.label == "main")
        .cloned()
        .expect("main window is configured");
    let builder = tauri::WebviewWindowBuilder::from_config(app.handle(), &config)?;
    webview_environment(app.handle(), builder)?.build()?;
    Ok(())
}

/// Applies the profile's WebView data folder and debug flags. Every window
/// must use the same settings to share one WebView2 browser process.
fn webview_environment<'a>(
    app: &AppHandle,
    mut builder: tauri::WebviewWindowBuilder<'a, tauri::Wry, AppHandle>,
) -> tauri::Result<tauri::WebviewWindowBuilder<'a, tauri::Wry, AppHandle>> {
    if let Some(profile) = &app.state::<StartupOptions>().profile {
        let dir = app
            .path()
            .app_local_data_dir()?
            .join(format!("webview-{profile}"));
        builder = builder.data_directory(dir);
    }
    if cfg!(debug_assertions)
        && let Ok(port) = std::env::var("TINLINK_DEVTOOLS_PORT")
        && port.parse::<u16>().is_ok()
    {
        // Keep wry's default flags, which this replaces.
        builder = builder.additional_browser_args(&format!(
            "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection --remote-debugging-port={port}"
        ));
    }
    Ok(builder)
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                "info,tinlink_desktop=debug,livekit=info,libwebrtc=warn".into()
            }),
        )
        .init();

    // Render received video as soon as it is decoded instead of smoothing
    // playout; screen sharing favours latency over fluidity. Must run before
    // the WebRTC runtime is created by the first room connection.
    if let Err(e) = livekit::webrtc::enable_zero_playout_delay() {
        tracing::warn!("zero playout delay unavailable: {e}");
    }

    tauri::Builder::default()
        .manage(StartupOptions::parse(std::env::args().skip(1)))
        .manage(AppState {
            backend: tokio::sync::Mutex::new(None),
            session: tokio::sync::Mutex::new(None),
            voice: Voice::default(),
            audio_prefs: AudioPrefs::default(),
            ptt_key: ptt::PttKey::default(),
            locally_muted: Default::default(),
        })
        .setup(|app| {
            create_main_window(app)?;
            let key = app.state::<AppState>().ptt_key.clone();
            ptt::start(app.handle().clone(), key);
            Ok(())
        })
        .on_window_event(|window, event| {
            // Screen viewer windows must not keep the app alive.
            if window.label() == "main" && matches!(event, tauri::WindowEvent::Destroyed) {
                window.app_handle().exit(0);
            }
        })
        .invoke_handler(tauri::generate_handler![
            startup_options,
            dev_join,
            account::build_info,
            account::restore,
            account::login,
            account::register,
            account::logout,
            account::api,
            account::upload,
            account::download,
            account::image_data,
            account::join_voice,
            leave,
            set_voice_mode,
            audio_devices,
            set_audio_device,
            set_ptt_key,
            record_ptt_key,
            set_local_mute,
            watch_screen,
            list_screens,
            start_share,
            stop_share,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::StartupOptions;

    fn parse(args: &[&str]) -> StartupOptions {
        StartupOptions::parse(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn parses_startup_options() {
        let opts = parse(&[
            "--server",
            "http://example.org",
            "--name",
            "bob",
            "--room",
            "lobby",
            "--voice-activation",
            "--join",
            "--share",
            "--profile",
            "bob-2",
        ]);
        assert_eq!(opts.server.as_deref(), Some("http://example.org"));
        assert_eq!(opts.name.as_deref(), Some("bob"));
        assert_eq!(opts.room.as_deref(), Some("lobby"));
        assert!(opts.voice_activation && opts.join && opts.share);
        assert_eq!(opts.profile.as_deref(), Some("bob-2"));
    }

    #[test]
    fn ignores_unknown_and_incomplete_options() {
        // Profile names end up in a file name.
        assert_eq!(parse(&["--profile", "../x"]).profile, None);
        let opts = parse(&["--bogus", "--name"]);
        assert_eq!(opts.name, None);
        assert!(!opts.join);
    }
}
