#!/bin/sh
# Make the data directory usable, then drop privileges and run the server.
#
# This exists because of one thing a Dockerfile cannot do: a **bind mount replaces the directory it is
# mounted over, ownership and all**. The image creates `/data` and gives it to the unprivileged user;
# the moment somebody mounts a host path there, that work is gone and the directory belongs to whoever
# made it on the host — usually root. The server then cannot create its database, and SQLite reports
# error 14, which says "unable to open" and not "you do not have permission".
#
# So the container starts as root, fixes the ownership of the one directory it needs, and then becomes
# the unprivileged user for the actual work. Nothing runs as root except these few lines.
#
# `setpriv` rather than `gosu` or `su-exec`: it is part of util-linux, which is already in the base
# image, and it does not fork — the server keeps PID 1 and therefore keeps receiving the signals
# Docker sends it.
set -eu

: "${BOA_DATA_DIR:=/data}"
: "${BOA_UID:=10001}"
: "${BOA_GID:=10001}"

if [ "$(id -u)" = "0" ]; then
    mkdir -p "$BOA_DATA_DIR"

    # Only when it is not already ours. A store with tens of thousands of attachment blobs would
    # otherwise be walked in full on every restart, which on a slow disk is a visible delay for no
    # reason.
    owner=$(stat -c %u "$BOA_DATA_DIR" 2>/dev/null || echo unknown)
    if [ "$owner" != "$BOA_UID" ]; then
        echo "entrypoint: $BOA_DATA_DIR belongs to uid $owner; giving it to $BOA_UID" >&2
        chown -R "$BOA_UID:$BOA_GID" "$BOA_DATA_DIR"
    fi

    exec setpriv --reuid="$BOA_UID" --regid="$BOA_GID" --clear-groups boa-server "$@"
fi

# Already unprivileged — somebody set `user:` in their compose file. Nothing to fix and no privileges
# to drop; if the directory is not writable, the server will say so.
exec boa-server "$@"
