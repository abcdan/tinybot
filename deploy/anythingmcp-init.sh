#!/bin/sh
set -eu

app=/run/anythingmcp-secrets
database=/run/anythingmcp-db
setup=/run/anythingmcp-setup

# AnythingMCP v0.55.9 runs as appuser (1001:65533). The PostgreSQL image
# reads POSTGRES_PASSWORD_FILE before dropping from root to postgres.
chown 1001:65533 "$app" "$database"
chmod 0700 "$app" "$database"
chmod 0755 "$setup"
umask 077

seed() {
    destination=$1
    supplied=$2
    count=$3
    if [ -s "$destination" ]; then
        return
    fi
    if [ -z "$supplied" ]; then
        supplied=$(od -An -v -tx1 -N "$count" /dev/urandom | tr -d ' \n')
        test "${#supplied}" -eq "$((count * 2))"
    fi
    temporary=$(mktemp "${destination}.XXXXXX")
    printf '%s' "$supplied" > "$temporary"
    chown 1001:65533 "$temporary"
    chmod 0400 "$temporary"
    ln "$temporary" "$destination" || test -s "$destination"
    rm -f "$temporary"
}

seed "$app/jwt_secret" "${AMCP_JWT_SECRET:-}" 32
seed "$app/cookie_secret" "${AMCP_COOKIE_SECRET:-}" 32
seed "$app/encryption_key" "${AMCP_ENCRYPTION_KEY:-}" 16
seed "$app/setup_password" '' 32
seed "$database/postgres_password" "${AMCP_POSTGRES_PASSWORD:-}" 32

temporary=$(mktemp "$setup/.htpasswd.XXXXXX")
printf '%s\n' "$(cat "$app/setup_password")" | htpasswd -nim setup > "$temporary"
chmod 0444 "$temporary"
mv "$temporary" "$setup/htpasswd"
