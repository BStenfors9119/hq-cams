#!/usr/bin/env bash
# hqcam footage sync.
#
# When the Pi is back on a network that can reach HQ storage, push the recorded
# clips off the USB drive to HQ (via rsync-over-ssh) and delete the local copies
# to reclaim space. Safe to run on a timer: it's a no-op unless enabled, the
# drive is mounted, and the destination is actually reachable.
#
# Why this is race-free against the recorder: the recorder only writes while
# *offline*, and this only transfers while it can reach HQ (i.e. *online*) — the
# two never run at once. Belt-and-braces, the recorder writes to `.mjpeg.part`
# and renames to `.mjpeg` only on clean close, and we transfer `*.mjpeg` only.
set -euo pipefail

CONF="${HQCAM_SYNC_CONF:-/usr/local/etc/hqcam/sync.conf}"
# shellcheck source=/dev/null
[ -f "$CONF" ] && . "$CONF"

SYNC_ENABLED="${SYNC_ENABLED:-no}"
SYNC_DEST="${SYNC_DEST:-}"
SYNC_SOURCE="${SYNC_SOURCE:-/media/hqcam}"
SYNC_SSH_KEY="${SYNC_SSH_KEY:-}"
SYNC_SSH_PORT="${SYNC_SSH_PORT:-22}"
SYNC_MIN_AGE="${SYNC_MIN_AGE:-30}"
SYNC_DELETE_AFTER="${SYNC_DELETE_AFTER:-yes}"
SYNC_BW_LIMIT="${SYNC_BW_LIMIT:-0}"

log() { echo "hqcam-sync: $*"; }

# Host[:port] extracted from an [user@]host:path rsync destination.
dest_host() { local hp="${SYNC_DEST%%:*}"; echo "${hp##*@}"; }

reachable() {
  timeout 5 bash -c "exec 3<>/dev/tcp/$1/$2" 2>/dev/null
}

# Rename power-cut `.part` leftovers (untouched for a while, so definitely not
# being written) back to `.mjpeg` so they get synced too. The recorder never
# writes `.part` while we're online, so a stale one is always an orphan.
recover_orphans() {
  local f
  while IFS= read -r f; do
    [ -n "$f" ] || continue
    log "recovering orphaned clip $(basename "$f")"
    mv -n "$f" "${f%.part}"
  done < <(find "$SYNC_SOURCE" -type f -name '*.mjpeg.part' \
                 ! -newermt "-${SYNC_MIN_AGE} seconds" 2>/dev/null)
}

ssh_cmd() {
  local s="ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new -o ConnectTimeout=10"
  [ -n "$SYNC_SSH_KEY" ] && s="$s -i $SYNC_SSH_KEY"
  [ "$SYNC_SSH_PORT" != "22" ] && s="$s -p $SYNC_SSH_PORT"
  echo "$s"
}

do_sync() {
  if [ -z "$SYNC_DEST" ]; then
    log "no SYNC_DEST configured — nothing to do"; exit 0
  fi
  if ! mountpoint -q "$SYNC_SOURCE"; then
    log "$SYNC_SOURCE is not mounted — no footage drive"; exit 0
  fi

  local host port; host="$(dest_host)"; port="$SYNC_SSH_PORT"
  if ! reachable "$host" "$port"; then
    log "HQ storage $host:$port not reachable — skipping (not on HQ?)"; exit 0
  fi

  recover_orphans

  local pending; pending="$(find "$SYNC_SOURCE" -type f -name '*.mjpeg' 2>/dev/null | wc -l)"
  if [ "$pending" -eq 0 ]; then
    log "nothing to sync"; exit 0
  fi
  log "syncing $pending clip(s) to $SYNC_DEST"

  # Assemble rsync options. --append-verify resumes a partial large file safely
  # (our clips are immutable once finalized); --remove-source-files frees space
  # only for files that transferred in full.
  local -a opts=(-a --partial --append-verify --timeout=300
                 --include='*/' --include='*.mjpeg' --exclude='*'
                 -e "$(ssh_cmd)")
  [ "$SYNC_DELETE_AFTER" = "yes" ] && opts+=(--remove-source-files)
  [ "$SYNC_BW_LIMIT" != "0" ] && opts+=(--bwlimit="$SYNC_BW_LIMIT")

  if rsync "${opts[@]}" "$SYNC_SOURCE/" "$SYNC_DEST"; then
    log "sync complete"
    # Drop now-empty date folders left behind by --remove-source-files.
    find "$SYNC_SOURCE" -mindepth 1 -type d -empty -delete 2>/dev/null || true
  else
    # Partial transfers are kept (--partial) and source files are only removed
    # once fully sent, so an interrupted run just resumes next time.
    log "rsync exited non-zero — will retry next run"
    exit 1
  fi
}

case "${1:-auto}" in
  auto)
    [ "$SYNC_ENABLED" = "yes" ] || { log "disabled (SYNC_ENABLED=no)"; exit 0; }
    do_sync ;;
  now)   do_sync ;;  # manual run, ignores the SYNC_ENABLED gate
  status)
    echo "enabled: $SYNC_ENABLED"
    echo "dest:    ${SYNC_DEST:-<unset>}"
    if mountpoint -q "$SYNC_SOURCE" 2>/dev/null; then mnt=yes; else mnt=no; fi
    echo "source:  $SYNC_SOURCE (mounted: $mnt)"
    echo "pending: $(find "$SYNC_SOURCE" -type f -name '*.mjpeg' 2>/dev/null | wc -l) clip(s)"
    ;;
  *) echo "usage: $0 {auto|now|status}" >&2; exit 2 ;;
esac
