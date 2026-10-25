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
crates/protocol   shared types: federated user ids, permission model
crates/server     API server and LiveKit token issuer
deploy/           docker compose stack (Caddy, server, PostgreSQL, LiveKit)
```

## Development

Requirements: Rust (stable), Docker.

```sh
cargo test
cargo clippy --all-targets
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
  fallback), H.264 with hardware encoding when available (NVENC, AMF, Quick Sync).
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
`--voice-activation`, `--join`. Debug builds accept the self-signed
certificate of the local stack; release builds do not.

Push-to-talk uses a global hotkey (default `F8`, changeable in the UI, e.g.
`Ctrl+Shift+Space`). A key can only be registered by one program at a time, so a
second instance on the same PC should use voice activation or another key.
Use headphones when testing two instances on one PC, otherwise each instance
plays the other's microphone back into the shared microphone.

## Roadmap

1. Media prototype: Windows desktop client joins a room, push-to-talk, screen sharing; measure latency
2. Core server: accounts, groups, channels, chat over WebSocket
3. Permissions: roles, overwrites, user-created channels with limits, moderation
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
