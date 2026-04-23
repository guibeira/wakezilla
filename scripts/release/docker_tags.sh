#!/usr/bin/env bash
set -euo pipefail

TAG="${1:?usage: docker_tags.sh <git-tag>}"

if [[ "${TAG}" != v* ]]; then
  echo "expected a git tag that starts with 'v', got: ${TAG}" >&2
  exit 1
fi

VERSION="${TAG#v}"

printf '%s\n' \
  "guibeira/wakezilla:${VERSION}" \
  "guibeira/wakezilla:v${VERSION}"

if [[ "${TAG}" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "guibeira/wakezilla:latest"
fi
