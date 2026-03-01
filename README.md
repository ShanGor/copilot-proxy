# Copilot Proxy (Rust)

Lightweight GitHub Copilot reverse proxy inspired by LiteLLM auth/header behavior.

## What It Does
- Uses LiteLLM-style `model_list` aliases from YAML.
- Rewrites `model` alias -> upstream Copilot model id.
- Emulates VS Code/Copilot outbound headers.
- Fetches Copilot API token using cached GitHub access token.
- If token files are missing, runs GitHub device-code auth flow, stores tokens, then starts serving.
- Reverse-proxies requests directly (no schema conversion).
- Applies optional JSON-path transforms (`remove`, `add`, `replace`) on request payloads.

## Current Support
- Primary tested route: `POST /v1/chat/completions`.
- Generic pass-through router for other paths is implemented; add route-specific tests before production use.

## Quick Start
1. Ensure you have Rust (`cargo`) installed.
2. Use existing LiteLLM-style config at `../config.yaml` or set `COPILOT_PROXY_CONFIG`.
3. Ensure `access-token` exists in token dir (default `~/.config/litellm/github_copilot/access-token`).
4. Run:

```bash
cd /mnt/d/sources/ai/litellm/copilot-proxy
cargo run
```

Default listen address: `0.0.0.0:4141`

### Enterprise Proxy Startup
Use a startup argument (curl-style) to force outbound internet access through an HTTP/HTTPS proxy:

```bash
cargo run -- --proxy http://proxy.company.local:8080
```

Or:

```bash
cargo run -- --proxy=http://proxy.company.local:8080
```

This applies to both:
- GitHub Copilot token refresh calls
- Upstream Copilot API reverse-proxy calls

### Debug Logging (Full Payloads)
The proxy logs full incoming request payloads and full upstream response payloads at `debug` level.

Enable with:

```bash
RUST_LOG=copilot_proxy=debug cargo run -- --proxy http://proxy.company.local:8080
```

Security note:
- Debug payload logs may contain sensitive prompts, responses, and metadata.
- Use in controlled environments only, and avoid enabling in production by default.

## Config
The project reads LiteLLM-compatible model mappings:

```yaml
model_list:
  - model_name: gpt-4o
    litellm_params:
      model: github_copilot/gpt-4o-2024-11-20
```

Optional extension section:

```yaml
proxy_settings:
  listen: 0.0.0.0:4141
  upstream_base: https://api.githubcopilot.com
  auth:
    token_dir: /path/to/token/dir
    github_api_key_url: https://api.github.com/copilot_internal/v2/token
  transforms:
    - when:
        route: /v1/chat/completions
        model: gpt-4o
      ops:
        - op: remove
          path: $.temperature
        - op: add
          path: $.metadata.source
          value: "copilot-proxy"
        - op: replace
          path: $.stream
          value: true
```

## JSON-path Notes
Supported path subset:
- `$.field`
- `$.nested.field`
- `$.array[0]`
- mixed forms like `$.metadata.tags[0]`

Missing paths are treated as no-op for `remove`; `add`/`replace` create missing containers.

## Environment Variables
- `COPILOT_PROXY_CONFIG`: path to YAML config file.
- `GITHUB_COPILOT_TOKEN_DIR`: override token directory.
- `RUST_LOG`: log level filter.

Note: proxy routing is controlled by the startup `--proxy` argument (not config file).

## Development (TDD)
### Run tests
```bash
cargo test
```

### Project layout
- `src/config.rs`: YAML parsing + model resolution.
- `src/transform.rs`: JSON-path transform engine.
- `src/headers.rs`: VS Code/Copilot header profile.
- `src/auth.rs`: token cache + API key refresh logic.
- `src/proxy.rs`: reverse proxy router + request rewrite.
- `src/main.rs`: runtime wiring and startup.

### Progress tracking
See [`PROJECT_PLAN.md`](./PROJECT_PLAN.md).

## Security and Ops Notes
- Never commit token files.
- Token data is stored in plaintext files (`access-token`, `api-key.json`) by design for compatibility.
- Add TLS termination, authz, and request limits in front of this service for production.
