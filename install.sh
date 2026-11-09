#!/bin/sh
# regency all-in-one installer: turns this machine into a complete regency
# (RabbitMQ + CouchDB + every regency service and the web UI, one systemd
# unit). For trying regency, labs, small setups: one node, NOT highly
# available, security level 1 (authenticated, clear text).
#
#   curl -fsSL https://github.com/regent-project/regency/releases/latest/download/install.sh | sh
#
# Options (after `sh -s --`), or the environment variables in brackets:
#   --version <x.y.z>         regency release to install [REGENCY_VERSION]
#                             (default: the latest release)
#   --package <file>          install this regency-all-in-one .deb/.rpm
#                             instead of downloading one [REGENCY_PACKAGE]
#   --admin-password <pw>     initial password of the web UI's `admin`, first
#                             install only [REGENCY_ADMIN_PASSWORD]
#                             (default: generated and printed once)
#   --no-start                install and configure, do not start regency
#   -h, --help
#
# Supported: x86_64 with systemd; Debian 13+, Ubuntu 24.04+, RHEL 10 and
# compatibles (Rocky, Alma, CentOS Stream, Oracle), Fedora 40+ (best effort:
# CouchDB publishes no Fedora packages, its el10 ones are used).
#
# What it does (safe to run again; it also upgrades regency):
#   1. RabbitMQ 4.x from Team RabbitMQ's repositories, plugins management and
#      stream, listening on localhost only; an admin account `regency-admin`.
#   2. CouchDB 3.x from Apache's repositories, single node, on localhost
#      only, with an admin account.
#      A RabbitMQ or CouchDB already installed is reused, not reconfigured:
#      the script only enables the plugins it needs and adds its own admin
#      account (CouchDB: `regency-admin`, unless COUCHDB_ADMIN_USER and
#      COUCHDB_ADMIN_PASSWORD are given).
#   3. The regency-all-in-one package of the GitHub release; the admin
#      credentials go into /etc/regency/all-in-one.env (root:regency, 0640),
#      used only by `regencyctl init` at start.
#   4. Starts regency-all-in-one and waits until the web UI answers on port
#      8080 (port 8443: endpoint for regops agents).

# Everything runs from main(), called on the last line: a truncated download
# runs nothing.
set -eu

REPO="regent-project/regency"
ENV_FILE="/etc/regency/all-in-one.env"
SECRETS_DIR="/var/lib/regency/secrets"
RABBITMQ_ADMIN="regency-admin"

# ---------------------------------------------------------------- utilities

if [ -t 1 ]; then
    BOLD="$(printf '\033[1m')"; RED="$(printf '\033[31m')"
    YELLOW="$(printf '\033[33m')"; RESET="$(printf '\033[0m')"
else
    BOLD=""; RED=""; YELLOW=""; RESET=""
fi

say() { printf '%s>>> %s%s\n' "$BOLD" "$*" "$RESET"; }
warn() { printf '%sWARNING: %s%s\n' "$YELLOW" "$*" "$RESET" >&2; }
die() { printf '%sERROR: %s%s\n' "$RED" "$*" "$RESET" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

usage() {
    cat <<'EOF'
regency all-in-one installer: RabbitMQ, CouchDB and every regency service on
this machine (single node, not highly available).

  curl -fsSL https://github.com/regent-project/regency/releases/latest/download/install.sh | sh
  curl -fsSL .../install.sh | sh -s -- [options]

Options (environment variable in brackets):
  --version <x.y.z>       regency release to install [REGENCY_VERSION] (default: latest)
  --package <file>        install this regency-all-in-one .deb/.rpm [REGENCY_PACKAGE]
  --admin-password <pw>   initial password of the web UI's `admin`, first install
                          only [REGENCY_ADMIN_PASSWORD] (default: generated, printed once)
  --no-start              install and configure, do not start regency
  -h, --help

Existing RabbitMQ/CouchDB installations are reused; for CouchDB, pass an admin
account with COUCHDB_ADMIN_USER / COUCHDB_ADMIN_PASSWORD or one is added.
EOF
}

# runs a command as root
as_root() {
    if [ -n "$SUDO" ]; then "$SUDO" "$@"; else "$@"; fi
}

# write_file <path> <mode> [owner:group] < content
write_file() {
    # a missing directory is created 0755 (e.g. /etc/rabbitmq before the
    # package: its service must read it); existing ones are left as they are
    as_root test -d "$(dirname "$1")" || as_root install -d -m 0755 "$(dirname "$1")"
    as_root sh -c 'umask 077; cat > "$1"' sh "$1"
    as_root chmod "$2" "$1"
    if [ -n "${3:-}" ]; then as_root chown "$3" "$1"; fi
}

# 32 alphanumerics: safe in env files, URLs and ini files
random_password() { LC_ALL=C tr -dc 'A-Za-z0-9' < /dev/urandom | head -c 32; }

# value of KEY in the env file ('' when absent or empty); never sourced
env_get() {
    as_root test -f "$ENV_FILE" || return 0
    as_root sed -n "s/^$1=//p" "$ENV_FILE" | tail -n 1 | sed "s/^'\(.*\)'\$/\1/; s/^\"\(.*\)\"\$/\1/"
}

# sets KEY='value' in the env file (replacing an active or commented line)
env_set() {
    as_root awk -v key="$1" -v val="$2" '
        BEGIN { line = key "=\x27" val "\x27"; done = 0 }
        $0 ~ "^#?" key "=" { if (!done) { print line; done = 1 }; next }
        { print }
        END { if (!done) print line }
    ' "$ENV_FILE" > "$TMP/env.new"
    as_root sh -c 'cat "$1" > "$2"' sh "$TMP/env.new" "$ENV_FILE"
    rm -f "$TMP/env.new"
}

# rabbitmqctl/rabbitmq-plugins as root, without Elixir's locale warning
rmq() { tool="$1"; shift; as_root env LC_ALL=C.UTF-8 ELIXIR_ERL_OPTIONS=+fnu "$tool" "$@"; }

# comments KEY out of the env file
env_unset() {
    as_root sed -i "s/^$1=.*/#$1=/" "$ENV_FILE"
}

# curl reading the credentials from stdin (never on the command line)
# usage: printf 'user = "u:p"\n' | curl_auth <url>
curl_auth() { curl -fs -o /dev/null --max-time 10 -K - "$1"; }

wait_for() { # <seconds> <command...>: until the command succeeds
    t="$1"; shift
    while ! "$@" >/dev/null 2>&1; do
        t=$((t - 2))
        [ "$t" -gt 0 ] || return 1
        sleep 2
    done
}

# ---------------------------------------------------------------- checks

detect_system() {
    [ "$(uname -s)" = Linux ] || die "regency's installer supports Linux only."
    case "$(uname -m)" in
        x86_64|amd64) ;;
        *) die "regency packages exist for x86_64 only (this machine: $(uname -m))." ;;
    esac
    [ -d /run/systemd/system ] || die "systemd is not running here (container?). \
Use the Docker quick start instead: https://github.com/$REPO/blob/main/deploy/compose/quickstart.yaml"
    [ -r /etc/os-release ] || die "cannot identify the distribution (/etc/os-release)."

    # shellcheck disable=SC1091
    . /etc/os-release
    OS_ID="${ID:-}"; OS_LIKE="${ID_LIKE:-}"; OS_VERSION="${VERSION_ID:-0}"
    OS_MAJOR="${OS_VERSION%%.*}"
    OS_CODENAME="${VERSION_CODENAME:-}"

    case " $OS_ID $OS_LIKE " in
        *" ubuntu "*)
            FAMILY=deb; DISTRO=ubuntu
            [ "$OS_ID" = ubuntu ] || OS_CODENAME="${UBUNTU_CODENAME:-$OS_CODENAME}"
            if [ "$OS_ID" = ubuntu ] && [ "$OS_MAJOR" -lt 24 ]; then
                die "Ubuntu 24.04 or newer is required (regency needs glibc 2.39)."
            fi ;;
        *" debian "*)
            FAMILY=deb; DISTRO=debian
            # testing/unstable have no VERSION_ID
            if [ "$OS_ID" = debian ] && [ -n "${VERSION_ID:-}" ] && [ "$OS_MAJOR" -lt 13 ]; then
                die "Debian 13 or newer is required (regency needs glibc 2.39)."
            fi ;;
        *" fedora "*)
            if [ "$OS_ID" = fedora ]; then
                FAMILY=rpm; DISTRO=fedora
                [ "$OS_MAJOR" -ge 40 ] || die "Fedora 40 or newer is required."
                warn "Fedora: CouchDB has no Fedora packages, its RHEL 10 ones are used (best effort)."
            else
                FAMILY=rpm; DISTRO=el
                [ "$OS_MAJOR" -ge 10 ] || die "RHEL 10 or a compatible release is required (glibc 2.39)."
            fi ;;
        *" rhel "*|*" centos "*)
            FAMILY=rpm; DISTRO=el
            [ "$OS_MAJOR" -ge 10 ] || die "RHEL 10 or a compatible release is required (glibc 2.39)." ;;
        *) die "unsupported distribution '$OS_ID' (supported: Debian, Ubuntu, RHEL-compatible, Fedora)." ;;
    esac
    [ "$FAMILY" = rpm ] || [ -n "$OS_CODENAME" ] || die "cannot find the distribution's codename."
}

setup_privileges() {
    SUDO=""
    if [ "$(id -u)" -ne 0 ]; then
        have sudo || die "run this script as root, or install sudo."
        SUDO=sudo
        say "Root privileges are needed: you may be asked for your password (sudo)."
        sudo -v || die "sudo failed."
    fi
}

# ---------------------------------------------------------------- packages

pkg_installed() {
    if [ "$FAMILY" = deb ]; then
        dpkg-query -W -f='${Status}' "$1" 2>/dev/null | grep -q 'ok installed'
    else
        rpm -q "$1" >/dev/null 2>&1
    fi
}

apt_get() {
    as_root env DEBIAN_FRONTEND=noninteractive apt-get -q -y \
        -o Dpkg::Options::=--force-confdef -o Dpkg::Options::=--force-confold "$@"
}

install_prerequisites() {
    say "Installing prerequisites"
    if [ "$FAMILY" = deb ]; then
        apt_get update >/dev/null
        apt_get install curl gnupg ca-certificates apt-transport-https debconf-utils >/dev/null
    else
        as_root dnf -q -y install curl ca-certificates logrotate >/dev/null
    fi
}

# apt key from <url> to <keyring>
apt_key() {
    curl -fsSL "$1" | gpg --dearmor > "$TMP/key.gpg" || die "cannot download the signing key $1"
    write_file "$2" 0644 < "$TMP/key.gpg"
    rm -f "$TMP/key.gpg"
}

# ---------------------------------------------------------------- RabbitMQ

install_rabbitmq() {
    if pkg_installed rabbitmq-server; then
        RABBITMQ_FRESH=0
        say "RabbitMQ is already installed: reusing it (configuration left as is)"
    else
        RABBITMQ_FRESH=1
        say "Installing RabbitMQ (Team RabbitMQ repositories)"
        if [ "$FAMILY" = deb ]; then install_rabbitmq_deb; else install_rabbitmq_rpm; fi
        # localhost only; streams advertised as localhost (the services reach
        # them there); D3's partition handling (no-op on one node)
        write_file /etc/rabbitmq/conf.d/90-regency.conf 0644 <<'EOF'
# Written by regency's installer: everything on this host, localhost only.
# To let runners/hosts elsewhere connect, change the addresses below.
listeners.tcp.default = 127.0.0.1:5672
management.tcp.ip = 127.0.0.1
stream.listeners.tcp.1 = 127.0.0.1:5552
stream.advertised_host = localhost
cluster_partition_handling = pause_minority
EOF
    fi
    as_root systemctl enable rabbitmq-server >/dev/null 2>&1
    if [ "$RABBITMQ_FRESH" = 1 ]; then
        rmq rabbitmq-plugins --offline enable rabbitmq_management rabbitmq_stream \
            rabbitmq_stream_management >/dev/null
        as_root systemctl restart rabbitmq-server   # new configuration
    else
        as_root systemctl start rabbitmq-server
    fi
    wait_for 120 rmq rabbitmqctl -q await_startup ||
        die "RabbitMQ did not start: journalctl -u rabbitmq-server"
    # online, no restart (a no-op for plugins already enabled)
    rmq rabbitmq-plugins -q enable rabbitmq_management rabbitmq_stream \
        rabbitmq_stream_management >/dev/null
    if [ "$RABBITMQ_FRESH" = 1 ]; then
        # only reachable from localhost anyway, but regency refuses it (D19)
        rmq rabbitmqctl -q delete_user guest >/dev/null 2>&1 || true
    fi
}

install_rabbitmq_deb() {
    # Ubuntu: one repository for every release since 24.04 (noble);
    # Debian: trixie's own Erlang is recent enough.
    if [ "$DISTRO" = ubuntu ]; then rcode=noble; else rcode=trixie; fi
    keyring=/usr/share/keyrings/com.rabbitmq.team.gpg
    apt_key "https://keys.openpgp.org/vks/v1/by-fingerprint/0A9AF2115F4687BD29803A206B73A36E6026DFCA" "$keyring"
    {
        echo "## Team RabbitMQ (written by regency's installer)"
        if [ "$DISTRO" = ubuntu ]; then
            for m in deb1 deb2; do
                echo "deb [arch=amd64 signed-by=$keyring] https://$m.rabbitmq.com/rabbitmq-erlang/ubuntu/$rcode $rcode main"
            done
        fi
        for m in deb1 deb2; do
            echo "deb [arch=amd64 signed-by=$keyring] https://$m.rabbitmq.com/rabbitmq-server/$DISTRO/$rcode $rcode main"
        done
    } | write_file /etc/apt/sources.list.d/rabbitmq.list 0644
    apt_get update >/dev/null
    apt_get install erlang-base erlang-asn1 erlang-crypto erlang-eldap erlang-ftp \
        erlang-inets erlang-mnesia erlang-os-mon erlang-parsetools erlang-public-key \
        erlang-runtime-tools erlang-snmp erlang-ssl erlang-syntax-tools erlang-tftp \
        erlang-tools erlang-xmerl >/dev/null
    apt_get install --fix-missing rabbitmq-server >/dev/null
}

install_rabbitmq_rpm() {
    keys="https://github.com/rabbitmq/signing-keys/releases/download/3.0"
    for k in rabbitmq-release-signing-key.asc cloudsmith.rabbitmq-erlang.E495BB49CC4BBE5B.key \
             cloudsmith.rabbitmq-server.9F4587F226208342.key; do
        as_root rpm --import "$keys/$k"
    done
    {
        echo "# Team RabbitMQ (written by regency's installer); el9 packages serve el10 and Fedora"
        for repo in "modern-erlang erlang \$basearch $keys/cloudsmith.rabbitmq-erlang.E495BB49CC4BBE5B.key" \
                    "modern-erlang-noarch erlang noarch $keys/cloudsmith.rabbitmq-erlang.E495BB49CC4BBE5B.key" \
                    "rabbitmq-el9 rabbitmq \$basearch $keys/cloudsmith.rabbitmq-server.9F4587F226208342.key $keys/rabbitmq-release-signing-key.asc" \
                    "rabbitmq-el9-noarch rabbitmq noarch $keys/cloudsmith.rabbitmq-server.9F4587F226208342.key $keys/rabbitmq-release-signing-key.asc"; do
            # shellcheck disable=SC2086
            set -- $repo
            id="$1"; path="$2"; arch="$3"; shift 3
            cat <<EOF
[$id]
name=$id
baseurl=https://yum1.rabbitmq.com/$path/el/9/$arch
        https://yum2.rabbitmq.com/$path/el/9/$arch
repo_gpgcheck=1
enabled=1
gpgkey=$*
gpgcheck=1
sslverify=1
sslcacert=/etc/pki/tls/certs/ca-bundle.crt
metadata_expire=300
pkg_gpgcheck=1
autorefresh=1
type=rpm-md

EOF
        done
    } | write_file /etc/yum.repos.d/rabbitmq.repo 0644
    as_root dnf -q -y install erlang rabbitmq-server >/dev/null
}

configure_rabbitmq_admin() {
    pw="$(env_get RABBITMQ_ADMIN_PASSWORD)"
    user="$(env_get RABBITMQ_ADMIN_USER)"
    user="${user:-$RABBITMQ_ADMIN}"
    [ -n "$pw" ] || pw="$(random_password)"
    wait_for 60 curl -fsS -o /dev/null http://127.0.0.1:15672/ ||
        die "RabbitMQ's management API does not answer on 127.0.0.1:15672."
    if ! printf 'user = "%s:%s"\n' "$user" "$pw" | curl_auth http://127.0.0.1:15672/api/whoami; then
        say "Creating the RabbitMQ admin account '$user'"
        rmq rabbitmqctl -q delete_user "$user" >/dev/null 2>&1 || true
        # rabbitmqctl cannot read it from stdin through the packages' wrapper:
        # on the command line for the duration of this call
        rmq rabbitmqctl -q add_user "$user" "$pw" >/dev/null
        rmq rabbitmqctl -q set_user_tags "$user" administrator >/dev/null
        rmq rabbitmqctl -q set_permissions -p / "$user" '.*' '.*' '.*' >/dev/null
        printf 'user = "%s:%s"\n' "$user" "$pw" | curl_auth http://127.0.0.1:15672/api/whoami ||
            die "the RabbitMQ admin account '$user' does not work."
    fi
    # saved at once: a later failure must not lose them
    env_set RABBITMQ_ADMIN_USER "$user"
    env_set RABBITMQ_ADMIN_PASSWORD "$pw"
}

# ---------------------------------------------------------------- CouchDB

couchdb_auth_ok() { # <user> <password>
    printf 'user = "%s:%s"\n' "$1" "$2" | curl_auth http://127.0.0.1:5984/_node/_local/_config/couchdb
}

install_couchdb() {
    COUCHDB_USER="$(env_get COUCHDB_ADMIN_USER)"
    COUCHDB_PASSWORD="$(env_get COUCHDB_ADMIN_PASSWORD)"
    if pkg_installed couchdb; then
        COUCHDB_FRESH=0
        say "CouchDB is already installed: reusing it (configuration left as is)"
        if [ -z "$COUCHDB_PASSWORD" ]; then
            # an existing admin given by the user, or our own account
            COUCHDB_USER="${COUCHDB_ADMIN_USER:-regency-admin}"
            COUCHDB_PASSWORD="${COUCHDB_ADMIN_PASSWORD:-$(random_password)}"
        fi
        as_root systemctl enable --now couchdb >/dev/null 2>&1
    else
        COUCHDB_FRESH=1
        COUCHDB_USER="${COUCHDB_USER:-admin}"
        COUCHDB_PASSWORD="${COUCHDB_PASSWORD:-$(random_password)}"
        say "Installing CouchDB (Apache repositories)"
        if [ "$FAMILY" = deb ]; then install_couchdb_deb; else install_couchdb_rpm; fi
        write_file /opt/couchdb/etc/local.d/90-regency.ini 0640 couchdb:couchdb <<'EOF'
; Written by regency's installer: one node, localhost only.
[couchdb]
single_node = true

[chttpd]
bind_address = 127.0.0.1
EOF
        as_root systemctl enable couchdb >/dev/null 2>&1
        as_root systemctl restart couchdb
    fi
    wait_for 90 curl -fsS -o /dev/null http://127.0.0.1:5984/_up ||
        die "CouchDB does not answer on 127.0.0.1:5984: journalctl -u couchdb"

    if ! couchdb_auth_ok "$COUCHDB_USER" "$COUCHDB_PASSWORD"; then
        if [ "$COUCHDB_FRESH" = 0 ] && [ -n "${COUCHDB_ADMIN_PASSWORD:-}" ]; then
            die "COUCHDB_ADMIN_USER/COUCHDB_ADMIN_PASSWORD are refused by CouchDB."
        fi
        say "Creating the CouchDB admin account '$COUCHDB_USER'"
        # CouchDB hashes it at start and writes the hash back to this file
        printf '; regency admin account (regency installer)\n[admins]\n%s = %s\n' \
            "$COUCHDB_USER" "$COUCHDB_PASSWORD" |
            write_file /opt/couchdb/etc/local.d/95-regency-admin.ini 0640 couchdb:couchdb
        as_root systemctl restart couchdb
        wait_for 90 couchdb_auth_ok "$COUCHDB_USER" "$COUCHDB_PASSWORD" ||
            die "the CouchDB admin account '$COUCHDB_USER' does not work: journalctl -u couchdb"
    fi
    env_set COUCHDB_ADMIN_USER "$COUCHDB_USER"
    env_set COUCHDB_ADMIN_PASSWORD "$COUCHDB_PASSWORD"
    # single node: the system databases (`_users`...) must exist
    for db in _users _replicator; do
        printf 'user = "%s:%s"\n' "$COUCHDB_USER" "$COUCHDB_PASSWORD" |
            curl -sS -o /dev/null --max-time 10 -K - -X PUT "http://127.0.0.1:5984/$db" || true
    done
}

install_couchdb_deb() {
    keyring=/usr/share/keyrings/couchdb-archive-keyring.gpg
    apt_key https://couchdb.apache.org/repo/keys.asc "$keyring"
    # the repository may not know a brand-new release yet
    ccode="$OS_CODENAME"
    if ! curl -fsS -o /dev/null "https://apache.jfrog.io/artifactory/couchdb-deb/dists/$ccode/Release"; then
        if [ "$DISTRO" = ubuntu ]; then ccode=noble; else ccode=trixie; fi
        warn "CouchDB has no packages for '$OS_CODENAME' yet: using '$ccode'."
    fi
    echo "deb [signed-by=$keyring] https://apache.jfrog.io/artifactory/couchdb-deb/ $ccode main" |
        write_file /etc/apt/sources.list.d/couchdb.list 0644
    # answers to the package's questions (password on stdin; the package
    # clears it from debconf after use)
    cookie="$(random_password)"
    cat <<EOF | as_root debconf-set-selections
couchdb couchdb/mode select standalone
couchdb couchdb/bindaddress string 127.0.0.1
couchdb couchdb/cookie string $cookie
couchdb couchdb/adminpass password $COUCHDB_PASSWORD
couchdb couchdb/adminpass_again password $COUCHDB_PASSWORD
EOF
    apt_get update >/dev/null
    apt_get install couchdb >/dev/null
}

install_couchdb_rpm() {
    if [ "$DISTRO" = fedora ]; then el=10; else el="$OS_MAJOR"; fi
    write_file /etc/yum.repos.d/couchdb.repo 0644 <<EOF
# Apache CouchDB (written by regency's installer)
[couchdb]
name=couchdb
baseurl=https://apache.jfrog.io/artifactory/couchdb-rpm/el$el/\$basearch/
gpgkey=https://couchdb.apache.org/repo/keys.asc https://couchdb.apache.org/repo/rpm-package-key.asc
gpgcheck=1
repo_gpgcheck=1
enabled=1
EOF
    as_root dnf -q -y install couchdb >/dev/null
    # CouchDB 3 does not start without an admin
    printf '; regency admin account (regency installer)\n[admins]\n%s = %s\n' \
        "$COUCHDB_USER" "$COUCHDB_PASSWORD" |
        write_file /opt/couchdb/etc/local.d/95-regency-admin.ini 0640 couchdb:couchdb
}

# ---------------------------------------------------------------- regency

download_package() {
    if [ -n "$PACKAGE" ]; then
        [ -f "$PACKAGE" ] || die "no such package: $PACKAGE"
        PKG_FILE="$PACKAGE"
        return
    fi
    if [ -n "$VERSION" ]; then
        api="https://api.github.com/repos/$REPO/releases/tags/v${VERSION#v}"
    else
        api="https://api.github.com/repos/$REPO/releases/latest"
    fi
    curl -fsSL -H 'Accept: application/vnd.github+json' "$api" -o "$TMP/release.json" ||
        die "cannot read the release from GitHub ($api)."
    if [ "$FAMILY" = deb ]; then pattern='/regency-all-in-one_[^/]*_amd64\.deb$'
    else pattern='/regency-all-in-one-[^/]*\.x86_64\.rpm$'; fi
    urls="$(grep -o '"browser_download_url": *"[^"]*"' "$TMP/release.json" |
        sed 's/.*"\(https:[^"]*\)"$/\1/')"
    url="$(printf '%s\n' "$urls" | grep "$pattern" | head -n 1 || true)"
    [ -n "$url" ] || die "the release has no regency-all-in-one package for this system."
    name="${url##*/}"
    say "Downloading $name"
    PKG_FILE="$TMP/$name"
    curl -fL --progress-bar "$url" -o "$PKG_FILE" || die "download failed: $url"
    sums="$(printf '%s\n' "$urls" | grep '/SHA256SUMS$' | head -n 1 || true)"
    if [ -n "$sums" ]; then
        curl -fsSL "$sums" -o "$TMP/SHA256SUMS" || die "cannot download $sums"
        expected="$(awk -v n="$name" '$2 == n || $2 == "*" n { print $1 }' "$TMP/SHA256SUMS")"
        actual="$(sha256sum "$PKG_FILE" | cut -d ' ' -f 1)"
        [ -n "$expected" ] && [ "$expected" = "$actual" ] ||
            die "checksum mismatch for $name (expected '$expected', got '$actual')."
    else
        warn "the release publishes no SHA256SUMS: package not checksummed."
    fi
}

install_regency() {
    download_package
    say "Installing regency-all-in-one"
    if [ "$FAMILY" = deb ]; then
        # apt reads it as user _apt: a copy in a directory it can enter
        PKG_DIR="$(as_root mktemp -d)"
        as_root chmod 0755 "$PKG_DIR"
        as_root install -m 0644 "$PKG_FILE" "$PKG_DIR/${PKG_FILE##*/}"
        apt_get install "$PKG_DIR/${PKG_FILE##*/}" >/dev/null
    else
        case "$PKG_FILE" in /*) ;; *) PKG_FILE="$(pwd)/$PKG_FILE" ;; esac
        as_root dnf -q -y install "$PKG_FILE" >/dev/null
    fi
    as_root test -f "$ENV_FILE" || die "$ENV_FILE is missing after installation."
    # what start_regency compares against (restart only when settings change)
    as_root cat "$ENV_FILE" > "$TMP/env.before"
}

configure_regency() {
    say "Writing the RabbitMQ and CouchDB addresses to $ENV_FILE"
    env_set REGENCY_RABBITMQ_URL "amqp://localhost:5672"
    env_set REGENCY_RABBITMQ_STREAM_HOST "localhost"
    env_set RABBITMQ_MANAGEMENT_URL "http://localhost:15672"
    env_set REGENCY_COUCHDB_URL "http://localhost:5984"
    if [ -n "$ADMIN_PASSWORD" ]; then
        if as_root test -e "$SECRETS_DIR/bootstrap-admin"; then
            warn "regency was already initialized: --admin-password ignored (change it in the web UI)."
        else
            env_set REGENCY_ADMIN_PASSWORD "$ADMIN_PASSWORD"
        fi
    fi
    as_root chown root:regency "$ENV_FILE"
    as_root chmod 0640 "$ENV_FILE"
    if as_root cat "$ENV_FILE" | cmp -s - "$TMP/env.before"; then ENV_CHANGED=0; else ENV_CHANGED=1; fi
    rm -f "$TMP/env.before"
}

start_regency() {
    if as_root test -e "$SECRETS_DIR/bootstrap-admin"; then FIRST_START=0; else FIRST_START=1; fi
    as_root systemctl enable regency-all-in-one >/dev/null 2>&1
    if as_root systemctl is-active --quiet regency-all-in-one; then
        if [ "$ENV_CHANGED" = 1 ]; then
            say "Restarting regency (new settings)"
            as_root systemctl restart regency-all-in-one
        fi
    else
        say "Starting regency"
        as_root systemctl start regency-all-in-one
    fi
    if [ "$FIRST_START" = 1 ]; then
        say "Waiting for regency to be ready (first start: accounts, queues, databases)..."
    else
        say "Waiting for regency to be ready..."
    fi
    if ! wait_for 300 curl -fsS -o /dev/null http://127.0.0.1:9094/readyz; then
        as_root journalctl -u regency-all-in-one -n 30 --no-pager >&2 || true
        die "regency is not ready after 5 minutes (log above; full: journalctl -u regency-all-in-one)."
    fi
    # applied at first start (kept in $SECRETS_DIR): no copy left in clear
    if [ -n "$(env_get REGENCY_ADMIN_PASSWORD)" ] && as_root test -e "$SECRETS_DIR/bootstrap-admin"; then
        env_unset REGENCY_ADMIN_PASSWORD
    fi
}

summary() {
    host="$(hostname -f 2>/dev/null || hostname)"
    echo
    say "regency is running."
    echo
    echo "  Web UI:       http://$host:8080   (user: admin)"
    if [ "$FIRST_START" = 1 ]; then
        if [ -n "$ADMIN_PASSWORD" ]; then
            echo "  Password:     the one you chose (--admin-password)"
        else
            pw="$(as_root cat "$SECRETS_DIR/bootstrap-admin" 2>/dev/null || true)"
            echo "  Password:     ${BOLD}$pw${RESET}   (shown once: change it after logging in)"
        fi
    else
        echo "  Password:     unchanged (initial one in $SECRETS_DIR/bootstrap-admin)"
    fi
    echo "  regops agents: https://$host:8443 (built-in CA: /var/lib/regency/tls/ca.pem)"
    echo
    echo "  Logs:         journalctl -u regency-all-in-one -f"
    echo "  Settings:     $ENV_FILE, /etc/regency/<service>.toml"
    echo "  Docs:         https://github.com/$REPO"
    echo
    if have firewall-cmd && as_root firewall-cmd --state >/dev/null 2>&1; then
        echo "  firewalld is active: to reach regency from other machines,"
        echo "    sudo firewall-cmd --permanent --add-port=8080/tcp --add-port=8443/tcp && sudo firewall-cmd --reload"
        echo
    fi
    warn "this is a single-node setup at security level 1 (no TLS on port 8080):"
    echo "         for trials and labs; keep ports 8080/8443 on a trusted network." >&2
}

# ---------------------------------------------------------------- main

main() {
    VERSION="${REGENCY_VERSION:-}"
    PACKAGE="${REGENCY_PACKAGE:-}"
    ADMIN_PASSWORD="${REGENCY_ADMIN_PASSWORD:-}"
    START=1
    while [ $# -gt 0 ]; do
        case "$1" in
            --version) [ $# -ge 2 ] || die "--version needs a value"; VERSION="$2"; shift 2 ;;
            --version=*) VERSION="${1#*=}"; shift ;;
            --package) [ $# -ge 2 ] || die "--package needs a value"; PACKAGE="$2"; shift 2 ;;
            --package=*) PACKAGE="${1#*=}"; shift ;;
            --admin-password) [ $# -ge 2 ] || die "--admin-password needs a value"; ADMIN_PASSWORD="$2"; shift 2 ;;
            --admin-password=*) ADMIN_PASSWORD="${1#*=}"; shift ;;
            --no-start) START=0; shift ;;
            -h|--help) usage; exit 0 ;;
            *) die "unknown option: $1 (see --help)" ;;
        esac
    done
    case "$ADMIN_PASSWORD" in
        *"'"*|*'"'*|*'\'*) die "the admin password must not contain quotes or backslashes." ;;
    esac
    if [ -n "$ADMIN_PASSWORD" ] && [ "${#ADMIN_PASSWORD}" -lt 8 ]; then
        die "the admin password must have at least 8 characters."
    fi
    have curl || die "curl is required."

    detect_system
    setup_privileges
    # files created for RabbitMQ/CouchDB must be readable by their services
    # (through sudo, a stricter umask of the user would apply too)
    umask 022
    TMP="$(mktemp -d)"   # 0700: holds credentials while they are written
    PKG_DIR=""
    trap 'status=$?; rm -rf "$TMP"; [ -z "$PKG_DIR" ] || as_root rm -rf "$PKG_DIR"
        [ "$status" = 0 ] || printf "%sInstallation failed (above). Fix the cause and run the script again: it resumes.%s\n" "$RED" "$RESET" >&2' EXIT
    trap 'exit 130' INT TERM

    say "Installing regency all-in-one on ${PRETTY_NAME:-$OS_ID}"
    install_prerequisites
    # first: its env file keeps the credentials created below
    install_regency
    install_rabbitmq
    configure_rabbitmq_admin
    install_couchdb
    configure_regency
    if [ "$START" = 0 ]; then
        say "Installed. Start it with: sudo systemctl enable --now regency-all-in-one"
        return
    fi
    start_regency
    summary
}

main "$@"
