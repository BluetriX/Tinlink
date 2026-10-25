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

Run the full stack locally:

```sh
cd deploy
cp .env.example .env   # set secrets, TINLINK_DEV_INSECURE_TOKENS=true for testing
docker compose up -d --build
curl -k https://localhost/health
```

For a public server set `use_external_ip: true` in `deploy/livekit.yaml`,
otherwise LiveKit only announces local addresses and remote clients cannot connect media.

Request a development voice token (only with `TINLINK_DEV_INSECURE_TOKENS=true`):

```sh
curl -k -X POST https://localhost/api/voice/token \
  -H 'content-type: application/json' \
  -d '{"name":"erik","room":"lobby"}'
```

## Roadmap

1. Media prototype: Windows desktop client joins a room, push-to-talk, screen sharing; measure latency
2. Core server: accounts, groups, channels, chat over WebSocket
3. Permissions: roles, overwrites, user-created channels with limits, moderation
4. Files and polish: uploads, previews, mentions, web client
5. Optional: end-to-end encryption, mobile, federation
