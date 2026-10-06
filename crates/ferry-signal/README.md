# ferry-signal

A small, self-hostable WebRTC **signaling** server for Ferry. Browsers and
devices use it to find each other and exchange the messages WebRTC needs to set
up a direct, encrypted data channel (SDP offers/answers and ICE candidates).
Once the channel is up, files travel device to device; the server is no longer
involved.

It speaks LocalSend's `/v1/ws` protocol, so stock LocalSend web clients work
against it unchanged. Ferry clients additionally get rooms (link/QR and
short-code joins), trickle ICE, `CANCEL`, keep-alive `PING`/`PONG` and,
optionally, TURN credentials.

**What it is not:**

- **No file storage or relaying.** File contents never reach the server: it
  only forwards signaling messages, and frames are capped at 64 KiB.
- **No accounts, no database, no disk writes.** Peers, rooms and rate-limit
  state live in memory and disappear with the connection or process.
- **No telemetry.** It makes no outbound connections. Logs contain random
  per-connection ids, counts and a salted, per-process hash of the client's
  network instead of IP addresses. Aliases and versions appear only at
  `RUST_LOG=debug`.

## Run it

```sh
cargo run --release -p ferry-signal            # from the repository root
# or, with the built binary:
ferry-signal                                   # listens on 127.0.0.1:3000
ferry-signal --addr 0.0.0.0:3000               # accept remote clients
ferry-signal --help
```

With no configuration it listens on **loopback only** (`127.0.0.1:3000`),
accepts any browser origin, trusts no proxy and has TURN disabled. That is fine
for local development. For anything else, put it behind a TLS-terminating
reverse proxy (browsers on `https://` pages can only open `wss://` sockets) and
set the variables below.

`ferry-signal healthcheck` exits 0 when the server at the configured address
answers `GET /healthz`. It's meant for container health checks, since the image has no shell or curl.

On `SIGINT`/`SIGTERM` (Ctrl-C on Windows) the server stops accepting
connections, closes every WebSocket with code `1001` (clients reconnect) and
exits after at most a few seconds.

### Configuration

| Flag / variable | Default | Meaning |
|---|---|---|
| `--addr`, `FERRY_SIGNAL_ADDR` | `127.0.0.1:3000` | Listen address. Use `0.0.0.0:3000` or `[::]:3000` to accept remote clients. |
| `FERRY_SIGNAL_ALLOWED_ORIGINS` | any origin | Comma-separated browser origins (`https://ferry.example`) allowed to connect. Requests without `Origin` (native apps) are always allowed. **Set this in production**, or any website can use your server from its visitors' browsers. |
| `FERRY_SIGNAL_TRUSTED_PROXIES` | none | Comma-separated IPs/CIDRs whose `X-Forwarded-For` is honoured. **Required behind a reverse proxy** (see below). |
| `FERRY_SIGNAL_MAX_CONNS_PER_IP` | `16` | Concurrent connections per IPv4 address or IPv6 /64. Raise it if many users share one public IP (campus or carrier NAT). |
| `FERRY_SIGNAL_MAX_CONNS` | `10000` | Concurrent connections in total. |
| `FERRY_TURN_SECRET` | unset | coturn `static-auth-secret`. Enables `GET /v1/turn` (set together with `FERRY_TURN_URLS`). |
| `FERRY_TURN_URLS` | unset | Comma-separated `turn:`/`turns:` URLs handed to clients. |
| `RUST_LOG` | `info` | Log filter (`tracing` env-filter syntax). |

Empty variables count as unset; invalid values stop the server at startup with
an error. The remaining limits are fixed defaults. Embedders can change them
through `ferry_signal::Config::limits` when they call
`ferry_signal::serve_with_shutdown` from Rust.

### Behind a reverse proxy (TLS)

Peers that share a public IP see each other as "nearby", and connection limits
apply per IP. Behind a proxy every connection comes *from the proxy*, so you
**must** list the proxy in `FERRY_SIGNAL_TRUSTED_PROXIES`. Otherwise all
users appear to be on the same network, can see each other, and share one
16-connection limit. `X-Forwarded-For` is read right to left and the first
address that is not itself a trusted proxy wins, so clients cannot spoof their
address by sending the header themselves.

Caddy (TLS certificates and WebSocket upgrades are automatic):

```caddyfile
signal.example.com {
    reverse_proxy 127.0.0.1:3000
}
```

nginx:

```nginx
location /v1/ {
    proxy_pass http://127.0.0.1:3000;
    proxy_http_version 1.1;
    proxy_set_header Upgrade $http_upgrade;
    proxy_set_header Connection "upgrade";
    proxy_set_header Host $host;
    proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
}
```

Then run:

```sh
FERRY_SIGNAL_TRUSTED_PROXIES=127.0.0.1 \
FERRY_SIGNAL_ALLOWED_ORIGINS=https://ferry.example \
ferry-signal
```

Clients connect to `wss://signal.example.com/v1/ws`. The server itself does not
time out slow HTTP request headers. On the open internet, always run it behind a
proxy that limits slow clients (for example nginx's `client_header_timeout`).

### Docker

```sh
docker build -f crates/ferry-signal/Dockerfile -t ferry-signal .   # from the repo root
docker run -d --name ferry-signal -p 127.0.0.1:3000:3000 \
  -e FERRY_SIGNAL_ALLOWED_ORIGINS=https://ferry.example \
  -e FERRY_SIGNAL_TRUSTED_PROXIES=172.17.0.1 \
  ferry-signal
```

The image is distroless (no shell) and runs as a non-root user with a built-in
`HEALTHCHECK`. It sets `FERRY_SIGNAL_ADDR=0.0.0.0:3000` because a container must
listen on all interfaces. Publish the port on the host's loopback, as above,
and let the host's proxy terminate TLS. With Docker's default bridge network the
proxy's connections arrive from the bridge gateway (usually `172.17.0.1`), which
is why that address is the trusted proxy.

## Endpoints

| Endpoint | |
|---|---|
| `GET /v1/ws?d=<base64url(JSON client info)>` | The signaling WebSocket. |
| `GET /v1/turn?peer=<client id>` | TURN credentials (only when configured, otherwise `404`). |
| `GET /healthz` | `200 ok`. |

Every HTTP error has a JSON body, `{"type":"ERROR","code":<status>,"message":"…"}`.
`/v1/ws` and `/v1/turn` answer `403` for a disallowed `Origin`, `429` when an
IP group sends too many requests or opens too many connections, and `503` when
the server is full or shutting down. A bad `d` parameter gets `400`, or `413`
when it or one of its fields is too large.

## Limits

An *IP group* is one IPv4 address or one IPv6 /64.

| Limit | Default | When exceeded |
|---|---|---|
| WebSocket frame/message size | 64 KiB | `ERROR 413`, close `1009` |
| SDP / ICE candidate | 48 KiB / 4 KiB | `ERROR 413` |
| Client info fields | alias 64, version 32, deviceModel 64, token 512 chars; ≤16 caps of 32 chars; key 128 chars; `d` ≤ 4 KiB | HTTP `413` (at connect), `ERROR 413` (`UPDATE`) |
| Inbound frames per connection (all frames count, including invalid ones and pongs) | 30/s, burst 60 | `ERROR 429`, close `4029` |
| Invalid messages per connection | 10 | close `1008` |
| `UPDATE` per connection | 10/min | `ERROR 429` |
| `ROOM_JOIN` per connection | 10/min | `ERROR 429` |
| Short-code (`c:`) joins per IP group, across connections | 30/min | `ERROR 429` |
| Rooms per connection | 4 | `ERROR 403` |
| Peers per room | 32 | `ERROR 409` |
| Connections per IP group / in total | 16 / 10,000 | HTTP `429` / `503` |
| HTTP requests (`/v1/ws`, `/v1/turn`) per IP group | 1/s, burst 30 | HTTP `429` |
| Outbound queue per connection | 64 messages, 10 s per write | close `4008`. A slow reader is disconnected and never blocks other peers. |
| Server ping interval / idle timeout | 30 s / 60 s | close `4000` |

Close codes: `1001` server shutting down, `1008` too many invalid messages,
`1009` frame too large, `4000` idle timeout, `4008` too slow, `4029` rate
limited.

## TURN (optional)

Direct connections fail between some networks (symmetric NAT, strict
firewalls). A TURN server can then relay the WebRTC traffic. `ferry-signal`
does not relay anything itself. When you configure a TURN server you run, it
hands out short-lived credentials for it, using coturn's REST scheme
(`use-auth-secret`):

```ini
# turnserver.conf (coturn)
use-auth-secret
static-auth-secret=<the same value as FERRY_TURN_SECRET>
realm=turn.example.com
# Don't let the relay reach your internal network:
denied-peer-ip=10.0.0.0-10.255.255.255
denied-peer-ip=172.16.0.0-172.31.255.255
denied-peer-ip=192.168.0.0-192.168.255.255
```

```sh
FERRY_TURN_SECRET=<secret> \
FERRY_TURN_URLS="turn:turn.example.com:3478?transport=udp,turns:turn.example.com:5349" \
ferry-signal
```

When TURN is configured, `HELLO.server.caps` includes `"turn"`. A connected
client calls `GET /v1/turn?peer=<its own client id from HELLO>` from the same
network and receives
`{"iceServers":[{"urls":[…],"username":"<expiry>:<client id>","credential":"…"}],"ttl":600}`.
The credential is `base64(HMAC-SHA1(secret, username))` and expires after 10
minutes. Requests for clients that aren't connected, or that come from
another network, get `403`. No long-lived TURN credentials ever reach clients.

**Privacy:** the relay forwards the WebRTC data channel, which stays end-to-end
encrypted (DTLS), so the TURN operator cannot read files. It does see both
peers' IP addresses, connection times and traffic volume. The username contains
the signaling client id, so TURN logs can be linked to a signaling connection.
Ferry labels relayed transfers as **Relayed** in the app, and TURN is off unless
you configure it.

## What the server sees

Signaling necessarily exposes some metadata to whoever runs the server. That
includes device aliases, model and type, public keys, client IP addresses, room
ids, which peers exchange offers, and the SDPs and ICE candidates, which contain
network addresses. It never sees room secrets: link/QR rooms are hashed and the
secret stays in the URL fragment. It never sees file names or contents.
Ferry clients authenticate each other over the DTLS channel, so a malicious
server cannot impersonate peers in link/QR rooms; see `docs/04-threat-model.md`.

## Protocol

The wire protocol (client info, message types, rooms, limits, TURN) is
specified in [`docs/05-protocol.md` §5.1](../../docs/05-protocol.md#51-signaling-ferry-signal).
Upstream LocalSend clients see exactly the upstream message set. Extension
messages go only to clients that send an `ext` object.

## Development

```sh
cargo test -p ferry-signal                      # unit + end-to-end tests
cargo clippy -p ferry-signal --all-targets
```

`tests/server.rs` starts the real server on an ephemeral port and drives it
with real WebSocket clients. It covers presence, rooms, relaying, isolation,
every limit, TURN and shutdown.
