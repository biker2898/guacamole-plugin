#!/usr/bin/env bash
#
# Installs the extensions in this repository into an Apache Guacamole
# deployment that runs under Docker Compose:
#
#   session-tools  ping meter + fullscreen button on the session page
#   branding       hides the Guacamole look (name, logo, version, favicon)
#
# Usage:
#   ./install.sh [options] [compose-dir]     (default compose-dir: ~/docker/guacamole)
#
# Options:
#   --no-branding   install only session-tools; removes branding if present
#   --uninstall     remove both extensions
#
# Environment:
#   BRAND     name shown instead of "Guacamole" (default: Portal)
#   SERVICE   name of the Guacamole web app service (default: guacamole)
#
# What it does:
#   1. Packages session-tools/ into session-tools.jar. Builds branding.jar
#      from branding/ and the UI texts of the installed Guacamole.
#   2. Finds the host folder mounted as the container's GUACAMOLE_HOME. If
#      none is mounted, it mounts <compose-dir>/home through
#      docker-compose.override.yml, leaving docker-compose.yml untouched.
#   3. Copies the jars into <that folder>/extensions/, recreates the Guacamole
#      container and checks the log that the extensions loaded.
#
# Run it again after editing anything here or upgrading Guacamole.
#
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
SERVICE=${SERVICE:-guacamole}
BRAND=${BRAND:-Portal}
# Location of the web app inside the official guacamole/guacamole image
WAR_PATH=/opt/guacamole/webapp/guacamole.war

BRANDING=true
UNINSTALL=false
COMPOSE_ARG=
for arg in "$@"; do
    case "$arg" in
        --no-branding) BRANDING=false ;;
        --uninstall)   UNINSTALL=true ;;
        -h|--help)     awk 'NR > 1 && !/^#/ { exit } NR > 1' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        -*)            echo "error: unknown option $arg" >&2; exit 1 ;;
        *)             COMPOSE_ARG=$arg ;;
    esac
done
COMPOSE_DIR=$(cd "${COMPOSE_ARG:-$HOME/docker/guacamole}" && pwd)

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

# Recreates the container and waits until each named extension is loaded
restart_and_check() {
    local since
    since=$(date -u +%Y-%m-%dT%H:%M:%SZ)
    docker compose up -d --force-recreate "$SERVICE"
    [ $# -eq 0 ] && return 0

    info "waiting for Guacamole to load: $*"
    local logs pending
    for _ in $(seq 1 60); do
        logs=$(docker compose logs --no-color --since "$since" "$SERVICE" 2>&1)
        pending=()
        for name in "$@"; do
            grep -q "Extension \"[^\"]*\" ($name) loaded" <<<"$logs" || pending+=("$name")
        done
        if [ ${#pending[@]} -eq 0 ]; then
            info "loaded. Reload the Guacamole page with Ctrl+F5."
            return 0
        fi
        for name in "${pending[@]}"; do
            if grep -qi "extension.*$name.*\(fail\|error\|incompatible\)" <<<"$logs"; then
                grep -i "$name" <<<"$logs" >&2
                die "Guacamole rejected the $name extension"
            fi
        done
        sleep 2
    done
    die "not reported as loaded after 120s: ${pending[*]}; check: docker compose -f $COMPOSE_DIR/docker-compose.yml logs $SERVICE"
}

HOME_DIR=$(find_home_mount)

if $UNINSTALL; then
    [ -n "$HOME_DIR" ] || die "no GUACAMOLE_HOME mount found; nothing to remove"
    removed=false
    for name in session-tools branding; do
        if [ -f "$HOME_DIR/extensions/$name.jar" ]; then
            rm -f "$HOME_DIR/extensions/$name.jar"
            info "removed $HOME_DIR/extensions/$name.jar"
            removed=true
        fi
    done
    $removed || die "no extensions from this repository found in $HOME_DIR/extensions"
    restart_and_check
    info "done"
    exit 0
fi

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
EXT_DIR=$HOME_DIR/extensions
mkdir -p "$EXT_DIR"

# session-tools: a jar is a zip with the manifest at its root
python3 - "$SCRIPT_DIR/session-tools" "$EXT_DIR/session-tools.jar.tmp" <<'EOF'
import os, sys, zipfile
src, out = sys.argv[1], sys.argv[2]
with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED) as jar:
    for root, _, files in os.walk(src):
        for name in sorted(files):
            path = os.path.join(root, name)
            jar.write(path, os.path.relpath(path, src))
EOF
mv "$EXT_DIR/session-tools.jar.tmp" "$EXT_DIR/session-tools.jar"
info "built $EXT_DIR/session-tools.jar"
EXTENSIONS=(session-tools)

# branding: texts come from the web app inside the image
if $BRANDING; then
    TMP=$(mktemp -d)
    trap 'rm -rf "$TMP"' EXIT
    docker compose create "$SERVICE" >/dev/null 2>&1 || true
    docker compose cp "$SERVICE:$WAR_PATH" "$TMP/guacamole.war" >/dev/null 2>&1 \
        || die "could not copy $WAR_PATH from the '$SERVICE' container; is it the official guacamole/guacamole image?"
    python3 "$SCRIPT_DIR/branding/build.py" "$TMP/guacamole.war" "$EXT_DIR/branding.jar" "$BRAND"
    EXTENSIONS+=(branding)
elif [ -f "$EXT_DIR/branding.jar" ]; then
    rm -f "$EXT_DIR/branding.jar"
    info "removed $EXT_DIR/branding.jar"
fi

# Guacamole reads extensions at startup; recreate so a new mount takes effect
restart_and_check "${EXTENSIONS[@]}"
