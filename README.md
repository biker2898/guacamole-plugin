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
- **Keys button**: opens a panel with F1–F12, Del, Ctrl+Alt+Del and Ctrl,
  Alt and Shift toggles, for keys the browser or device cannot send. A
  toggled Ctrl, Alt or Shift stays pressed until the next other key (from the
  panel or the physical keyboard) or until tapped again.
- **Clipboard button**: opens a text box for browsers where Guacamole's
  automatic clipboard sync does not work, such as on Android. Paste text into
  the box (or tap "From device"), then "Send" it to the remote clipboard,
  "Send + paste" to also press Ctrl+V, or "Type" it key by key where pasting
  is blocked (for example the Windows login screen). Text copied on the
  remote desktop appears in the box; "Copy to device" copies it onto this
  device. The "Auto-send what I copy on this device" switch sends text
  copied on the device to the remote clipboard by itself, ready for Ctrl+V.
  It works in browsers that grant lasting clipboard access (Chrome, Edge,
  Chrome on Android), asks for that permission when switched on, and is
  remembered per device.
- **Settings button**: opens a dialog with two settings:
  - *Screen resolution*: "Fit browser window" (Guacamole's default) or a
    fixed 16:9 or 16:10 size such as 1920 × 1080 or 1280 × 800. Applying it
    reconnects the session. The choice is stored per Guacamole user on the
    server (see [Per-user settings](#per-user-settings)), so it follows the
    user to every browser and device. Without that service the session uses
    "Fit browser window". Connections that set their own `width` and
    `height` parameters ignore it.
  - *Change password*: changes the user's Guacamole password. The account
    needs the "change own password" permission (UPDATE on its own user);
    otherwise the server refuses the change.

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

## Per-user settings

Guacamole keeps no settings of its own for a non-admin user, so the chosen
resolution is stored by a small service in `prefs/`
(`session-tools-prefs.py`, Python standard library only). Each request
carries the user's Guacamole token; the service asks Guacamole whose token it
is and reads or writes only that user's settings, in
`/var/lib/session-tools-prefs/`.

Install it once on the Guacamole server (replace `SECRET_PATH` with the
`WEBAPP_CONTEXT` value, or `guacamole` if none is set):

```sh
sudo install -D -m 644 prefs/session-tools-prefs.py /usr/local/lib/session-tools-prefs/session-tools-prefs.py
sudo install -m 644 prefs/session-tools-prefs.service /etc/systemd/system/
echo 'GUACAMOLE_URL=http://127.0.0.1:8080/SECRET_PATH' | sudo tee /etc/default/session-tools-prefs
sudo systemctl daemon-reload && sudo systemctl enable --now session-tools-prefs
```

Then let nginx pass `<path>/session-tools/prefs` to it, as in
`examples/nginx/guacamole-locations.conf`, and reload nginx. After editing
`session-tools-prefs.py`, copy it again and run
`sudo systemctl restart session-tools-prefs`.

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

## Shared drive for RDP (optional)

RDP connections can map a folder on the Guacamole server as a drive in the
remote Windows session, for file transfer in both directions.

1. Mount a folder into the `guacd` service in `docker-compose.yml`:

   ```yaml
   guacd:
     image: guacamole/guacd:1.6.0
     restart: unless-stopped
     volumes:
       - ./drive:/drive
   ```

   `guacd` runs as UID 1000, so that UID must be able to write the folder:

   ```sh
   cd ~/docker/guacamole
   mkdir -p drive/shared && sudo chown -R 1000:1000 drive
   docker compose up -d
   ```

2. In **Settings → Connections → (the RDP connection) → Device Redirection**:

   | Field                 | Value                                  |
   |-----------------------|----------------------------------------|
   | Enable drive          | checked                                |
   | Drive name            | a neutral name, e.g. `WORKDRIVE`       |
   | Drive path            | `/drive/shared`                        |
   | Automatically create drive | checked                           |

`/drive/shared` is one folder shared by every user of the connection. For a
private folder per Guacamole user, use `/drive/${GUAC_USERNAME}` instead.
Avoid the drive name `Guacamole` when using the branding extension: it shows
up in Windows Explorer and gives the product away.

## Files

- `session-tools/`: ping meter, fullscreen, keys, clipboard and settings extension (manifest, JS, CSS).
- `branding/`: branding extension sources and `build.py`, which builds the jar.
- `prefs/`: per-user settings service and its systemd unit.
- `install.sh`: builds and installs both extensions.
- `examples/nginx/`: optional nginx hardening.
