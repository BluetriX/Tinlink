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
use crate::screen::ScreenShare;
use crate::stats::{self, LinkStats};
use crate::viewer::Viewer;
use crate::voice::Voice;

/// How often the participant list is pushed to the UI.
const SNAPSHOT_INTERVAL: Duration = Duration::from_millis(200);
/// Stats are collected on every n-th snapshot.
const STATS_EVERY: u32 = 5;

pub struct Session {
    room: Arc<Room>,
    identity: String,
    livekit_url: String,
    share: Option<ScreenShare>,
    tasks: Vec<JoinHandle<()>>,
    /// Keeps the platform audio device module (WASAPI capture and playout) alive.
    _audio: PlatformAudio,
}

#[derive(Deserialize)]
struct TokenResponse {
    url: String,
    token: String,
    identity: String,
}

/// Fetches a LiveKit token from the Tinlink server's development endpoint.
async fn fetch_token(server: &str, name: &str, room: &str) -> Result<TokenResponse> {
    let client = reqwest::Client::builder()
        // Caddy serves https://localhost with its own local CA during development.
        .danger_accept_invalid_certs(cfg!(debug_assertions))
        .timeout(Duration::from_secs(10))
        .build()?;
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
        server: &str,
        name: &str,
        room_name: &str,
        voice: &Voice,
    ) -> Result<Self> {
        let token = fetch_token(server, name, room_name).await?;

        // Must exist before the first track is created so WebRTC captures
        // and plays out through the real devices (with AEC, NS and AGC).
        let audio = PlatformAudio::new().context("no audio devices available")?;

        let mut options = RoomOptions::default();
        options.auto_subscribe = true;
        options.dynacast = true;
        let (room, events) = Room::connect(&token.url, &token.token, options)
            .await
            .with_context(|| format!("cannot connect to LiveKit at {}", token.url))?;
        let room = Arc::new(room);
        tracing::info!(identity = %token.identity, room = %room_name, "joined");

        let mic = LocalAudioTrack::create_audio_track("microphone", audio.rtc_source());
        // Attach before publishing so push-to-talk starts muted.
        voice.set_mic(Some(mic.clone()));
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

        let link_stats = Arc::new(Mutex::new(HashMap::new()));
        let tasks = vec![
            tokio::spawn(event_loop(app.clone(), events)),
            tokio::spawn(snapshot_loop(app, room.clone(), mic, link_stats)),
        ];

        Ok(Self {
            room,
            identity: token.identity,
            livekit_url: token.url,
            share: None,
            tasks,
            _audio: audio,
        })
    }

    pub fn identity(&self) -> &str {
        &self.identity
    }

    pub fn livekit_url(&self) -> &str {
        &self.livekit_url
    }

    pub async fn start_share(&mut self, source_id: Option<u64>) -> Result<()> {
        if self.share.is_none() {
            self.share = Some(ScreenShare::start(&self.room, source_id).await?);
        }
        Ok(())
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

/// Opens a viewer window for every subscribed screen share.
async fn event_loop(app: AppHandle, mut events: mpsc::UnboundedReceiver<RoomEvent>) {
    let mut viewers: HashMap<TrackSid, Viewer> = HashMap::new();
    while let Some(event) = events.recv().await {
        match event {
            RoomEvent::TrackSubscribed {
                track: RemoteTrack::Video(track),
                publication,
                participant,
            } if publication.source() == TrackSource::Screenshare => {
                let title = format!("{} - screen", participant.name());
                match Viewer::open(&app, &title, track) {
                    Ok(viewer) => {
                        viewers.insert(publication.sid(), viewer);
                    }
                    Err(e) => tracing::error!("cannot open screen viewer: {e:#}"),
                }
            }
            RoomEvent::TrackUnsubscribed { publication, .. } => {
                viewers.remove(&publication.sid());
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
            transmitting: app.state::<AppState>().voice.transmitting(),
            mic_muted: mic.is_muted(),
            sharing_screen: local
                .track_publications()
                .values()
                .any(|p| p.source() == TrackSource::Screenshare),
            stats: stats.get(&local_identity).cloned(),
            identity: local_identity,
            is_local: true,
        }];
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
