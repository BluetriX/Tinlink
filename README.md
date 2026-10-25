# Tinlink

Self-hosted voice, chat, screen sharing and file sharing for small communities.
A Discord / TeamSpeak replacement with a focus on **low latency**.

> Status: early development. Nothing is usable yet.

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
| Desktop client (Windows first, then macOS) | Tauri 2, media in the Rust core via the LiveKit Rust SDK, UI in Leptos |
| Web / mobile client (later) | Same Leptos UI in the browser |
| Reverse proxy, TLS | Caddy |

The server decides who may do what and encodes it into a signed LiveKit token.
Media flows directly between clients and LiveKit, never through the API server.

No npm: dependencies are managed with Cargo and checked by `cargo deny`.
Any JavaScript that is unavoidable is vendored as a single pinned file with a checksum.

## Repository layout

```
apps/desktop      desktop client (Tauri 2), media in the Rust core via the LiveKit SDK
crates/hwenc      hardware H.264 encoding via Media Foundation (Windows)
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

For a public server set `use_external_ip: true` in `deploy/livekit.yaml`,
otherwise LiveKit only announces local addresses and remote clients cannot connect media.

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
| GET, POST | `/groups` | My groups, create a group |
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
| POST | `/channels/{id}/voice-token` | LiveKit token for the channel's voice room |
| POST | `/voice/token` | Unauthenticated dev token (only with `TINLINK_DEV_INSECURE_TOKENS=true`) |

Live updates come over a WebSocket at `/api/gateway`. The client first sends
`{"type":"auth","token":"..."}` and receives `{"type":"ready"}`, then events such as
`message_created`, `channel_updated`, `member_joined` or `group_updated` (refetch
channels and permissions). Events are filtered by the receiver's permissions. On
`{"type":"resync"}` the client fell behind and should reload its state.

LiveKit reports joins and leaves via webhooks (`/api/livekit/webhook`, signed with
the API secret). The channel list includes who is in which voice channel, and
gateway events `voice_joined` / `voice_left` keep clients current. Temporary
channels are deleted when their room finishes, or after 10 minutes if nobody joined.

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

## Desktop client

The prototype in `apps/desktop` joins a room, publishes the microphone with
push-to-talk or voice activation, shows participants with speaking indicator
and link statistics (RTT to the SFU, jitter, loss, jitter buffer delay), shares
a screen and opens a native window for every remote screen share.

All media runs natively in the Rust core through the LiveKit SDK (libwebrtc),
never in the WebView:

- Audio: WebRTC's audio device module (WASAPI on Windows) with its echo
  cancellation, noise suppression and gain control; Opus with DTX and RED.
- Screen sharing: libwebrtc's desktop capturer (Windows Graphics Capture, DXGI
  fallback), H.264. The SDK has no hardware encoder on Windows (its NVENC support
  is Linux only), so `crates/hwenc` drives the GPU's Media Foundation encoder
  (NVENC, AMF, Quick Sync) and feeds pre-encoded frames into the SDK: up to
  1440p at 60 fps. Without a hardware encoder, WebRTC encodes in software
  (OpenH264) at up to 1080p, 30 fps. `crates/hwenc` is the only crate that
  uses `unsafe` (COM calls).
- Viewing: decoded frames are scaled with libyuv and drawn with softbuffer;
  only the newest frame is rendered, and the receiver uses zero playout delay.

The UI is a static page in `apps/desktop/ui` (no npm, no bundler).

Requirements on Windows: Rust (stable, MSVC), Visual Studio Build Tools with the
C++ workload, WebView2 (preinstalled on Windows 11). The first build downloads a
prebuilt libwebrtc. `.cargo/config.toml` links the static C runtime, which
libwebrtc requires.

```sh
cargo run -p tinlink-desktop
# Second instance for testing, joining automatically:
cargo run -p tinlink-desktop -- --name bob --room lobby --voice-activation --join
```

Options: `--server URL` (default `https://localhost`), `--name`, `--room`,
`--voice-activation`, `--join`, `--share` (share the first screen after joining). Debug builds accept the self-signed
certificate of the local stack; release builds do not.

Push-to-talk uses a global hotkey (default `F8`, changeable in the UI, e.g.
`Ctrl+Shift+Space`). A key can only be registered by one program at a time, so a
second instance on the same PC should use voice activation or another key.
Use headphones when testing two instances on one PC, otherwise each instance
plays the other's microphone back into the shared microphone.

## Roadmap

1. Media prototype: Windows desktop client joins a room, push-to-talk, screen sharing; measure latency
2. Core server: accounts, groups, channels, chat, live events over WebSocket (done)
3. Permissions: roles, overwrites, user-created channels with limits (done), moderation in voice
4. Files and polish: uploads, previews, mentions, web client
5. Optional: end-to-end encryption, mobile, federation

## License

Tinlink is licensed under the [GNU Affero General Public License v3.0](LICENSE)
(`AGPL-3.0-only`). If you run a modified version as a service, you must publish
your changes.

The protocol crate (`crates/protocol`) is dual-licensed under
[MIT](crates/protocol/LICENSE-MIT) or [Apache-2.0](crates/protocol/LICENSE-APACHE),
so third-party clients can use it freely.

All dependencies use permissive licenses, except a few MPL-2.0 crates that the
desktop app inherits from Tauri (listed in `deny.toml`); this is enforced in CI by `cargo deny`.
