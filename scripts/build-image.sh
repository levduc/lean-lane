#!/usr/bin/env bash
# build-image.sh [--push REPO] — build the lean-lane container image.
#
#   ./scripts/build-image.sh
#       builds lean-lane:<git-short-sha> and lean-lane:local
#   ./scripts/build-image.sh --push ghcr.io/<owner>/lean-lane
#       additionally tags REPO:<sha> and REPO:latest and pushes both
#
# Without --push nothing leaves this machine; the push commands are printed.
set -euo pipefail
cd "$(cd "$(dirname "$0")/.." && pwd)"

PUSH=""
while [ $# -gt 0 ]; do
  case "$1" in
    --push) PUSH=${2:?--push needs a repository, e.g. ghcr.io/<owner>/lean-lane}; shift 2 ;;
    -h|--help) sed -n '2,9p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

SHA=$(git rev-parse --short HEAD)
[ -z "$(git status --porcelain --untracked-files=no)" ] || SHA="$SHA-dirty"
RUST_VERSION=$(sed -n 's/^channel *= *"\(.*\)"/\1/p' rust-toolchain.toml)

DOCKER_BUILDKIT=1 docker build \
  --build-arg RUST_VERSION="$RUST_VERSION" \
  --label org.opencontainers.image.revision="$(git rev-parse HEAD)" \
  --label org.opencontainers.image.title=lean-lane \
  -t "lean-lane:$SHA" -t lean-lane:local .

echo "built lean-lane:$SHA and lean-lane:local ($(docker image inspect lean-lane:local --format '{{.Size}}' | awk '{printf "%.1f MB", $1/1e6}'))"

if [ -n "$PUSH" ]; then
  docker tag "lean-lane:$SHA" "$PUSH:$SHA"
  docker tag "lean-lane:$SHA" "$PUSH:latest"
  docker push "$PUSH:$SHA"
  docker push "$PUSH:latest"
else
  echo "to publish (not done):"
  echo "  docker tag lean-lane:$SHA ghcr.io/<owner>/lean-lane:$SHA && docker push ghcr.io/<owner>/lean-lane:$SHA"
fi
