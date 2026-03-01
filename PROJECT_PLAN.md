# Project Plan and Progress

Status legend: `[ ]` pending, `[-]` in progress, `[x]` done

- [x] Capture requirements and implementation plan in docs
- [x] Scaffold Rust project and dependency baseline
- [x] TDD: unit tests for config parsing and model resolution
- [x] TDD: unit tests for JSON-path transforms
- [x] Implement config/model/transform modules to satisfy tests
- [x] TDD: unit tests for Copilot headers and auth token cache behavior
- [x] Implement auth and header modules to satisfy tests
- [x] Implement reverse-proxy request handling and route mapping
- [x] Write integration-style tests for proxy request rewriting and forwarding
- [x] Write README for users and developers
- [x] Run full test suite and finalize

## Notes
- Config source of truth for model mappings: `/mnt/d/sources/ai/litellm/config.yaml`
- Implementation path: `/mnt/d/sources/ai/litellm/copilot-proxy`
- Core TDD cycle completed: failing tests were added first, then implementations were added to satisfy tests.
