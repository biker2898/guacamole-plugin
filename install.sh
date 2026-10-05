#!/usr/bin/env bash
#
# Installs the "Session Tools" extension (ping meter + fullscreen button) into
# an Apache Guacamole deployment that runs under Docker Compose.
#
# Usage:
#   ./install.sh [compose-dir]              install or update (default: ~/docker/guacamole)
#   ./install.sh --uninstall [compose-dir]  remove the extension
#
# Environment:
#   SERVICE   name of the Guacamole web app service (default: guacamole)
#
# What it does:
#   1. Packages ./session-tools into session-tools.jar.
#   2. Finds the host folder mounted as the container's GUACAMOLE_HOME. If
#      none is mounted, it mounts <compose-dir>/home through
#      docker-compose.override.yml, leaving docker-compose.yml untouched.
#   3. Copies the jar into <that folder>/extensions/ and recreates the
#      Guacamole container, then checks the log that the extension loaded.
#
# Run it again after editing anything in ./session-tools.
#
set -euo pipefail

NAME=session-tools
DISPLAY_NAME="Session Tools"
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
SRC_DIR=$SCRIPT_DIR/$NAME
SERVICE=${SERVICE:-guacamole}

UNINSTALL=false
if [ "${1:-}" = "--uninstall" ]; then
    UNINSTALL=true
    shift
fi
COMPOSE_DIR=$(cd "${1:-$HOME/docker/guacamole}" && pwd)

die() { echo "error: $*" >&2; exit 1; }
info() { echo "==> $*"; }

command -v docker >/dev/null || die "docker not found"
command -v python3 >/dev/null || die "python3 not found"
cd "$COMPOSE_DIR"
docker compose config --services 2>/dev/null | grep -qx "$SERVICE" \
    || die "no service '$SERVICE' in a compose project at $COMPOSE_DIR (set SERVICE=...)"

# Prints the host folder bound to the container's GUACAMOLE_HOME, or nothing
find_home_mount() {
    docker compose config --format json | python3 -c '
import json, sys
service = json.load(sys.stdin)["services"][sys.argv[1]]
env = service.get("environment") or {}
if isinstance(env, list):
    env = dict(item.split("=", 1) for item in env if "=" in item)
target = (env.get("GUACAMOLE_HOME") or "/etc/guacamole").rstrip("/")
for volume in service.get("volumes") or []:
    if volume.get("type") == "bind" and volume.get("target", "").rstrip("/") == target:
        print(volume["source"])
' "$SERVICE"
}

HOME_DIR=$(find_home_mount)

if $UNINSTALL; then
    [ -n "$HOME_DIR" ] || die "no GUACAMOLE_HOME mount found; nothing to remove"
    JAR=$HOME_DIR/extensions/$NAME.jar
    [ -f "$JAR" ] || die "$JAR not found; nothing to remove"
    rm -f "$JAR"
    info "removed $JAR"
    docker compose restart "$SERVICE"
    info "done"
    exit 0
fi

[ -f "$SRC_DIR/guac-manifest.json" ] || die "$SRC_DIR/guac-manifest.json not found"

# Mount <compose-dir>/home as GUACAMOLE_HOME when nothing is mounted there yet
if [ -z "$HOME_DIR" ]; then
    OVERRIDE=$COMPOSE_DIR/docker-compose.override.yml
    [ -e "$OVERRIDE" ] && die "$OVERRIDE already exists; mount a folder as GUACAMOLE_HOME (/etc/guacamole) for '$SERVICE' yourself and re-run"
    info "mounting $COMPOSE_DIR/home as GUACAMOLE_HOME via $OVERRIDE"
    cat > "$OVERRIDE" <<EOF
# Added by $SCRIPT_DIR/install.sh: mounts a template GUACAMOLE_HOME so
# extensions in ./home/extensions are loaded.
services:
  $SERVICE:
    volumes:
      - ./home:/etc/guacamole:ro
EOF
    HOME_DIR=$(find_home_mount)
    [ -n "$HOME_DIR" ] || die "mount not picked up; check $OVERRIDE"
fi

# Package the extension; a jar is a zip with the manifest at its root
mkdir -p "$HOME_DIR/extensions"
JAR=$HOME_DIR/extensions/$NAME.jar
python3 - "$SRC_DIR" "$JAR.tmp" <<'EOF'
import os, sys, zipfile
src, out = sys.argv[1], sys.argv[2]
with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED) as jar:
    for root, _, files in os.walk(src):
        for name in sorted(files):
            path = os.path.join(root, name)
            jar.write(path, os.path.relpath(path, src))
EOF
mv "$JAR.tmp" "$JAR"
info "built $JAR"

# Recreate so a new mount takes effect; Guacamole reads extensions at startup
SINCE=$(date -u +%Y-%m-%dT%H:%M:%SZ)
docker compose up -d --force-recreate "$SERVICE"

info "waiting for Guacamole to load the extension"
for _ in $(seq 1 60); do
    LOGS=$(docker compose logs --no-color --since "$SINCE" "$SERVICE" 2>&1)
    if grep -q "Extension \"$DISPLAY_NAME\" ($NAME) loaded" <<<"$LOGS"; then
        info "loaded. Reload the Guacamole page with Ctrl+F5."
        exit 0
    fi
    if grep -qi "extension.*$NAME.*\(fail\|error\|incompatible\)" <<<"$LOGS"; then
        grep -i "$NAME" <<<"$LOGS" >&2
        die "Guacamole rejected the extension"
    fi
    sleep 2
done
die "extension not reported as loaded after 120s; check: docker compose -f $COMPOSE_DIR/docker-compose.yml logs $SERVICE"
