#!/usr/bin/env bash
set -euo pipefail

# Build and export a Docker image as a tar file for Portainer import.
# Usage:
#   ./scripts/export_portainer_image.sh
#   ./scripts/export_portainer_image.sh --image alfen-ha:local --output alfen-ha-local.tar
#   ./scripts/export_portainer_image.sh --gzip
#   ./scripts/export_portainer_image.sh --no-build

IMAGE="alfen-ha:local"
OUTPUT="alfen-ha-local.tar"
DO_BUILD=1
DO_GZIP=0

usage() {
  cat <<'EOF'
Usage: export_portainer_image.sh [options]

Options:
  --image <name:tag>   Docker image name to export (default: alfen-ha:local)
  --output <file.tar>  Output tar filename (default: alfen-ha-local.tar)
  --no-build           Skip docker build step
  --gzip               Compress resulting tar to .tar.gz
  -h, --help           Show this help
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --image)
      IMAGE="$2"
      shift 2
      ;;
    --output)
      OUTPUT="$2"
      shift 2
      ;;
    --no-build)
      DO_BUILD=0
      shift
      ;;
    --gzip)
      DO_GZIP=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "Unknown argument: $1" >&2
      usage
      exit 1
      ;;
  esac
done

if [[ "$DO_BUILD" -eq 1 ]]; then
  echo "[1/2] Building image: $IMAGE"
  docker build -t "$IMAGE" .
else
  echo "[1/2] Skipping build"
fi

echo "[2/2] Exporting image to: $OUTPUT"
docker save -o "$OUTPUT" "$IMAGE"

if [[ "$DO_GZIP" -eq 1 ]]; then
  echo "Compressing: $OUTPUT"
  gzip -f "$OUTPUT"
  echo "Done: ${OUTPUT}.gz"
else
  echo "Done: $OUTPUT"
fi

echo "Import in Portainer via: Images -> Load image"
