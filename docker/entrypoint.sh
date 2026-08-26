#!/bin/sh
set -eu

load_secret_file() {
    value_name="$1"
    file_name="$2"
    current_value="$(printenv "$value_name" 2>/dev/null || true)"
    secret_file="$(printenv "$file_name" 2>/dev/null || true)"

    # Explicit values keep backward compatibility for direct `docker run`
    # deployments. Compose uses the file variant so PEM material is absent
    # from the container configuration shown by `docker inspect`.
    if [ -n "$current_value" ] || [ -z "$secret_file" ]; then
        return
    fi
    if [ ! -f "$secret_file" ] || [ ! -r "$secret_file" ]; then
        echo "aero-entrypoint: $file_name does not name a readable file" >&2
        exit 1
    fi

    secret_value="$(sed -e '${/^$/d;}' "$secret_file")"
    if [ -z "$secret_value" ]; then
        echo "aero-entrypoint: $file_name points to an empty secret" >&2
        exit 1
    fi
    export "$value_name=$secret_value"
}

load_secret_file AERO__AUTH__JWT_PRIVATE_KEY_PEM AERO__AUTH__JWT_PRIVATE_KEY_FILE
load_secret_file AERO__AUTH__JWT_PUBLIC_KEY_PEM AERO__AUTH__JWT_PUBLIC_KEY_FILE
load_secret_file AERO_METRICS_TOKEN AERO_METRICS_TOKEN_FILE

# Bind-mounted development volumes are created by Docker as root. Repair their
# ownership on first attachment, then avoid an O(number-of-blobs) traversal on
# every restart. Files subsequently created by the gateway inherit the same
# unprivileged owner.
if [ "$(id -u)" -eq 0 ]; then
    aero_owner="$(id -u aero):$(id -g aero)"
    for data_dir in /var/lib/aero/blobs /var/lib/aero/hls; do
        if [ "$(stat -c '%u:%g' "$data_dir")" != "$aero_owner" ]; then
            chown -R aero:aero "$data_dir"
        fi
    done
    exec runuser -u aero -- "$@"
fi

exec "$@"
