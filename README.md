# Guacamole Session Tools

Two small Apache Guacamole extensions, plus an install script for Guacamole
running under Docker Compose.

## session-tools

A floating, draggable widget on every remote session:

- **Ping meter**: round-trip time between the browser and the Guacamole
  server. It times the keep-alive pings that Guacamole's WebSocket tunnel
  already sends and the server echoes back, so it adds no traffic. It does not
  include the hop from the Guacamole server to the remote desktop.
  Green is under 100 ms, yellow is 100–250 ms, red is over 250 ms, and "lost"
  means no echo for 5 seconds.
- **Fullscreen button**: toggles browser fullscreen. In Chrome and Edge it
  also locks the keyboard, so Esc and Alt+Tab reach the remote desktop. It is
  hidden where the browser has no Fullscreen API (Safari on iPhone).

The widget appears only on the session page (`#/client/...`). Its position is
remembered per browser.

## branding

Makes the login page and UI stop looking like Guacamole, so automated scanners
and casual visitors do not recognize it:

- Replaces "Apache Guacamole" / "Guacamole" with a neutral name (default
  "Portal") in every UI string of every language, including the page title.
- Hides the Guacamole logo and the version number on the login page.
- Replaces the Guacamole favicon with a plain one.

The texts are generated from the installed Guacamole, so re-run the install
script after upgrading Guacamole.

This is obscurity, not security: anyone who reaches the page can still read
its source. Combine it with the nginx setup below and strong passwords or
two-factor login.

## Install or update

Run on the Guacamole server:

```sh
./install.sh                         # compose project in ~/docker/guacamole
./install.sh /path/to/compose        # another location
BRAND="Remote Access" ./install.sh   # another name instead of "Portal"
./install.sh --no-branding           # session-tools only
SERVICE=web ./install.sh             # if the web app service is not "guacamole"
```

Requirements: Docker Compose, python3, and the official
`guacamole/guacamole` image.

The script copies the jars into the folder mounted as the container's
`GUACAMOLE_HOME` (`/etc/guacamole`). If nothing is mounted there, it adds the
mount in `docker-compose.override.yml`. It then recreates the container and
checks the log that each extension loaded.

After editing anything here, run `./install.sh` again and reload the Guacamole
page with Ctrl+F5.

## Uninstall

```sh
./install.sh --uninstall
```

## Hardening with nginx (optional)

`examples/nginx/` holds the nginx setup this was built with:

- Guacamole is served only under a secret path; `/`, `/guacamole/` and
  everything else return a plain 404. Set the path on the container with
  `WEBAPP_CONTEXT: <secret>` (for example `openssl rand -hex 8`) and replace
  `SECRET_PATH` in `guacamole-locations.conf` with the same value.
- Login requests (`<path>/api/tokens`) are limited to 10 per minute per IP.
- The nginx version is hidden.
- `X-Forwarded-For` carries only the real client IP. With
  `REMOTE_IP_VALVE_ENABLED: "true"` on the container, Guacamole's built-in ban
  extension then blocks the attacker, not a proxy. Tune it with
  `BAN_MAX_INVALID_ATTEMPTS` and `BAN_ADDRESS_DURATION` (seconds).
- `site-cloudflare.conf` takes the client IP from Cloudflare.

Publish the container port on localhost only (`127.0.0.1:8080:8080`).
Otherwise anyone can reach Guacamole directly and skip all of the above.

## Files

- `session-tools/`: ping meter and fullscreen extension (manifest, JS, CSS).
- `branding/`: branding extension sources and `build.py`, which builds the jar.
- `install.sh`: builds and installs both extensions.
- `examples/nginx/`: optional nginx hardening.
