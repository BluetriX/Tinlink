# Self-hosting Tinlink

This guide sets up a Tinlink server reachable from the internet, for example in
a home lab (Proxmox VM behind a router) or on a rented server. Everything runs
with Docker Compose from the `deploy/` directory.

## What runs

| Container | Purpose | Exposed |
| --- | --- | --- |
| `caddy` | HTTPS for the API, WebSocket gateway and LiveKit signaling, TURN over TLS on 443, automatic Let's Encrypt certificates | TCP 80, TCP/UDP 443 |
| `server` | Tinlink server (accounts, groups, chat, files, permissions) | internal only |
| `postgres` | Database | internal only |
| `livekit` | LiveKit SFU and TURN server for voice and screen sharing | TCP 7881, UDP 50000-50100, UDP 3478 |

Clients talk to one domain: `https://<domain>/api/...` for the server and
`wss://<domain>/rtc` for LiveKit signaling (both through Caddy). Audio and video
normally go directly to LiveKit over UDP, never through Caddy. Only when a
network blocks that (see [Restrictive networks](#restrictive-networks)) they
fall back to TURN over TLS on port 443 of a second name, `turn.<domain>`.

## Requirements

- A Linux host with Docker Engine and the Compose plugin. On Proxmox a Debian 12/13
  VM is the simplest choice. An LXC container works too, but needs
  `nesting=1` (and `keyctl=1` when unprivileged) for Docker.
- 2 vCPU, 2 GB RAM and 10 GB disk are enough for about 100 users. Building the
  server image compiles Rust and needs about 4 GB RAM; give the VM 4 GB for
  the first build or add swap.
- A DNS name that points to your public IP. With a changing home IP use DynDNS,
  e.g. a MyFRITZ address, DuckDNS, or a CNAME on your own domain that points to one.
- A second DNS name for TURN that points to the same IP, e.g. `turn.chat.example.org`
  as a CNAME to `chat.example.org`.
- No other service may use ports 80 and 443 on the public IP. On a FRITZ!Box the
  built-in remote access ("Internetzugriff auf die FRITZ!Box über HTTPS") uses
  443 by default; move it to another port.

## Port forwarding

Forward these ports from the router to the Tinlink host:

| Port | Protocol | Used for |
| --- | --- | --- |
| 80 | TCP | Let's Encrypt HTTP challenge, redirect to HTTPS |
| 443 | TCP | API, gateway, LiveKit signaling |
| 443 | UDP | HTTP/3 (optional) |
| 7881 | TCP | Media fallback when UDP is blocked |
| 50000-50100 | UDP | Voice and screen sharing media |
| 3478 | UDP | TURN over UDP (optional) |

Also open them in the host firewall (and the Proxmox firewall, if enabled).
The UDP range limits the number of concurrent media connections; 101 ports are
plenty for a few dozen people in voice at once.

## Setup

```sh
git clone https://github.com/BluetriX/Tinlink.git
cd Tinlink/deploy
cp .env.example .env
```

Edit `.env`:

```sh
TINLINK_DOMAIN=chat.example.org          # your DNS name
TINLINK_TURN_DOMAIN=turn.chat.example.org # second DNS name, same IP
LIVEKIT_PUBLIC_URL=wss://chat.example.org
LIVEKIT_NODE_IP=                         # leave empty, see below
LIVEKIT_API_KEY=...                      # openssl rand -hex 16
LIVEKIT_API_SECRET=...                   # openssl rand -base64 32
POSTGRES_PASSWORD=...                    # openssl rand -base64 24
TINLINK_DEV_INSECURE_TOKENS=false
TINLINK_OPEN_REGISTRATION=true           # see "Registration" below
```

Edit `livekit.yaml` and set `use_external_ip: true`. LiveKit then finds the
public IP via STUN and announces it to clients.

Start the stack:

```sh
docker compose up -d --build
docker compose logs -f caddy      # wait for "certificate obtained successfully" (twice)
curl https://chat.example.org/health
curl https://chat.example.org/api/info
```

## Connecting clients

The web client is served at `https://chat.example.org/`; it needs nothing but a
current browser (Chrome, Edge, Firefox or Safari). Push-to-talk works there only
while the tab has focus.

In the desktop app enter the server as `chat.example.org` (or
`https://chat.example.org`). The first account can always be registered; it
creates a group and invites others with the "Invite" button.

Release builds of the app only accept valid certificates, so the server must be
reachable under its real DNS name with the Let's Encrypt certificate. Inside your
own network the router has to resolve that name to the host or support NAT
loopback ("hairpin"); a FRITZ!Box does both for forwarded ports.

## Restrictive networks

Company and public networks often block everything except web traffic, so the
direct media ports (UDP 50000-50100, TCP 7881) cannot be reached: login and
chat work, but voice does not connect. For these networks LiveKit runs a TURN
server that clients reach as `turns:<TINLINK_TURN_DOMAIN>:443`, which looks like
any other HTTPS connection. Caddy recognizes those connections by their TLS name,
decrypts them and passes them to LiveKit; everything else on port 443 stays
HTTPS. Clients try the direct ports first and use TURN only when they fail,
because relaying over TCP adds some latency.

To check from a restricted network:

- `http://portquiz.net:7881/` and `http://portquiz.net:50000/` show whether
  outgoing TCP on those ports is open (UDP cannot be tested in a browser).
- `https://livekit.io/connection-test` runs the full check against your server.
  It asks for the LiveKit URL (`wss://<TINLINK_DOMAIN>`) and a token; a
  development token can be made with `lk token create --join --room test
  --identity test --api-key ... --api-secret ...` (LiveKit CLI) on a trusted
  machine. "Connecting via TURN" should succeed even when UDP and TCP fail.

It still fails when the network forces all traffic through a proxy that breaks
TLS open (TLS inspection) or only allows known sites; then only a VPN or a
network exception helps.

The relayed media goes from LiveKit's TURN server to LiveKit's own public IP,
so the router must support NAT loopback (a FRITZ!Box does).

## Registration

With `TINLINK_OPEN_REGISTRATION=true` anyone who can reach the server can
create an account (but sees nothing without an invite). Once everyone has an
account, set it to `false` and run `docker compose up -d` again. The first
account can always be created.

## Changing public IP

LiveKit discovers the public IP when it starts. If your home IP changes while
the stack is running, voice connections fail until LiveKit is restarted:

```sh
docker compose restart livekit
```

A daily restart (cron) or a restart hook in your DynDNS client avoids this.
With a static IP you can instead set `LIVEKIT_NODE_IP` to it.

## Updates and backups

```sh
git pull
docker compose up -d --build
```

Database migrations run automatically on start. Back up the `pg` volume
(e.g. `docker compose exec postgres pg_dump -U tinlink tinlink > backup.sql`)
and the `files` volume with the uploaded files.

## Troubleshooting

- **Login works, voice does not connect:** UDP 50000-50100 or TCP 7881 is not
  forwarded, or `use_external_ip` is still `false`. `docker compose logs livekit`
  shows the announced IPs at startup.
- **Voice works at home, not in a company network:** see
  [Restrictive networks](#restrictive-networks). `TINLINK_TURN_DOMAIN` must
  resolve to your IP, and `docker compose logs caddy` must show a certificate
  for it.
- **Certificate errors:** ports 80/443 do not reach Caddy or the DNS name does
  not point to your public IP. `docker compose logs caddy` shows the ACME error.
- **Voice works for remote users but not inside your network:** the router does
  not support NAT loopback; add a local DNS entry for the domain that points to
  the host.
