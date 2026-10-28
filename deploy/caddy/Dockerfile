# Caddy with the layer4 plugin (github.com/mholt/caddy-l4), so that TURN over
# TLS can share port 443 with HTTPS. Keep both versions pinned and update them
# together; the plugin release names the Caddy version it was built against.
FROM caddy:2.11.7-builder AS builder
RUN xcaddy build v2.11.7 --with github.com/mholt/caddy-l4@v0.1.2

FROM caddy:2.11.7
COPY --from=builder /usr/bin/caddy /usr/bin/caddy
