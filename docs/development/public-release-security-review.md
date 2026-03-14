# Public Release Security Review

**Date**: 2026-03-12
**Reviewer**: Claude Code (automated scan + manual verification)
**Verdict**: ✅ Safe to make public — no blocking issues found

---

## Scope

Full repository scan covering:
- Source code (Rust, TypeScript)
- Configuration and deployment files
- Documentation
- Test fixtures
- `.gitignore` and git history

---

## Findings

### 🟢 LOW / Informational — No Action Required

---

#### 1. Development placeholder token in `deploy/local/server.toml`

**File**: `deploy/local/server.toml:13`
```toml
admin_token = "dev-secret-change-me"
```
**Assessment**: Not a real secret. The value is an obvious placeholder, the file is clearly scoped to local development (`deploy/local/`), and `require_auth = false` in the same file confirms this is not a production config. Safe to publish.

---

#### 2. Empty `admin_token` in production config template

**File**: `deploy/server.toml:64`
```toml
# TODO: replace with a real secret before starting the service.
admin_token = ""
```
**Assessment**: Empty string — no secret exposed. The TODO comment and the `openssl rand -hex 32` example in the comment above are correct and instructional. Safe to publish.

---

#### 3. Grafana default password in Docker Compose

**File**: `deploy/docker-compose.yml:78`
```yaml
GF_SECURITY_ADMIN_PASSWORD: "${GRAFANA_PASSWORD:-changeme}"
```
**Assessment**: Uses an environment variable with a `changeme` fallback. The fallback is a well-known Docker Compose convention for examples. No real credential exposed. The monitoring stack is opt-in (`--profile monitoring`). Safe to publish, but consider adding a note in the README reminding operators to set `GRAFANA_PASSWORD` in production.

---

#### 4. Public domain name referenced in config and docs

**Files**: `deploy/server.toml`, `README.md`, `docs/client-guide.md`, `docs/architecture.md`
```
edge.rustunnel.com
```
**Assessment**: This is the project's own public domain — a DNS name you control and intend to be publicly known. Exposing it in a public repository is intentional and harmless. It functions as the example domain throughout the documentation, which is standard practice for open-source tunnel services (ngrok, Cloudflare Tunnel, etc. all do this).

---

#### 5. Email placeholder in certbot instructions

**File**: `deploy/server.toml:36`
```toml
#   --agree-tos --email your@email.com
```
**Assessment**: Literal placeholder text in a comment, not a real address. Safe to publish.

---

#### 6. Grafana admin username hardcoded

**File**: `deploy/docker-compose.yml:77`
```yaml
GF_SECURITY_ADMIN_USER: admin
```
**Assessment**: `admin` is the Grafana default and is universally expected in example Compose files. Not a credential leak. Safe to publish.

---

## Areas Confirmed Clean

| Area | Status | Notes |
|------|--------|-------|
| Private keys / certificates | ✅ Clean | No `.pem`, `.key`, `.crt` files; gitignore covers them |
| API keys / bearer tokens | ✅ Clean | All values are empty strings or obvious placeholders |
| Database files | ✅ Clean | No `.db` / `.sqlite` files tracked; gitignore covers them |
| Real passwords | ✅ Clean | None found |
| Personal information | ✅ Clean | No real names, email addresses, or phone numbers |
| Internal IP addresses | ✅ Clean | Only `localhost` / `0.0.0.0` used |
| AWS / GCP / cloud resource names | ✅ Clean | None present |
| Cloudflare API token / Zone ID | ✅ Clean | Empty strings in config; code reads from env vars |
| Private registry tokens | ✅ Clean | `Cargo.toml` and `package.json` use public registries only |
| Log files | ✅ Clean | No `.log` files tracked |
| Test fixtures with real data | ✅ Clean | Tests use synthetic tokens and temp directories |
| Hardcoded production URLs | ✅ Clean | All URLs are parameterised via config |
| `.env` files | ✅ Clean | None tracked; gitignore covers `*.env` patterns |
| TLS certificates in source | ✅ Clean | Tests generate certs dynamically in `/tmp` |
| Git history | ✅ Clean | One commit (`5ab7820`) explicitly purged credentials |

---

## Recommendations Before Publishing

None are blocking, but the following would improve the production operator experience:

1. **Add a `GRAFANA_PASSWORD` reminder to README or `deploy/server.toml`**
   The Docker Compose default `changeme` will work but is insecure. A one-line note like _"Set `GRAFANA_PASSWORD` in your environment before running the monitoring stack"_ is sufficient.

2. **Consider replacing `edge.rustunnel.com` with a generic example domain in config templates**
   Using `tunnel.example.com` in `deploy/server.toml` and the README would make it clearer that operators must substitute their own domain. This is cosmetic and not a security concern — the current domain is public by design — but it is a common convention for open-source project templates.

3. **Ensure `deploy/local/server.toml` has a clear header comment**
   It currently has none. Adding `# LOCAL DEVELOPMENT ONLY — do not use in production` at the top of the file removes any ambiguity for new contributors.

---

## `.gitignore` Coverage Verification

The following sensitive patterns are correctly excluded:

```
*.pem  *.key  *.crt  *.p12  *.pfx   # TLS credentials
.env  .env.*                          # Environment files
*.db  *.sqlite  *.sqlite3             # Database files
*.log  logs/                          # Log files
.claude/                              # Claude Code project memory
dashboard-ui/node_modules/            # npm dependencies
dashboard-ui/.next/  dashboard-ui/out/ # Next.js build artifacts
```

No gaps identified.

---

## Final Verdict

**The repository is safe to make public.**

The codebase follows sound secret-hygiene practices: all sensitive values are either empty placeholders, environment-variable references, or clearly-labelled development stubs. There are no real credentials, private keys, personal data, or internal infrastructure details that would pose a risk if exposed publicly.
