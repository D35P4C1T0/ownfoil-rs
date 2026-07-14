#!/bin/sh
set -eu

if [ "$(id -u)" -ne 0 ]; then
    exec "$@"
fi

uid="${PUID:-1000}"
gid="${PGID:-1000}"

if ! getent group "$gid" >/dev/null 2>&1; then
    if command -v groupadd >/dev/null 2>&1; then
        groupadd --gid "$gid" ownfoil
    else
        addgroup -S -g "$gid" ownfoil
    fi
fi
group_name="$(getent group "$gid" | cut -d: -f1)"

if ! getent passwd "$uid" >/dev/null 2>&1; then
    if command -v useradd >/dev/null 2>&1; then
        useradd --uid "$uid" --gid "$group_name" --create-home --shell /usr/sbin/nologin ownfoil
    else
        adduser -S -D -H -u "$uid" -G "$group_name" -s /sbin/nologin ownfoil
    fi
fi
user_name="$(getent passwd "$uid" | cut -d: -f1)"

chown "$uid:$gid" /app/config /app/data
if command -v gosu >/dev/null 2>&1; then
    exec gosu "$user_name:$group_name" "$@"
fi
exec su-exec "$user_name:$group_name" "$@"
