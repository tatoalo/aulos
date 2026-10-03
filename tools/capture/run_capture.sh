#!/usr/bin/env bash
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
PY="${PYTHON:-python3}"
"${PY}" "${HERE}/dump_formats.py"
"${PY}" "${HERE}/dump_progress_vectors.py"
"${PY}" "${HERE}/verify.py"
