#!/usr/bin/env bash
# Runs the nasiko-state JavaScript tests (oss/agents/common/js/nasiko-state) with node's built-in
# test runner, against the @a2a-js/sdk version the store is written for.
#
# node_modules never lands in the repository: the package is copied into a work directory outside
# it (default $TMPDIR/nasiko-state-js-env, override with NASIKO_STATE_JS_ENV), the SDK is installed
# there, and the tests run from the copy. The copy is refreshed on every run.
#
# Usage: oss/agents/common/run_js_tests.sh [node --test args...]
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
package="$here/js/nasiko-state"
env_root="${NASIKO_STATE_JS_ENV:-${TMPDIR:-/tmp}/nasiko-state-js-env}"
sdk="@a2a-js/sdk@1.3.0"

mkdir -p "$env_root"
rm -rf "$env_root/src" "$env_root/test"
cp -R "$package/src" "$package/test" "$package/package.json" "$env_root/"

stamp="$env_root/node_modules/.nasiko-state-sdk"
if [ ! -f "$stamp" ] || [ "$(cat "$stamp")" != "$sdk" ]; then
  npm install --prefix "$env_root" --silent --no-audit --no-fund --no-save "$sdk"
  printf '%s' "$sdk" >"$stamp"
fi

cd "$env_root"
exec node --test "$@" "test/*.test.js"
