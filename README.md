# Guacamole Session Tools

A small Apache Guacamole extension that adds a floating, draggable widget to
every remote session:

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

## Files

- `session-tools/guac-manifest.json`: extension manifest. `"guacamoleVersion": "*"`
  lets it load on any Guacamole version.
- `session-tools/session-tools.js`: widget logic.
- `session-tools/session-tools.css`: widget styling.
- `install.sh`: packages the extension and installs it into a Docker Compose
  deployment.

## Install or update

Run on the Guacamole server:

```sh
./install.sh                     # compose project in ~/docker/guacamole
./install.sh /path/to/compose    # another location
SERVICE=web ./install.sh         # if the web app service is not "guacamole"
```

The script copies the jar into the folder mounted as the container's
`GUACAMOLE_HOME` (`/etc/guacamole`). If nothing is mounted there, it adds the
mount in `docker-compose.override.yml`. It then recreates the container and
checks the log for `Extension "Session Tools" (session-tools) loaded.`

After editing anything in `session-tools/`, run `./install.sh` again and reload
the Guacamole page with Ctrl+F5.

## Uninstall

```sh
./install.sh --uninstall
```
