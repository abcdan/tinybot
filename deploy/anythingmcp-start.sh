#!/bin/sh
set -eu

directory=/run/anythingmcp-secrets
JWT_SECRET=$(cat "$directory/jwt_secret")
COOKIE_SECRET=$(cat "$directory/cookie_secret")
ENCRYPTION_KEY=$(cat "$directory/encryption_key")
DATABASE_URL=$(node -e '
    const password = require("node:fs").readFileSync("/run/anythingmcp-db/postgres_password", "utf8");
    process.stdout.write(`postgresql://anythingmcp:${encodeURIComponent(password)}@postgres:5432/anythingmcp`);
')
export JWT_SECRET COOKIE_SECRET ENCRYPTION_KEY DATABASE_URL

exec ./start.sh
