#!/bin/sh
# DESIGN §18.2. `bgutil-pot` is deliberately NOT started here: the server supervises it, so it
# inherits the already-dropped privileges and gets restarted when it dies (DESIGN §16.2).
set -eu

# UID/GID here are *environment* variables the operator may set (the legacy contract), not the
# shell's own — dash does not define them, which is what SC3028 is warning about.
# shellcheck disable=SC3028
PUID="${UID:-$PUID}"          # legacy UID/GID still win, exactly as before
PGID="${GID:-$PGID}"

# The audio root defaults to the download root, so the two collapse into one path unless the
# operator split them (the shipped compose does: /downloads/audio).
AUDIO_DIR="${AUDIO_DOWNLOAD_DIR:-$DOWNLOAD_DIR}"
TEMP_DIR="${TEMP_DIR:-${DOWNLOAD_DIR}/.aulos-tmp}"
export TEMP_DIR

if [ "$(id -u)" -eq 0 ] && [ "$(id -g)" -eq 0 ]; then
  IS_ROOT=1
else
  IS_ROOT=0
fi

echo "Setting umask to ${UMASK}"
umask "${UMASK}"
echo "Creating download (${DOWNLOAD_DIR}), state (${STATE_DIR}), temp (${TEMP_DIR}), audio (${AUDIO_DIR}) directories"
# A directory *we* create as root is our own mess: chown it immediately, before the CHOWN_DIRS
# switch below, because `CHOWN_DIRS=false` means "do not walk the operator's library", not "leave
# a root-owned directory the de-privileged server cannot write into". Without this, a split
# AUDIO_DOWNLOAD_DIR (or a first start on an empty volume) boots green and then fails every write
# with EACCES. DOWNLOAD_DIR comes first so a nested audio root inherits an already-owned parent.
#
# `[ -d "$d" ] && continue` would be an AND-OR list whose overall status is non-zero when the
# directory is missing, which `set -e` turns into an exit — hence the `if`.
for d in "${DOWNLOAD_DIR}" "${STATE_DIR}" "${TEMP_DIR}" "${AUDIO_DIR}"; do
  if [ ! -d "$d" ]; then
    mkdir -p "$d"
    if [ "${IS_ROOT}" -eq 1 ]; then
      echo "Created ${d}; giving it to ${PUID}:${PGID}"
      chown "${PUID}:${PGID}" "$d"
    fi
  fi
done

if [ "${IS_ROOT}" -eq 1 ]; then
  # An `[ … ] && echo …` one-liner would be the last command of the list and abort the script
  # under `set -e` whenever PUID is non-zero — i.e. in the normal case.
  if [ "${PUID}" -eq 0 ]; then
    echo "Warning: running as root is not recommended; check PUID/PGID (or legacy UID/GID)"
  fi
  case "${CHOWN_DIRS:-true}" in
    false)     echo "Skipping ownership changes (CHOWN_DIRS=false)" ;;
    recursive) echo "Changing ownership recursively (legacy behaviour)"
               chown -R "${PUID}:${PGID}" "${DOWNLOAD_DIR}" "${STATE_DIR}" "${TEMP_DIR}" "${AUDIO_DIR}" ;;
    *)         echo "Changing ownership of the directories themselves and the state dir"
               chown    "${PUID}:${PGID}" "${DOWNLOAD_DIR}" "${TEMP_DIR}" "${AUDIO_DIR}"
               chown -R "${PUID}:${PGID}" "${STATE_DIR}" ;;
  esac
  echo "Running aulos-server as ${PUID}:${PGID}"
  exec gosu "${PUID}:${PGID}" /usr/local/bin/aulos-server "$@"
else
  echo "User set by docker; running aulos-server as $(id -u):$(id -g)"
  exec /usr/local/bin/aulos-server "$@"
fi
