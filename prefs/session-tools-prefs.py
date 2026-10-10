#!/usr/bin/env python3
"""
Per-user settings store for the session-tools extension.

Guacamole lets a non-admin account change its password but keeps no other
settings for it on the server, so the extension stores them here. Every
request carries the user's Guacamole token; this server asks Guacamole whose
token it is and reads or writes only that user's settings.

  GET /prefs?dataSource=postgresql    returns {"resolution": "1920x1080"}
  PUT /prefs?dataSource=postgresql    body {"resolution": "auto"}, merged in

Environment:
  GUACAMOLE_URL    Guacamole as reachable from this host, including its path
                   (default http://127.0.0.1:8080/guacamole)
  LISTEN           address:port to listen on (default 127.0.0.1:8081)
  STATE_DIRECTORY  folder for the settings files (set by systemd's
                   StateDirectory=; default ./prefs-data)

Only Python's standard library is used.
"""
import hashlib
import json
import os
import re
import sys
import tempfile
import urllib.error
import urllib.parse
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

GUACAMOLE_URL = os.environ.get('GUACAMOLE_URL', 'http://127.0.0.1:8080/guacamole').rstrip('/')
LISTEN = os.environ.get('LISTEN', '127.0.0.1:8081')
STATE_DIR = os.environ.get('STATE_DIRECTORY', 'prefs-data').split(':')[0]

MAX_BODY = 4096
DATA_SOURCE = re.compile(r'^[A-Za-z0-9_-]{1,64}$')

# Settings a user may store, with the values each accepts
SETTINGS = {
    'resolution': re.compile(r'^(auto|\d{3,5}x\d{3,5})$'),
}


class Refused(Exception):
    def __init__(self, status, message):
        super().__init__(message)
        self.status = status


def username_for(token, data_source):
    """Asks Guacamole which user the token belongs to."""
    request = urllib.request.Request(
        GUACAMOLE_URL + '/api/session/data/' + data_source + '/self',
        headers={'Guacamole-Token': token})
    try:
        with urllib.request.urlopen(request, timeout=5) as response:
            username = json.load(response).get('username')
    except urllib.error.HTTPError as error:
        if error.code in (401, 403, 404):
            raise Refused(403, 'not signed in')
        raise Refused(502, 'Guacamole answered HTTP %d' % error.code)
    except (OSError, ValueError):
        raise Refused(502, 'Guacamole is not reachable')
    if not isinstance(username, str) or not username:
        raise Refused(403, 'not signed in')
    return username


def settings_path(data_source, username):
    # Hashed, so no username can name a path outside the folder
    key = hashlib.sha256((data_source + '/' + username).encode()).hexdigest()
    return os.path.join(STATE_DIR, key + '.json')


def load(path):
    try:
        with open(path) as file:
            settings = json.load(file)
        return settings if isinstance(settings, dict) else {}
    except (OSError, ValueError):
        return {}


def save(path, settings):
    handle, temporary = tempfile.mkstemp(dir=STATE_DIR, suffix='.tmp')
    with os.fdopen(handle, 'w') as file:
        json.dump(settings, file)
    os.replace(temporary, path)


class Handler(BaseHTTPRequestHandler):

    server_version = 'session-tools-prefs'
    sys_version = ''

    def send_json(self, status, body):
        data = json.dumps(body).encode()
        self.send_response(status)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(data)))
        self.send_header('Cache-Control', 'no-store')
        self.end_headers()
        self.wfile.write(data)

    def user_settings_path(self):
        url = urllib.parse.urlsplit(self.path)
        if url.path != '/prefs':
            raise Refused(404, 'not found')
        data_source = urllib.parse.parse_qs(url.query).get('dataSource', ['postgresql'])[0]
        token = self.headers.get('Guacamole-Token', '')
        if not DATA_SOURCE.match(data_source) or not token:
            raise Refused(403, 'not signed in')
        return settings_path(data_source, username_for(token, data_source))

    def handle_request(self, action):
        try:
            self.send_json(200, action(self.user_settings_path()))
        except Refused as error:
            self.send_json(error.status, {'message': str(error)})

    def do_GET(self):
        self.handle_request(load)

    def do_PUT(self):
        def update(path):
            length = int(self.headers.get('Content-Length') or 0)
            if length > MAX_BODY:
                raise Refused(413, 'too large')
            try:
                changes = json.loads(self.rfile.read(length) or b'{}')
            except ValueError:
                raise Refused(400, 'invalid JSON')
            if not isinstance(changes, dict):
                raise Refused(400, 'expected an object')
            for name, value in changes.items():
                if name not in SETTINGS or not isinstance(value, str) or not SETTINGS[name].match(value):
                    raise Refused(400, 'invalid setting: %s' % name)
            settings = load(path)
            settings.update(changes)
            save(path, settings)
            return settings
        self.handle_request(update)

    def log_message(self, format, *args):
        # Requests are not logged; errors still go to stderr
        pass


def main():
    os.makedirs(STATE_DIR, exist_ok=True)
    host, _, port = LISTEN.rpartition(':')
    server = ThreadingHTTPServer((host or '127.0.0.1', int(port)), Handler)
    print('listening on %s, Guacamole at %s, settings in %s' % (LISTEN, GUACAMOLE_URL, STATE_DIR), file=sys.stderr)
    server.serve_forever()


if __name__ == '__main__':
    main()
