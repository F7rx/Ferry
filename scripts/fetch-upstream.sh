#!/usr/bin/env sh
# Clones the unmodified upstream LocalSend sources used as the reference peer
# in tests/interop (and for future upstream syncs). Gitignored.
set -eu
REV=2ef4dc6af1b81b3bee69db9b54299e39e6f368d1
cd "$(dirname "$0")/.."
mkdir -p .upstream
if [ ! -d .upstream/localsend ]; then
  git clone --filter=blob:none https://github.com/localsend/localsend.git .upstream/localsend
fi
git -C .upstream/localsend fetch --quiet origin "$REV" || true
git -C .upstream/localsend checkout --quiet "$REV"
test -z "$(git -C .upstream/localsend status --porcelain)" || { echo "upstream clone has local changes" >&2; exit 1; }
echo "upstream LocalSend at $REV"
