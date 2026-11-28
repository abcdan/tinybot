#!/bin/sh
set -eu

repo_dir=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
image=tinybot-chromium-tests:local

docker build --tag "$image" "$repo_dir/workers/browser"
docker run --rm --init --network none --read-only --cap-drop ALL \
    --security-opt no-new-privileges:true \
    --security-opt "seccomp=$repo_dir/deploy/chromium-seccomp.json" \
    --tmpfs /tmp:mode=1777,size=192m --shm-size 256m -e HOME=/tmp \
    --mount "type=bind,src=$repo_dir/tests/browser.test.mjs,dst=/opt/tinybot/browser.test.mjs,readonly" \
    --mount "type=bind,src=$repo_dir/tests/mini_app_render.test.mjs,dst=/opt/tinybot/mini_app_render.test.mjs,readonly" \
    --mount "type=bind,src=$repo_dir/web,dst=/opt/tinybot/web,readonly" \
    "$image" node --test --test-concurrency=1 browser.test.mjs mini_app_render.test.mjs
