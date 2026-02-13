---
applyTo: ".github/workflows/**/*.yml"
---

# CI Workflow Review

## Pipeline Structure

- Required stages: `lint-fmt` → `lint-clippy` → `test` → `integration` → `coverage`
- Gate job `ci-status` must require all checks
- Test matrix: ubuntu, macos, windows
- Coverage via `cargo-llvm-cov` uploaded to codecov

## Security

- Reject `pull_request_target` trigger without explicit justification
- Pin action versions to full SHA — reject tag-only references
- Reject secrets in workflow logs or step outputs
- Reject `--no-verify` or hook-skipping flags
