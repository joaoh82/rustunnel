In code (2 places — actual hardcoded default):
  - crates/rustunnel-client/src/main.rs:233-236 — the rustunnel setup default server prompt

  In configuration files (live, deployed):
  - deploy/server.toml — domain, cert paths, comments

  In documentation (everything else — no code impact):
  - README.md, docs/client-guide.md, docs/docker-deployment.md, docs/development/ai-agent-integration.md

  ---
  What you actually need to do, in order:

  1. DNS — add A records for edge.rustunnel.com and *.edge.rustunnel.com pointing to your server IP (keep the old tunnel.rustunnel.com records alive during transition so
  existing clients don't break)
  2. New TLS certificate — run Certbot for the new domain:
  certbot certonly --dns-cloudflare \
    --dns-cloudflare-credentials /etc/letsencrypt/cloudflare.ini \
    -d "edge.rustunnel.com" -d "*.edge.rustunnel.com"
  3. Server config (/etc/rustunnel/server.toml on your VPS):
  domain    = "edge.rustunnel.com"
  cert_path = "/etc/letsencrypt/live/edge.rustunnel.com/fullchain.pem"
  key_path  = "/etc/letsencrypt/live/edge.rustunnel.com/privkey.pem"
  3. Then systemctl restart rustunnel.service.
  4. Client configs — anyone with ~/.rustunnel/config.yml needs to change server: tunnel.rustunnel.com:4040 → server: edge.rustunnel.com:4040. They can just re-run
  rustunnel setup.
  5. Code + docs — update the hardcoded default and docs. Want me to do that now?

  The only actual code change is the two lines in main.rs for the rustunnel setup default. Everything else is docs and the deploy/server.toml template.

  ---

  What still works:
  - Old clients connect to tunnel.rustunnel.com:4040 (the DNS A record still points to the same server IP, same port) — the control-plane WebSocket connection succeeds.
  - The server accepts the connection regardless of the domain name the client used to reach it.

  What breaks:
  - HTTP tunnel URLs. The server now generates public URLs using domain = "edge.rustunnel.com", so a new tunnel gets https://abc123.edge.rustunnel.com. The old client is
  connected and working, but the URL it receives is on the new domain. Requests hitting *.tunnel.rustunnel.com won't be routed anywhere because the server is no longer
  listening under that domain name in its routing logic.

  So the actual breakage is on the public-facing URL side, not the client connection side.

  If you want a clean zero-downtime migration:

  1. Keep both DNS records alive.
  2. Update server.toml with the new domain and cert.
  3. Tell existing clients to update their config (rustunnel setup) — their connections will re-establish on the next reconnect and immediately get URLs on the new domain.
  4. Once all clients are updated and no traffic is hitting *.tunnel.rustunnel.com, remove the old DNS record.

  The transition window is just however long it takes clients to reconnect — since the client has auto-reconnect, it's effectively seamless as long as users update their
  config before the next reconnect cycle.