#!/bin/sh
# Points the web client at backend: REGENCY_BACKEND_URL (empty = same origin,
# e.g. when a reverse proxy routes /regency.v1.Regency/ to backend).
set -eu
url="${REGENCY_BACKEND_URL:-}"
escaped=$(printf '%s' "$url" | sed 's/[&|\\]/\\&/g')
sed -i "s|<meta name=\"regency-backend\" content=\"[^\"]*\"|<meta name=\"regency-backend\" content=\"$escaped\"|" \
  /usr/share/nginx/html/index.html
echo "regency web client: backend = ${url:-same origin}"
