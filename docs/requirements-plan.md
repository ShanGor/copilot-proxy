# GitHub Copilot Reverse Proxy (Rust) - Requirements and Plan

## 1. Goal
Build a lightweight Rust proxy that reuses LiteLLM-inspired GitHub Copilot authentication and VS Code-like header behavior, while doing direct reverse-proxying (no schema conversion).

## 2. Scope
### In Scope
1. GitHub device code auth -> GitHub access token -> Copilot API token.
2. VS Code/Copilot-like outbound headers.
3. Direct reverse proxying with streaming pass-through.
4. Config compatibility with current `config.yaml` model list shape.
5. Payload interpolation via JSON path transforms (`remove`, `add`, `replace`).

### Out of Scope
1. LiteLLM request/response model conversions.
2. Spend tracking, router/policy plugins, key management ecosystem.
3. Multi-provider abstraction in v1.

## 3. Functional Requirements
1. Parse current config shape:
   - `model_list[].model_name`
   - `model_list[].litellm_params.model`
   - `model_list[].model_info.mode`
   - `model_list[].litellm_params.drop_params`
2. Resolve model alias to upstream model id.
3. Implement token caching and refresh logic.
4. Build Copilot/VS Code header profile with configurable overrides.
5. Reverse-proxy supported routes without conversion.
6. Apply ordered JSON-path request transforms before forwarding.
7. Structured logging with secret redaction.

## 4. Non-Functional Requirements
1. Async runtime with low overhead.
2. Reliability with refresh lock/singleflight behavior.
3. Safe token persistence and redacted logs.

## 5. Configuration Strategy
1. Existing top-level `config.yaml` remains valid input.
2. Optional `proxy_settings` section extends behavior for:
   - listen address
   - auth token directory
   - header profile and overrides
   - upstream base url
   - transform rules

## 6. Milestones
1. Config parser + model resolver.
2. Auth/token storage.
3. Header builder.
4. Reverse proxy routes and streaming.
5. JSON-path transform engine.
6. Tests and docs.

## 7. Acceptance Criteria
1. Existing `config.yaml` loads without edits.
2. Alias model rewrites to mapped upstream model.
3. Token refresh works and is cached.
4. Streaming pass-through is preserved.
5. Transform operations work deterministically.
6. No provider conversion logic is introduced.

## 8. TDD Policy
1. Write unit tests first for each module.
2. Implement minimum code to pass tests.
3. Refactor while preserving passing tests.
