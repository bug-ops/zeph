---
applyTo: ".github/workflows/**/*.yml"
---

# CI Workflow Review

## Pipeline Structure

- Required structure: `lint-fmt` → `lint-clippy` → (`test`, `integration`, `coverage`) in parallel
- `docker-build-and-scan` must be present and treated as a required job
- Gate job `ci-status` must require all checks (including `docker-build-and-scan`)
- Test matrix: ubuntu, macos, windows
- Coverage via `cargo-llvm-cov` uploaded to codecov

## Security

- Reject `pull_request_target` trigger without explicit justification
- Prefer stable, reputable action versions (e.g., major tags like `v2`, `v6`); pin to full SHAs for security-sensitive workflows when feasible
- Reject secrets in workflow logs or step outputs
- Reject `--no-verify` or hook-skipping flags
