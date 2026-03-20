# Updating version for full release:

⏺ Update all four crates, in this order:

  ┌──────────────────────────────────────┬────────────────────────────────────────────────────────────────────┐
  │                 File                 │                                Why                                 │
  ├──────────────────────────────────────┼────────────────────────────────────────────────────────────────────┤
  │ crates/rustunnel-client/Cargo.toml   │ rustunnel --version                                                │
  ├──────────────────────────────────────┼────────────────────────────────────────────────────────────────────┤
  │ crates/rustunnel-server/Cargo.toml   │ rustunnel-server --version + embedded in the startup banner        │
  ├──────────────────────────────────────┼────────────────────────────────────────────────────────────────────┤
  │ crates/rustunnel-mcp/Cargo.toml      │ rustunnel-mcp --version + reported in the MCP initialize handshake │
  ├──────────────────────────────────────┼────────────────────────────────────────────────────────────────────┤
  │ crates/rustunnel-protocol/Cargo.toml │ shared library — keep in sync to avoid confusion                   │
  └──────────────────────────────────────┴────────────────────────────────────────────────────────────────────┘

  Then tag git with the same version:

  1. bump all four Cargo.toml files to e.g. 0.3.0
    - Run: `./scripts/bump-version.sh 0.3.0`
  2. git add -A && git commit -m "chore: bump version to 0.3.0"
  3. git tag v0.3.0
  4. git push && git push --tags
 

# On the server (or build machine):

  1. `git pull` — fetches latest code
  2. `make update-server`
  or
  2. `cd dashboard-ui && npm run build` - Rebuilt Next.js, copied fresh out/ into assets/
  2. `make release-server` — recompiles the server with the new embedded assets
  4. install — copies the binary to /usr/local/bin/rustunnel-server
    - `install -Dm755 target/release/rustunnel-server /usr/local/bin/rustunnel-server`
  5. `systemctl restart rustunnel.service` — restarts the service

# Check it started
```sh
systemctl status rustunnel.service
journalctl -u rustunnel.service -f
```

