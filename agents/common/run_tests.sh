#!/usr/bin/env bash
# Runs the nasiko_state tests once per supported a2a-sdk major: 0.3.x (pydantic tasks) and
# 1.x (protobuf tasks). The same test modules run in both; tests that only make sense on one
# major skip themselves on the other.
#
# Virtualenvs live outside the repository (default: $TMPDIR/nasiko-state-test-venvs, override
# with NASIKO_STATE_TEST_VENVS) and are reused between runs. `uv` is used when installed,
# otherwise `python3 -m venv` and pip.
#
# Usage: oss/agents/common/run_tests.sh [pytest args...]
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
venv_root="${NASIKO_STATE_TEST_VENVS:-${TMPDIR:-/tmp}/nasiko-state-test-venvs}"
python="${PYTHON:-python3}"

# name | a2a-sdk requirement | optional extra requirements. google-adk (nasiko_adk_sessions) is
# installed with the 0.3.x suite only, the major the stock google-adk agent pins; its tests skip
# themselves where google-adk is absent. langgraph covers the checkpointer, bounded as the stock langgraph
# agent bounds it (oss/agents/langgraph/pyproject.toml); anyio is pytest's async runner.
suites=(
  "a2a-0.3|a2a-sdk[http-server]>=0.3,<1|google-adk>=2.11,<3"
  "a2a-1.x|a2a-sdk[http-server]==1.1.1|"
)
shared_requirements=("langgraph>=1.0,<2" "langgraph-checkpoint>=4.1,<5" "pytest" "anyio")

install_requirements() {
  local venv="$1"
  shift
  if command -v uv >/dev/null 2>&1; then
    uv pip install --quiet --python "$venv/bin/python" "$@"
  else
    "$venv/bin/python" -m pip install --quiet "$@"
  fi
}

prepare_venv() {
  local venv="$1" sdk="$2" extras="$3" stamp="$1/.nasiko-state-requirements"
  local wanted="$sdk $extras ${shared_requirements[*]}"
  if [ ! -x "$venv/bin/python" ]; then
    if command -v uv >/dev/null 2>&1; then
      uv venv --quiet --python "$python" "$venv"
    else
      "$python" -m venv "$venv"
    fi
  fi
  if [ ! -f "$stamp" ] || [ "$(cat "$stamp")" != "$wanted" ]; then
    # shellcheck disable=SC2086  # extras is a single requirement or empty
    install_requirements "$venv" "$sdk" $extras "${shared_requirements[@]}"
    printf '%s' "$wanted" >"$stamp"
  fi
}

mkdir -p "$venv_root"
status=0
for suite in "${suites[@]}"; do
  IFS='|' read -r name sdk extras <<<"$suite"
  venv="$venv_root/$name"
  echo "=== $name ($sdk)"
  prepare_venv "$venv" "$sdk" "$extras"
  # No bytecode and no pytest cache: nothing may be written into the source tree.
  PYTHONDONTWRITEBYTECODE=1 "$venv/bin/python" -m pytest -p no:cacheprovider "$here/tests" "$@" || status=1
done
exit "$status"
