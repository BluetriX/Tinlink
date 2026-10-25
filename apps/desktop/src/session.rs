//! A connection to one LiveKit room: microphone, playout, screen share
//! viewers and periodic state snapshots for the UI.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use livekit::options::TrackPublishOptions;
use livekit::prelude::*;
use livekit::{PlatformAudio, RoomOptions};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;

use crate::AppState;
use crate::audio::AudioPrefs;
use crate::muted_speech::MutedSpeechMonitor;
use crate::screen::{ScreenShare, ShareInfo};
use crate::stats::{self, LinkStats};
use crate::viewer::Viewer;
use crate::voice::Voice;

/// How often the participant list is pushed to the UI.
const SNAPSHOT_INTERVAL: Duration = Duration::from_millis(200);
/// Stats are collected on every n-th snapshot.
const STATS_EVERY: u32 = 5;

pub struct Session {
    app: AppHandle,
    room: Arc<Room>,
    /// Open screen share viewers by track; the event loop adds and removes them.
    viewers: Viewers,
    identity: String,
    livekit_url: String,
    share: Option<ScreenShare>,
    tasks: Vec<JoinHandle<()>>,
    /// Keeps the platform audio device module (WASAPI capture and playout) alive.
    audio: PlatformAudio,
    /// Beeps when the user talks while nothing is transmitted.
    _muted_speech: MutedSpeechMonitor,
}

type Viewers = Arc<std::sync::Mutex<HashMap<TrackSid, Viewer>>>;

/// Mutes or unmutes `identity`'s audio for this client only: a disabled
/// remote track is played out at volume zero.
fn apply_local_mute(room: &Room, identity: &str, muted: bool) {
    for participant in room.remote_participants().into_values() {
        if participant.identity().as_str() != identity {
            continue;
        }
        for publication in participant.track_publications().into_values() {
            if let Some(RemoteTrack::Audio(track)) = publication.track() {
                track.rtc_track().set_enabled(!muted);
            }
        }
    }
}

/// A LiveKit URL and access token, from the dev endpoint or a channel.
#[derive(Deserialize)]
pub struct TokenResponse {
    pub url: String,
    pub token: String,
    pub identity: String,
}

impl std::fmt::Debug for TokenResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenResponse")
            .field("url", &self.url)
            .field("token", &"<redacted>")
            .field("identity", &self.identity)
            .finish()
    }
}

/// Fetches a LiveKit token from the Tinlink server's development endpoint.
pub async fn fetch_token(server: &str, name: &str, room: &str) -> Result<TokenResponse> {
    let client = crate::backend::http_client();
    let url = format!("{}/api/voice/token", server.trim_end_matches('/'));
    let resp = client
        .post(&url)
        .json(&serde_json::json!({ "name": name, "room": room }))
        .send()
        .await
        .with_context(|| format!("cannot reach {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!("token request failed ({status}): {body}");
    }
    Ok(resp.json().await?)
}

impl Session {
    pub async fn join(
        app: AppHandle,
        token: TokenResponse,
        voice: &Voice,
        audio_prefs: &AudioPrefs,
    ) -> Result<Self> {
        // Must exist before the first track is created so WebRTC captures
        // and plays out through the real devices (with AEC, NS and AGC).
        let audio = PlatformAudio::new().context("no audio devices available")?;
        audio_prefs.apply(&audio);

        let mut options = RoomOptions::default();
        options.auto_subscribe = true;
        options.dynacast = true;
        let (room, events) = Room::connect(&token.url, &token.token, options)
            .await
            .with_context(|| format!("cannot connect to LiveKit at {}", token.url))?;
        let room = Arc::new(room);
        tracing::info!(identity = %token.identity, room = %room.name(), "joined");

        let mic = LocalAudioTrack::create_audio_track("microphone", audio.rtc_source());
        // Attach before publishing so push-to-talk starts muted.
        voice.set_mic(Some(mic.clone()));
        // A member muted by a moderator may not publish; they still listen.
        if let Err(e) = publish_mic(&room, &mic).await {
            tracing::warn!("microphone not published: {e:#}");
        }
        voice.resync();

        let muted_speech = {
            let (room, app, hint_app) = (room.clone(), app.clone(), app.clone());
            MutedSpeechMonitor::start(
                audio_prefs.input_name(&audio),
                move || mic_published(&room) && app.state::<AppState>().voice.transmitting(),
                move || {
                    let _ = hint_app.emit("muted-speech", ());
                },
            )
        };

        let link_stats = Arc::new(Mutex::new(HashMap::new()));
        let viewers = Viewers::default();
        let tasks = vec![
            tokio::spawn(event_loop(
                app.clone(),
                room.clone(),
                mic.clone(),
                viewers.clone(),
                events,
            )),
            tokio::spawn(snapshot_loop(app.clone(), room.clone(), mic, link_stats)),
        ];

        Ok(Self {
            app,
            viewers,
            room,
            identity: token.identity,
            livekit_url: token.url,
            share: None,
            tasks,
            audio,
            _muted_speech: muted_speech,
        })
    }

    pub fn identity(&self) -> &str {
        &self.identity
    }

    pub fn livekit_url(&self) -> &str {
        &self.livekit_url
    }

    pub fn audio(&self) -> &PlatformAudio {
        &self.audio
    }

    pub fn set_local_mute(&self, identity: &str, muted: bool) {
        apply_local_mute(&self.room, identity, muted);
    }

    /// Opens (or brings to front) the viewer for `identity`'s screen share.
    pub fn watch(&self, identity: &str) -> Result<()> {
        let participant = self
            .room
            .remote_participants()
            .into_values()
            .find(|p| p.identity().as_str() == identity)
            .ok_or_else(|| anyhow::anyhow!("{identity} is not in this room"))?;
        let (sid, track) = participant
            .track_publications()
            .into_values()
            .find_map(|p| match (p.source(), p.track()) {
                (TrackSource::Screenshare, Some(RemoteTrack::Video(track))) => {
                    Some((p.sid(), track))
                }
                _ => None,
            })
            .ok_or_else(|| anyhow::anyhow!("{identity} is not sharing a screen"))?;
        let mut viewers = self.viewers.lock().unwrap();
        if let Some(viewer) = viewers.get(&sid)
            && viewer.is_open()
        {
            viewer.focus();
            return Ok(());
        }
        let title = format!("{} - screen", participant.name());
        viewers.insert(sid, Viewer::open(&self.app, &title, track)?);
        Ok(())
    }

    pub async fn start_share(&mut self, source_id: Option<u64>) -> Result<ShareInfo> {
        let share = match self.share.take() {
            Some(share) => share,
            None => ScreenShare::start(&self.app, &self.room, source_id).await?,
        };
        let info = share.info().clone();
        self.share = Some(share);
        Ok(info)
    }

    pub async fn stop_share(&mut self) {
        if let Some(share) = self.share.take() {
            share.stop(&self.room).await;
        }
    }

    pub async fn leave(mut self, voice: &Voice) {
        self.stop_share().await;
        voice.set_mic(None);
        for task in &self.tasks {
            // Dropping the event loop closes all viewer windows.
            task.abort();
        }
        if let Err(e) = self.room.close().await {
            tracing::warn!("error while leaving: {e}");
        }
        tracing::info!("left room");
    }
}

async fn publish_mic(room: &Room, mic: &LocalAudioTrack) -> Result<()> {
    room.local_participant()
        .publish_track(
            LocalTrack::Audio(mic.clone()),
            TrackPublishOptions {
                source: TrackSource::Microphone,
                dtx: true,
                red: true,
                ..Default::default()
            },
        )
        .await
        .context("cannot publish microphone")?;
    Ok(())
}

fn mic_published(room: &Room) -> bool {
    room.local_participant()
        .track_publications()
        .values()
        .any(|p| p.source() == TrackSource::Microphone)
}

/// Opens a viewer window for every subscribed screen share, and publishes
/// the microphone again when a moderator lifts a server mute.
async fn event_loop(
    app: AppHandle,
    room: Arc<Room>,
    mic: LocalAudioTrack,
    viewers: Viewers,
    mut events: mpsc::UnboundedReceiver<RoomEvent>,
) {
    // Dropping the viewers (on leave) closes their windows.
    let _close_on_exit = CloseViewers(viewers.clone());
    while let Some(event) = events.recv().await {
        match event {
            RoomEvent::TrackSubscribed {
                track: RemoteTrack::Audio(track),
                participant,
                ..
            } => {
                let muted = app
                    .state::<AppState>()
                    .locally_muted
                    .lock()
                    .unwrap()
                    .contains(participant.identity().as_str());
                if muted {
                    track.rtc_track().set_enabled(false);
                }
            }
            RoomEvent::TrackSubscribed {
                track: RemoteTrack::Video(track),
                publication,
                participant,
            } if publication.source() == TrackSource::Screenshare => {
                let title = format!("{} - screen", participant.name());
                match Viewer::open(&app, &title, track) {
                    Ok(viewer) => {
                        viewers.lock().unwrap().insert(publication.sid(), viewer);
                    }
                    Err(e) => tracing::error!("cannot open screen viewer: {e:#}"),
                }
            }
            RoomEvent::TrackUnsubscribed { publication, .. } => {
                viewers.lock().unwrap().remove(&publication.sid());
            }
            RoomEvent::ParticipantPermissionChanged {
                participant: Participant::Local(_),
                permission: Some(permission),
            } => {
                // livekit.TrackSource.MICROPHONE in the protocol.
                const MICROPHONE: i32 = 2;
                let allowed = permission.can_publish
                    && (permission.can_publish_sources.is_empty()
                        || permission.can_publish_sources.contains(&MICROPHONE));
                tracing::info!(allowed, "microphone permission changed");
                if allowed && !mic_published(&room) {
                    match publish_mic(&room, &mic).await {
                        Ok(()) => app.state::<AppState>().voice.resync(),
                        Err(e) => tracing::warn!("{e:#}"),
                    }
                }
            }
            RoomEvent::Disconnected { reason } => {
                tracing::warn!(?reason, "disconnected");
                let _ = app.emit("disconnected", format!("{reason:?}"));
                // Release the session so the UI can join again. leave() aborts
                // this task, so it has to run on its own.
                let app = app.clone();
                tokio::spawn(async move {
                    let state = app.state::<AppState>();
                    let session = state.session.lock().await.take();
                    if let Some(session) = session {
                        session.leave(&state.voice).await;
                    }
                });
                break;
            }
            _ => {}
        }
    }
}

struct CloseViewers(Viewers);

impl Drop for CloseViewers {
    fn drop(&mut self) {
        self.0.lock().unwrap().clear();
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ParticipantView {
    identity: String,
    name: String,
    is_local: bool,
    speaking: bool,
    /// Local participant only: the microphone is open (key held or voice activation).
    transmitting: bool,
    mic_muted: bool,
    /// Remote participants only: muted for this client.
    locally_muted: bool,
    sharing_screen: bool,
    stats: Option<LinkStats>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RoomSnapshot {
    room: String,
    participants: Vec<ParticipantView>,
}

/// Pushes the participant list to the UI and refreshes stats periodically.
async fn snapshot_loop(
    app: AppHandle,
    room: Arc<Room>,
    mic: LocalAudioTrack,
    link_stats: Arc<Mutex<HashMap<String, LinkStats>>>,
) {
    let mut tick = tokio::time::interval(SNAPSHOT_INTERVAL);
    let mut n = 0u32;
    loop {
        tick.tick().await;
        if n.is_multiple_of(STATS_EVERY) {
            // Stats queries can take a few ms each; never block the snapshot.
            tokio::spawn(collect_stats(room.clone(), mic.clone(), link_stats.clone()));
        }
        n = n.wrapping_add(1);

        let stats = link_stats.lock().await.clone();
        let local = room.local_participant();
        let local_identity = local.identity().to_string();
        let mut participants = vec![ParticipantView {
            name: local.name(),
            speaking: local.is_speaking(),
            // Without a publication a moderator muted us on the server.
            transmitting: mic_published(&room) && app.state::<AppState>().voice.transmitting(),
            mic_muted: mic.is_muted() || !mic_published(&room),
            locally_muted: false,
            sharing_screen: local
                .track_publications()
                .values()
                .any(|p| p.source() == TrackSource::Screenshare),
            stats: stats.get(&local_identity).cloned(),
            identity: local_identity,
            is_local: true,
        }];
        let locally_muted = app
            .state::<AppState>()
            .locally_muted
            .lock()
            .unwrap()
            .clone();
        let mut remotes: Vec<_> = room.remote_participants().into_values().collect();
        remotes.sort_by_key(|p| p.identity().to_string());
        for p in remotes {
            let pubs = p.track_publications();
            let identity = p.identity().to_string();
            participants.push(ParticipantView {
                name: p.name(),
                speaking: p.is_speaking(),
                transmitting: false,
                mic_muted: pubs
                    .values()
                    .find(|p| p.source() == TrackSource::Microphone)
                    .is_none_or(|p| p.is_muted()),
                locally_muted: locally_muted.contains(&identity),
                sharing_screen: pubs
                    .values()
                    .any(|p| p.source() == TrackSource::Screenshare),
                stats: stats.get(&identity).cloned(),
                identity,
                is_local: false,
            });
        }
        let snapshot = RoomSnapshot {
            room: room.name(),
            participants,
        };
        if app.emit("room-state", &snapshot).is_err() {
            break;
        }
    }
}

async fn collect_stats(
    room: Arc<Room>,
    mic: LocalAudioTrack,
    out: Arc<Mutex<HashMap<String, LinkStats>>>,
) {
    let mut fresh = HashMap::new();
    if let Ok(s) = mic.get_stats().await {
        fresh.insert(
            room.local_participant().identity().to_string(),
            stats::outbound(&s),
        );
    }
    for p in room.remote_participants().into_values() {
        let (mut audio, mut video) = (None, None);
        for publication in p.track_publications().into_values() {
            let Some(track) = publication.track() else {
                continue;
            };
            let Ok(s) = track.get_stats().await else {
                continue;
            };
            match publication.source() {
                TrackSource::Microphone => audio = Some(stats::inbound(&s)),
                TrackSource::Screenshare => video = Some(stats::inbound(&s)),
                _ => {}
            }
        }
        // Audio provides jitter and loss, the screen share adds video numbers.
        let merged = match (audio, video) {
            (Some(a), Some(v)) => Some(a.with_video(&v)),
            (a, v) => a.or(v),
        };
        if let Some(m) = merged {
            fresh.insert(p.identity().to_string(), m);
        }
    }
    let mut out = out.lock().await;
    let fresh: HashMap<_, _> = fresh
        .into_iter()
        .map(|(id, s)| {
            let s = s.since(out.get(&id));
            (id, s)
        })
        .collect();
    tracing::trace!(stats = ?fresh, "link stats");
    *out = fresh;
}

#[cfg(test)]
mod tests {
    use axum::Json;
    use axum::http::StatusCode;
    use axum::routing::post;
    use serde_json::{Value, json};

    use super::fetch_token;

    /// Serves `router` on a random local port and returns its base URL.
    async fn serve(router: axum::Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn fetches_token_from_dev_endpoint() {
        let router = axum::Router::new().route(
            "/api/voice/token",
            post(|Json(req): Json<Value>| async move {
                Json(json!({
                    "url": "ws://localhost:7880",
                    "token": format!("token-for-{}-in-{}", req["name"].as_str().unwrap(), req["room"].as_str().unwrap()),
                    "identity": "bob@localhost",
                    "room": "lobby",
                }))
            }),
        );
        let base = serve(router).await;

        // A trailing slash on the server URL must not break the path.
        let token = fetch_token(&format!("{base}/"), "bob", "lobby")
            .await
            .unwrap();
        assert_eq!(token.url, "ws://localhost:7880");
        assert_eq!(token.token, "token-for-bob-in-lobby");
        assert_eq!(token.identity, "bob@localhost");
    }

    #[tokio::test]
    async fn reports_server_errors() {
        let router = axum::Router::new().route(
            "/api/voice/token",
            post(|| async {
                (
                    StatusCode::FORBIDDEN,
                    "authentication is not implemented yet",
                )
            }),
        );
        let base = serve(router).await;

        let err = fetch_token(&base, "bob", "lobby").await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("403"), "{msg}");
        assert!(
            msg.contains("authentication is not implemented yet"),
            "{msg}"
        );
    }

    #[tokio::test]
    async fn reports_unreachable_server() {
        // Port 9 (discard) is not served on the test machine.
        let err = fetch_token("http://127.0.0.1:9", "bob", "lobby")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("cannot reach"), "{err}");
    }
}
