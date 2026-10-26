# Tinlink

Self-hosted voice, chat, screen sharing and file sharing for small communities.
A Discord / TeamSpeak replacement with a focus on **low latency**.

> Status: early development, ready for first tests. The server and the Windows
> desktop client work end to end: accounts, groups, chat, files, voice, screen
> sharing and moderation. Expect rough edges and breaking changes.

## Current state

- **Server:** accounts, groups with roles and per-channel permissions, invites,
  text chat with file uploads, live updates over WebSocket, voice presence,
  user-created temporary rooms and voice moderation (server mute, disconnect,
  move). Self-hosted with Docker Compose, see [docs/self-hosting.md](docs/self-hosting.md).
- **Windows client:** a native app (one portable `.exe`) with chat, voice
  (push-to-talk or voice activation) and screen sharing with GPU encoding.
- **Web client:** served by the server itself at `https://<your domain>/`, for
  Linux, macOS and anyone without the Windows app: chat, voice and screen sharing
  in the browser.
- **Latency** (measured on a local network): about 60–70 ms glass to glass for
  screen sharing at 1440p60, 2.7 ms of it for hardware encoding on an RTX 3070;
  audio roughly 60–90 ms mouth to ear.
- **Not yet:** native macOS and mobile clients, federation, end-to-end encryption,
  installer and code signing, message search.

## Quick start

1. Run a server: follow [docs/self-hosting.md](docs/self-hosting.md) (a Linux host
   with Docker, a DNS name, a few forwarded ports).
2. Build the Windows client (requirements under [Desktop client](#desktop-client)):
   `cargo build --release -p tinlink-desktop`. The result,
   `target/release/tinlink-desktop.exe`, runs without installation and can be
   passed on to others. It needs only Windows 10/11 (64 bit) with WebView2, which
   is preinstalled; "N" editions also need the Media Feature Pack. It is not
   signed yet, so Windows SmartScreen asks once ("More info" → "Run anyway").
3. Start it, enter your server's domain and create an account. The first account
   creates a group and invites others with the "Invite" button. Without the
   Windows app, open `https://<your domain>/` in a browser instead.

## Goals

- Groups (like Discord servers) with channels, roles and per-channel permission overwrites
- Every channel has a text chat; voice and screen sharing can be enabled per channel
- Members may create their own channels within limits set by admins (optionally temporary, like TS3)
- File sharing in channel chats
- Low latency audio (Opus, 10 ms frames) and screen sharing (hardware encoding, simulcast)
- Self-hosting with a single `docker compose` file
- Later: federation between servers (user ids are `name@domain` from day one) and end-to-end encryption

## Architecture

| Component | Technology |
| --- | --- |
| Server (REST, WebSocket gateway, permissions) | Rust, axum, PostgreSQL |
| Media (voice, screen sharing) | [LiveKit](https://livekit.io) SFU, WebRTC over UDP |
| Desktop client (Windows first, then macOS) | Tauri 2, media in the Rust core via the LiveKit Rust SDK, static HTML/JS UI |
| Web client | static HTML/JS served by the server, vendored livekit-client |
| Mobile client (later) | to be decided |
| Reverse proxy, TLS | Caddy |

The server decides who may do what and encodes it into a signed LiveKit token.
Media flows directly between clients and LiveKit, never through the API server.

No npm: dependencies are managed with Cargo and checked by `cargo deny`.
Any JavaScript that is unavoidable is vendored as a single pinned file with a checksum.

## Repository layout

```
apps/desktop      desktop client (Tauri 2), media in the Rust core via the LiveKit SDK
apps/web          web client (static files, no build step)
crates/hwenc      hardware H.264 encoding via Media Foundation (Windows)
crates/winput     listen-only keyboard and mouse state for push-to-talk (Windows)
crates/protocol   shared types: federated user ids, permission model
crates/server     API server and LiveKit token issuer
deploy/           docker compose stack (Caddy, server, PostgreSQL, LiveKit)
```

## Development

Requirements: Rust (stable), Docker.

```sh
cargo test        # integration tests need DATABASE_URL, see below
cargo clippy --all-targets
```

The server's integration tests run against a real PostgreSQL and create a fresh
database per test, so the user needs the `CREATEDB` privilege:

```sh
docker run -d --name tinlink-pg -p 5432:5432 -e POSTGRES_USER=tinlink \
  -e POSTGRES_PASSWORD=tinlink postgres:17
DATABASE_URL=postgres://tinlink:tinlink@localhost/tinlink cargo test
```

These cover the server crates (the default workspace members). The desktop app
is checked with `-p tinlink-desktop`, see below.

Run the full stack locally:

```sh
cd deploy
cp .env.example .env   # set secrets, TINLINK_DEV_INSECURE_TOKENS=true for testing
docker compose up -d --build
curl -k https://localhost/health
```

To run a server that others can reach over the internet (domain, certificates,
port forwarding), follow [docs/self-hosting.md](docs/self-hosting.md).

When the stack runs in Docker inside WSL2 and the client on Windows, LiveKit would
announce its container address, which Windows cannot reach. Set
`LIVEKIT_NODE_IP` in `deploy/.env` to the WSL address (`hostname -I | cut -d' ' -f1`
inside WSL; it changes when WSL restarts) and run `docker compose up -d` again.

Request a development voice token (only with `TINLINK_DEV_INSECURE_TOKENS=true`):

```sh
curl -k -X POST https://localhost/api/voice/token \
  -H 'content-type: application/json' \
  -d '{"name":"erik","room":"lobby"}'
```

## Server API

All endpoints live under `/api` and use JSON. Authenticated endpoints expect
`Authorization: Bearer <token>` from register or login. Permissions are
serialized as flag names, e.g. `"VIEW_CHANNEL | SEND_MESSAGES"`.

| Method | Path | Purpose |
| --- | --- | --- |
| POST | `/auth/register`, `/auth/login`, `/auth/logout` | Accounts and sessions |
| GET | `/users/me` | Current user |
| PUT | `/users/me/password` | Change password `{"current_password","new_password"}`, signs out other sessions |
| GET, POST | `/groups` | My groups (with `unread` and `mention_count`), create a group |
| GET, PATCH, DELETE | `/groups/{id}` | Group details (incl. my permissions), settings, delete (owner) |
| GET | `/groups/{id}/members` | Members with role ids |
| DELETE | `/groups/{id}/members/{user}` | Leave or kick |
| PUT, DELETE | `/groups/{id}/members/{user}/roles/{role}` | Assign or remove a role |
| POST | `/groups/{id}/invites`, `/invites/{code}/join` | Invites |
| GET, POST | `/groups/{id}/roles` | Roles |
| PATCH, DELETE | `/groups/{id}/roles/{role}` | Edit or delete a role |
| POST | `/groups/{id}/categories` | Create a category |
| GET, POST | `/groups/{id}/channels` | Visible channels with my permissions, create a channel |
| PATCH, DELETE | `/channels/{id}` | Edit or delete a channel |
| GET | `/channels/{id}/overwrites` | Permission overwrites |
| PUT, DELETE | `/channels/{id}/overwrites/{role\|member}/{target}` | Set or remove an overwrite |
| GET, POST | `/channels/{id}/messages` | Chat history (`?before=&limit=`), send |
| PATCH, DELETE | `/channels/{id}/messages/{msg}` | Edit (author), delete |
| PUT | `/channels/{id}/read` | Mark read up to `{"message_id"}` (never moves back) |
| POST | `/channels/{id}/attachments` | Upload a file (multipart field `file`), then send its id in `attachment_ids` |
| GET | `/files/{id}?exp=&sig=` | Download via the signed URL included in messages |
| GET | `/info` | Server version and source code URL |
| POST | `/channels/{id}/voice-token` | LiveKit token for the channel's voice room |
| POST | `/voice/token` | Unauthenticated dev token (only with `TINLINK_DEV_INSECURE_TOKENS=true`) |
| PUT | `/groups/{id}/members/{user}/voice-mute` | Server-side mute `{"muted":true}` (MUTE_MEMBERS) |
| POST | `/channels/{id}/voice/{user}/disconnect` | Disconnect from voice (MOVE_MEMBERS, or yourself) |
| POST | `/channels/{id}/voice/{user}/move` | Move to another voice channel `{"to":"..."}` (MOVE_MEMBERS) |

Live updates come over a WebSocket at `/api/gateway`. The client first sends
`{"type":"auth","token":"..."}` and receives `{"type":"ready"}`, then events such as
`message_created`, `channel_updated`, `member_joined` or `group_updated` (refetch
channels and permissions). Events are filtered by the receiver's permissions. On
`{"type":"resync"}` the client fell behind and should reload its state.

LiveKit reports joins and leaves via webhooks (`/api/livekit/webhook`, signed with
the API secret). The channel list includes who is in which voice channel, and
gateway events `voice_joined` / `voice_left` keep clients current. Temporary
channels are deleted when their room finishes, or after 10 minutes if nobody joined.

Moderation uses the LiveKit server API at `LIVEKIT_INTERNAL_URL` (in compose
`http://livekit:7880`). Moderators can only act on members with a lower role. A
server mute removes the microphone from the member's grant immediately and from
future tokens. After role, overwrite or mute changes the server updates the grants
of connected participants and disconnects anyone who lost access. A move
disconnects the member and sends `voice_moved` (`{"from","to"}`), so the client
joins the target room.

Uploaded files are stored in `TINLINK_FILES_DIR` (default `/data/files`, limit
`TINLINK_MAX_UPLOAD_MB`, default 25). Messages carry signed download URLs valid for
about a day; images and a few media types are shown inline, everything else is
served as a download. Uploads never attached to a message are removed after an hour.

Messages carry `mentions`: the ids of group members mentioned with `@name` who can
see the channel (recomputed on edit). The channel list includes the caller's read
state per channel: `last_message_id`, `last_read_id` and `mention_count` (unread
mentions). A channel is unread when `last_message_id` is greater than
`last_read_id`; your own messages count as read.

Rules worth knowing:

- The group owner has all permissions. Each group has an `@everyone` role whose id
  equals the group id.
- Roles have a position; members can only manage roles below their highest role and
  only grant permissions they hold.
- Members with `CREATE_OWN_CHANNELS` can create channels in categories with
  `allow_user_channels` (a new group has a "Rooms" category), up to
  `user_channel_limit` per member, and get moderation rights in their channels.
- Registration is open by default; set `TINLINK_OPEN_REGISTRATION=false` to allow
  only the first account.

## Web client

`apps/web` is a browser client without a build step: plain HTML, CSS and ES
modules plus one vendored, checksummed copy of
[livekit-client](apps/web/vendor/README.md). The server serves it from
`TINLINK_WEB_DIR` (default `/app/web`, which the Docker image contains) with a
strict Content Security Policy, so it is reachable at `https://<your domain>/`
and talks to the API on the same origin.

It covers what the desktop client does (chat with unread markers, @mentions
with notifications, edit and delete, uploads, voice with push-to-talk or voice
activation and an adjustable threshold, per-person volume, screen sharing with
selectable resolution and frame rate, moderation, password change), with two
browser limits: push-to-talk only works while the tab has focus, and screen
sharing uses the browser's encoder. `LIVEKIT_PUBLIC_URL` must be a `wss://` URL,
since an HTTPS page may not open plain WebSocket connections.

Tests of the pure logic run with `node --test apps/web/tests/*.test.js`.

## Desktop client

`apps/desktop` is a client for the server above: log in or create an account,
create groups or join one with an invite code, browse the channel tree,
chat with history, live updates and file attachments (images inline), edit
and delete messages (moderators can delete any), @-mention members with
autocomplete (mentions of you are highlighted), see unread channels in bold
with mention badges on channels and groups, get a Windows notification and a
flashing taskbar button for a mention while the window is in the background
or another channel is open (shown as "Windows PowerShell", because the
portable exe has no registered app id), change your password, see
who is in which voice channel, join a channel's voice, and create your own
(temporary) rooms in the "Rooms" category. Moderators click a member in a
voice channel to server-mute, disconnect or move them; a moved client follows
into the new channel by itself. In voice: push-to-talk or voice
activation, microphone and output selection, speaking indicators, link statistics (RTT to the SFU, jitter,
loss, jitter buffer delay), screen sharing, and a native window for every
remote screen share (closed viewers reopen with "Watch"; double-click or F11
toggles fullscreen, Escape leaves it). "Mute" next to a participant silences
them for you only. After a lost connection (or a restart of the app) the
client joins its last voice channel again by itself; leaving with "Leave" or
being removed by a moderator ends that. The LiveKit SDK's audio engine can
hang after a failed connection attempt, so the client checks that the voice
server answers before connecting and, should the engine still stop
responding, restarts itself and rejoins. A volume slider per person needs a change in the LiveKit
SDK's C++ layer and is not possible yet. While sharing, the voice panel shows resolution, frame
rate and encoder, and a blue frame marks the shared monitor (excluded from the
capture, so viewers do not see it). If you talk for about 10 seconds while
nothing is transmitted (push-to-talk not held, or server-muted), the app plays
a quiet double beep and shows a hint, at most every 30 seconds; for this it
only measures the microphone level locally.

The login is kept in the app data directory; all HTTP and the gateway
WebSocket run in the Rust core, the WebView never sees the token.

All media runs natively in the Rust core through the LiveKit SDK (libwebrtc),
never in the WebView:

- Audio: WebRTC's audio device module (WASAPI on Windows) with its echo
  cancellation, noise suppression and gain control; Opus with DTX and RED.
- Screen sharing: libwebrtc's desktop capturer (Windows Graphics Capture, DXGI
  fallback), H.264. The SDK has no hardware encoder on Windows (its NVENC support
  is Linux only), so `crates/hwenc` drives the GPU's Media Foundation encoder
  (NVENC, AMF, Quick Sync) and feeds pre-encoded frames into the SDK, up to
  native 4K at 60 fps. Without a hardware encoder, WebRTC encodes in software
  (OpenH264) at up to 1080p, 30 fps. Resolution (native, 1440p, 1080p, 720p;
  only scales down) and frame rate (60, 30, 15; default 1080p, 30) are chosen in
  the voice settings and can be changed while sharing; the bitrate follows
  (0.06 bit per pixel and frame, 1.5 to 20 Mbit/s). `crates/hwenc` uses `unsafe`
  for the COM calls; apart from it only `crates/winput` does (one Win32 call).
- Viewing: decoded frames are scaled with libyuv and drawn with softbuffer;
  only the newest frame is rendered, and the receiver uses zero playout delay.

Latency measurements (one PC, Ryzen 7 5700X3D, RTX 3070, local LiveKit, so the
network part is close to zero; over the internet add the round trip to the server):

| What | Result |
| --- | --- |
| Voice: RTT to the SFU / jitter / receive jitter buffer | 0–1 ms / 1–2 ms / 20–35 ms |
| Voice: estimated mouth to ear (10 ms capture, 20 ms Opus frame, jitter buffer, playout) | 60–90 ms |
| Screen sharing: hardware encode (NVENC via Media Foundation, 720p) | 2.7 ms median |
| Screen sharing: glass to glass, 2560×1440 at 60 fps | about 60–70 ms |
| CPU in a voice call, per client | about 4–5 % of one core |

Glass to glass was measured with a millisecond clock on screen next to its own
image in the viewer window (the nested images give several samples per shot).

The UI is a static page in `apps/desktop/ui` (no npm, no bundler).

Requirements on Windows: Rust (stable, MSVC), Visual Studio Build Tools with the
C++ workload, WebView2 (preinstalled on Windows 11). The first build downloads a
prebuilt libwebrtc. `.cargo/config.toml` links the static C runtime, which
libwebrtc requires.

```sh
cargo run -p tinlink-desktop
# Second instance with its own login and settings:
cargo run -p tinlink-desktop -- --profile bob
```

`--profile NAME` keeps a separate login and WebView storage, e.g. for two
accounts on one PC. Debug builds also offer a "join with a dev token" form
(server needs `TINLINK_DEV_INSECURE_TOKENS=true`); the command line options
`--server URL`, `--name`, `--room`, `--voice-activation`, `--join` and `--share`
use that flow for quick media tests, e.g.
`cargo run -p tinlink-desktop -- --name bob --room lobby --voice-activation --join`.
With `TINLINK_DEVTOOLS_PORT=9222` a debug build opens the WebView's DevTools
protocol on that port for UI automation. Debug builds accept the self-signed
certificate of the local stack; release builds do not.

Voice activation transmits while the microphone level is above an adjustable
threshold (slider with a live level meter in the voice settings) and keeps
going for half a second after it drops, so word endings are not cut off.

Push-to-talk (default `F8`) only listens: the key still reaches the game or
whatever program has focus. Any key, `Mouse3`/`Mouse4`/`Mouse5` (middle and side
buttons) or a combination like `Ctrl+Shift+Space` works; "Record" in the voice
settings takes the next key or button you press. Key state is read by
`crates/winput`, which (like `crates/hwenc`) confines its one `unsafe` Win32 call.
Use headphones when testing two instances on one PC, otherwise each instance
plays the other's microphone back into the shared microphone.

## Roadmap

1. Media prototype: Windows desktop client joins a room, push-to-talk, screen sharing; measure latency (done)
2. Core server: accounts, groups, channels, chat, live events over WebSocket (done)
3. Permissions: roles, overwrites, user-created channels with limits, voice moderation (done)
4. Files and polish: uploads and image previews (done), Windows client for the server (done), mentions and unread markers (done), web client (done), installer
5. Lower latency: 10 ms Opus frames, a GPU-only capture path without CPU colour conversion
6. More platforms: native macOS, then mobile
7. Optional: end-to-end encryption, federation

## License

Tinlink is licensed under the [GNU Affero General Public License v3.0](LICENSE)
(`AGPL-3.0-only`). If you run a modified version as a service, you must publish
your changes.

The protocol crate (`crates/protocol`) is dual-licensed under
[MIT](crates/protocol/LICENSE-MIT) or [Apache-2.0](crates/protocol/LICENSE-APACHE),
so third-party clients can use it freely.

All dependencies use permissive licenses, except a few MPL-2.0 crates that the
desktop app inherits from Tauri (listed in `deny.toml`); this is enforced in CI by `cargo deny`.
