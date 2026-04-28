#!/usr/bin/env bash
set -euo pipefail

# Deploy a local docker-compose file to Portainer as a standalone stack.
# If a stack with the same name exists, update it; otherwise create it.
#
# Usage:
#   ./scripts/deploy_portainer_stack.sh \
#     --url https://portainer.local:9443 \
#     --api-key <key> \
#     --endpoint-id 2 \
#     --stack-name alfen-ha \
#     --compose-file docker-compose.yml

PORTAINER_URL=""
API_KEY=""
ENDPOINT_ID="2"
STACK_NAME="alfen-ha"
COMPOSE_FILE="docker-compose.yml"
INSECURE=1

usage() {
  cat <<'EOF'
Usage: deploy_portainer_stack.sh [options]

Options:
  --url <url>             Portainer base URL (e.g. https://host:9443)
  --api-key <key>         Portainer API key
  --endpoint-id <id>      Portainer endpoint id (default: 2)
  --stack-name <name>     Stack name to create/update (default: alfen-ha)
  --compose-file <path>   Compose file path (default: docker-compose.yml)
  --secure                Enforce TLS verification (default: insecure)
  -h, --help              Show this help
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --url)
      PORTAINER_URL="$2"
      shift 2
      ;;
    --api-key)
      API_KEY="$2"
      shift 2
      ;;
    --endpoint-id)
      ENDPOINT_ID="$2"
      shift 2
      ;;
    --stack-name)
      STACK_NAME="$2"
      shift 2
      ;;
    --compose-file)
      COMPOSE_FILE="$2"
      shift 2
      ;;
    --secure)
      INSECURE=0
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "Unknown option: $1" >&2
      usage
      exit 1
      ;;
  esac
done

if [[ -z "$PORTAINER_URL" || -z "$API_KEY" ]]; then
  echo "Both --url and --api-key are required." >&2
  usage
  exit 1
fi

if [[ ! -f "$COMPOSE_FILE" ]]; then
  echo "Compose file not found: $COMPOSE_FILE" >&2
  exit 1
fi

if ! command -v jq >/dev/null 2>&1; then
  echo "jq is required but not installed." >&2
  exit 1
fi

CURL_FLAGS=(--fail-with-body --show-error --silent)
if [[ "$INSECURE" -eq 1 ]]; then
  CURL_FLAGS+=(--insecure)
fi

API_BASE="${PORTAINER_URL%/}/api"
AUTH_HEADER=( -H "X-API-Key: ${API_KEY}" -H "Content-Type: application/json" )

echo "Reading compose file: $COMPOSE_FILE"
STACK_FILE_CONTENT="$(cat "$COMPOSE_FILE")"

echo "Looking up stack '${STACK_NAME}' on endpoint ${ENDPOINT_ID}"
STACKS_JSON="$(curl "${CURL_FLAGS[@]}" "${API_BASE}/stacks?endpointId=${ENDPOINT_ID}" "${AUTH_HEADER[@]}")"
STACK_ID="$(printf '%s' "$STACKS_JSON" | jq -r --arg name "$STACK_NAME" 'map(select(.Name == $name)) | first | .Id // empty')"

if [[ -n "$STACK_ID" ]]; then
  echo "Updating existing stack id=${STACK_ID}"
  UPDATE_PAYLOAD="$(jq -n \
    --arg content "$STACK_FILE_CONTENT" \
    '{stackFileContent: $content, env: [], prune: true, pullImage: true}')"

  curl "${CURL_FLAGS[@]}" \
    -X PUT "${API_BASE}/stacks/${STACK_ID}?endpointId=${ENDPOINT_ID}" \
    "${AUTH_HEADER[@]}" \
    -d "$UPDATE_PAYLOAD" >/dev/null

  echo "Stack updated successfully."
else
  echo "Stack not found; creating '${STACK_NAME}'"
  CREATE_PAYLOAD="$(jq -n \
    --arg name "$STACK_NAME" \
    --arg content "$STACK_FILE_CONTENT" \
    '{Name: $name, StackFileContent: $content, Env: [], FromAppTemplate: false}')"

  curl "${CURL_FLAGS[@]}" \
    -X POST "${API_BASE}/stacks/create/standalone/string?endpointId=${ENDPOINT_ID}" \
    "${AUTH_HEADER[@]}" \
    -d "$CREATE_PAYLOAD" >/dev/null

  echo "Stack created successfully."
fi
