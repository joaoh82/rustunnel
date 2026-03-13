 # On your local machine:
 ```sh
  npm run build           # builds + copies to assets/
  git add crates/rustunnel-server/src/dashboard/assets/
  git commit -m "chore: rebuild dashboard assets"
  git push
  ```

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

