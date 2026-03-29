#!/usr/bin/env bash
# check-env-vars.sh — verify ZEPH_* env var coverage in docker-compose.yml vs env.rs
#
# Exits with code 1 if any vars defined in env.rs are missing from compose.yml.
# Run from the repository root.

set -euo pipefail

ENV_RS="crates/zeph-config/src/env.rs"
COMPOSE_YML="docker/docker-compose.yml"

if [[ ! -f "$ENV_RS" ]]; then
  echo "ERROR: $ENV_RS not found. Run from the repository root." >&2
  exit 2
fi

if [[ ! -f "$COMPOSE_YML" ]]; then
  echo "ERROR: $COMPOSE_YML not found. Run from the repository root." >&2
  exit 2
fi

# Vars present in env.rs only as deprecated no-ops (warn and ignore, not user-configurable)
DEPRECATED_VARS=(
  ZEPH_STT_MODEL
  ZEPH_STT_BASE_URL
)

# Extract ZEPH_* var names from env.rs (string literals passed to env::var / env::var_os)
env_rs_vars=$(grep -oE '"ZEPH_[A-Z0-9_]+"' "$ENV_RS" | tr -d '"' | sort -u)

# Extract ZEPH_* var names from compose.yml (keys in environment: block)
compose_vars=$(grep -oE 'ZEPH_[A-Z0-9_]+' "$COMPOSE_YML" | sort -u)

missing=()
while IFS= read -r var; do
  # Skip deprecated vars that exist in env.rs only to emit warnings
  if printf '%s\n' "${DEPRECATED_VARS[@]}" | grep -qx "$var"; then
    continue
  fi
  if ! echo "$compose_vars" | grep -qx "$var"; then
    missing+=("$var")
  fi
done <<< "$env_rs_vars"

if [[ ${#missing[@]} -eq 0 ]]; then
  echo "OK: all ZEPH_* vars from $ENV_RS are present in $COMPOSE_YML"
  exit 0
fi

echo "DRIFT DETECTED: the following ZEPH_* vars are in $ENV_RS but missing from $COMPOSE_YML:"
for var in "${missing[@]}"; do
  echo "  - $var"
done
echo ""
echo "Add them to $COMPOSE_YML with empty defaults, e.g.:"
echo "  ${missing[0]}: \${${missing[0]}:-}"
exit 1
