/*
 * Floating, draggable widget shown while a remote session (#/client/...) is
 * open. It holds:
 *
 *  - A ping meter: round-trip time between this browser and the Guacamole
 *    server, measured from the "ping" messages the Guacamole WebSocket tunnel
 *    already sends and the server echoes back. It does not include the hop
 *    from the Guacamole server to the remote desktop.
 *
 *  - A fullscreen toggle (hidden where the Fullscreen API is missing, such as
 *    Safari on iPhone).
 *
 *  - A keys button. It opens a panel with F1-F12, Del, Ctrl+Alt+Del and
 *    Ctrl, Alt and Shift toggles, for keys the browser or device cannot send.
 *
 *  - A clipboard button. It opens a text box to send text to the remote
 *    clipboard or type it, and to copy remote text onto this device, for
 *    browsers (such as on Android) where the automatic clipboard sync fails.
 *
 *  - A files button. It opens a panel to upload files from this device to
 *    the drive the connection shares with the remote computer, and to
 *    download files from it.
 *
 *  - A sound button. It opens a panel to turn the remote computer's sound
 *    on or off for the user's account. Sound streams uncompressed, so
 *    turning it off saves bandwidth on slow links. Switching reconnects the
 *    session, so it takes a click in the panel, not just the button.
 *
 *  - A settings button. It opens a dialog to pick the screen resolution of
 *    the remote desktop and to change the user's Guacamole password.
 *
 * Drag the widget to move it; its position is remembered per browser.
 */
(function () {

    var STORAGE_KEY = 'session-tools-position';
    var RESOLUTION_KEY = 'session-tools-resolution';
    var DRAG_THRESHOLD = 6;

    // Longest wait for the user's settings before connecting without them
    var SETTINGS_TIMEOUT_MS = 4000;

    // Round-trip thresholds in milliseconds
    var GOOD_MS = 100;
    var FAIR_MS = 250;

    // No echo for this long means the connection has stalled
    var STALE_MS = 5000;

    // Number of recent samples averaged for the displayed value
    var SAMPLE_COUNT = 5;

    // Matches an echoed internal ping: "0.,4.ping,<len>.<timestamp>;"
    var PING_ECHO = /(?:^|;)0\.,4\.ping,\d+\.(\d+);/g;

    var ICON_ENTER = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round">'
        + '<path d="M4 9V4h5M20 9V4h-5M4 15v5h5M20 15v5h-5"/></svg>';
    // Fixed resolutions offered in the settings dialog, besides "fit window",
    // grouped by aspect ratio
    var RESOLUTIONS = [
        ['Widescreen (16:9)', [[1280, 720], [1366, 768], [1600, 900], [1920, 1080], [2560, 1440]]],
        ['Laptop (16:10)', [[1280, 800], [1440, 900], [1680, 1050], [1920, 1200], [2560, 1600]]]
    ];

    var MIN_PASSWORD_LENGTH = 8;

    // Per-device switch for sending this device's clipboard automatically,
    // and how often to look at it while switched on
    var AUTO_CLIPBOARD_KEY = 'session-tools-clipboard-auto';
    var AUTO_CLIPBOARD_MS = 1000;

    // X11 keysyms, as Guacamole sends them
    var KEY_CTRL = 0xFFE3;
    var KEY_ALT = 0xFFE9;
    var KEY_SHIFT = 0xFFE1;
    var KEY_DELETE = 0xFFFF;
    var KEY_F1 = 0xFFBE;

    var ICON_CLIPBOARD = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">'
        + '<rect x="8" y="2" width="8" height="4" rx="1"/><path d="M16 4h2a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2V6a2 2 0 0 1 2-2h2"/></svg>';
    var ICON_FOLDER = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">'
        + '<path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v9a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"/><path d="M12 11v6M9 14l3-3 3 3"/></svg>';
    var ICON_KEYBOARD ='<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">'
        + '<rect x="2" y="5" width="20" height="14" rx="2"/><path d="M6 9h.01M10 9h.01M14 9h.01M18 9h.01M6 13h.01M18 13h.01M10 13h4M7 16h10"/></svg>';

    var ICON_SOUND_ON = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">'
        + '<path d="M11 5 6 9H2v6h4l5 4z"/><path d="M15.5 8.5a5 5 0 0 1 0 7M19 5a10 10 0 0 1 0 14"/></svg>';
    var ICON_SOUND_OFF = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">'
        + '<path d="M11 5 6 9H2v6h4l5 4z"/><path d="M23 9l-6 6M17 9l6 6"/></svg>';

    var ICON_SETTINGS = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">'
        + '<circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 1 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06A1.65 1.65 0 0 0 4.68 15a1.65 1.65 0 0 0-1.51-1H3a2 2 0 1 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06A1.65 1.65 0 0 0 9 4.68a1.65 1.65 0 0 0 1-1.51V3a2 2 0 1 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06A1.65 1.65 0 0 0 19.4 9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 1 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z"/></svg>';
    var ICON_EXIT = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round">'
        + '<path d="M9 4v5H4M15 4v5h5M9 20v-5H4M15 20v-5h5"/></svg>';

    var doc = document;
    var root = doc.documentElement;

    /* ---------------------------------------------------------------------
     * Ping measurement
     * ------------------------------------------------------------------- */

    var samples = [];
    var lastEcho = 0;
    var activeSocket = null;

    function resetSamples() {
        samples = [];
        lastEcho = 0;
    }

    function handleMessage(event) {

        // Only the newest tunnel counts; older sockets may still be closing
        if (event.target !== activeSocket || typeof event.data !== 'string')
            return;

        var now = Date.now();
        var match;
        PING_ECHO.lastIndex = 0;
        while ((match = PING_ECHO.exec(event.data))) {
            var rtt = now - parseInt(match[1], 10);
            if (rtt < 0 || rtt > 60000)
                continue;
            samples.push(rtt);
            if (samples.length > SAMPLE_COUNT)
                samples.shift();
            lastEcho = now;
        }
    }

    // Observe every WebSocket the page opens. Guacamole assigns its own
    // onmessage handler; an extra listener runs alongside it untouched.
    var NativeWebSocket = window.WebSocket;
    if (NativeWebSocket) {
        var WrappedWebSocket = function (url, protocols) {
            var isTunnel = /\/websocket-tunnel/.test(url);
            var socket = arguments.length > 1
                ? new NativeWebSocket(url, protocols)
                : new NativeWebSocket(url);
            if (isTunnel) {
                activeSocket = socket;
                resetSamples();
                socket.addEventListener('message', handleMessage);
            }
            return socket;
        };
        WrappedWebSocket.prototype = NativeWebSocket.prototype;
        ['CONNECTING', 'OPEN', 'CLOSING', 'CLOSED'].forEach(function (name) {
            WrappedWebSocket[name] = NativeWebSocket[name];
        });
        window.WebSocket = WrappedWebSocket;
    }

    function updatePing(meter, label) {

        var state, text;

        if (!lastEcho) {
            state = '';
            text = '– ms';
        }
        else if (Date.now() - lastEcho > STALE_MS) {
            state = 'bad';
            text = 'lost';
        }
        else {
            var sum = 0;
            for (var i = 0; i < samples.length; i++)
                sum += samples[i];
            var avg = Math.round(sum / samples.length);
            state = avg < GOOD_MS ? 'good' : avg < FAIR_MS ? 'fair' : 'bad';
            text = avg + ' ms';
        }

        meter.className = 'ping' + (state ? ' ' + state : '');
        label.textContent = text;
        meter.title = 'Round-trip time to the Guacamole server'
            + (state === 'good' ? ' (good)' : state === 'fair' ? ' (fair)' : state === 'bad' ? ' (poor)' : '');
    }

    /* ---------------------------------------------------------------------
     * Fullscreen
     * ------------------------------------------------------------------- */

    var requestFullscreen = root.requestFullscreen || root.webkitRequestFullscreen;
    var exitFullscreen = doc.exitFullscreen || doc.webkitExitFullscreen;
    var fullscreenSupported = !!(requestFullscreen && exitFullscreen);

    function fullscreenElement() {
        return doc.fullscreenElement || doc.webkitFullscreenElement;
    }

    function toggleFullscreen() {
        if (fullscreenElement())
            exitFullscreen.call(doc);
        else {
            var result = requestFullscreen.call(root);
            // Lock the keyboard where supported so Esc, Alt+Tab, etc. reach
            // the remote desktop instead of the browser
            if (result && result.then && navigator.keyboard && navigator.keyboard.lock)
                result.then(function () { navigator.keyboard.lock().catch(function () {}); });
        }
    }

    function updateFullscreenIcon(button) {
        var active = !!fullscreenElement();
        button.innerHTML = active ? ICON_EXIT : ICON_ENTER;
        button.title = active ? 'Exit fullscreen' : 'Fullscreen';
    }

    /* ---------------------------------------------------------------------
     * Resolution
     *
     * The browser sends the desired screen size when it connects (GUAC_WIDTH,
     * GUAC_HEIGHT and GUAC_DPI in the connect data). A chosen resolution
     * replaces those values, so it applies from the next connection on.
     * Connections that set "width" and "height" themselves ignore it.
     *
     * The choice is stored per Guacamole user on the server, by the
     * session-tools-prefs service (prefs/ in this repository), so it follows
     * the user to every browser and device.
     * ------------------------------------------------------------------- */

    // Older versions kept the choice in the browser; it moves to the server
    function readLegacyResolution() {
        var match = doc.cookie.match(new RegExp('(?:^|;\\s*)' + RESOLUTION_KEY + '=([^;]*)'));
        if (match)
            return match[1];
        try {
            var saved = JSON.parse(localStorage.getItem(RESOLUTION_KEY));
            if (saved && saved.width > 0 && saved.height > 0)
                return saved.width + 'x' + saved.height;
        }
        catch (e) {}
        return null;
    }

    function clearLegacyResolution() {
        var expired = RESOLUTION_KEY + '=; path=/; max-age=0';
        var parts = window.location.hostname.split('.');
        for (var i = 0; i < parts.length - 1; i++)
            doc.cookie = expired + '; domain=' + parts.slice(i).join('.');
        doc.cookie = expired;
        try {
            localStorage.removeItem(RESOLUTION_KEY);
        }
        catch (e) {}
    }

    function parseResolution(value) {
        var match = /^(\d+)x(\d+)$/.exec(value || '');
        return match ? { width: parseInt(match[1], 10), height: parseInt(match[2], 10) } : null;
    }

    function setQueryParam(url, name, value) {
        var pattern = new RegExp('([?&]|^)(' + name + '=)[^&]*');
        if (pattern.test(url))
            return url.replace(pattern, '$1$2' + value);
        return url + (url ? '&' : '') + name + '=' + value;
    }

    function applyResolution(data, resolution) {
        if (!resolution)
            return data;
        data = setQueryParam(data, 'GUAC_WIDTH', resolution.width);
        data = setQueryParam(data, 'GUAC_HEIGHT', resolution.height);
        // A fixed size is meant in real pixels; keep Windows at 100% scaling
        return setQueryParam(data, 'GUAC_DPI', 96);
    }

    /* ---------------------------------------------------------------------
     * Sound
     *
     * The browser lists the audio formats it can play when it connects
     * (GUAC_AUDIO in the connect data). With none listed, guacd sends no
     * sound at all, which saves the bandwidth of the uncompressed stream.
     * Like the resolution, the choice is stored per user.
     * ------------------------------------------------------------------- */

    function soundEnabled(settings) {
        return settings.audio !== 'off';
    }

    function applySound(data, enabled) {
        if (enabled)
            return data;
        return data.split('&').filter(function (param) {
            return param.indexOf('GUAC_AUDIO=') !== 0;
        }).join('&');
    }

    // Browsers start an AudioContext created before the first click or key
    // press on the page suspended, and Guacamole never resumes the one it
    // plays sound through. A page loaded straight into a session (after a
    // reload, or a switch of sound or resolution) would stay silent, so
    // resume it on the first interaction.
    function resumeSound() {
        var factory = window.Guacamole && window.Guacamole.AudioContextFactory;
        var context = factory && factory.singleton;
        if (context && context.state === 'suspended')
            context.resume().catch(function () {});
    }

    ['pointerdown', 'mousedown', 'touchend', 'keydown'].forEach(function (type) {
        window.addEventListener(type, resumeSound, true);
    });

    /* ---------------------------------------------------------------------
     * Per-user settings, stored by the session-tools-prefs service
     * ------------------------------------------------------------------- */

    var userSettings = null;

    function settingsRequest(method, body) {

        var auth = guacamoleService('authenticationService');
        var token = auth && auth.getCurrentToken();
        if (!token)
            return Promise.reject(new Error('You are not signed in.'));

        return fetch('session-tools/prefs?dataSource=' + encodeURIComponent(auth.getDataSource()), {
            method: method,
            headers: {
                'Content-Type': 'application/json',
                'Guacamole-Token': token
            },
            body: body ? JSON.stringify(body) : undefined
        }).then(function (response) {
            if (!response.ok)
                throw new Error('Could not reach the settings service (HTTP ' + response.status + ').');
            return response.json();
        });
    }

    function saveSettings(changes) {
        return settingsRequest('PUT', changes).then(function (settings) {
            userSettings = { token: userSettings && userSettings.token, promise: Promise.resolve(settings) };
            return settings;
        });
    }

    // Settings of the signed-in user, fetched once per sign-in
    function loadSettings() {

        var auth = guacamoleService('authenticationService');
        var token = auth && auth.getCurrentToken();
        if (userSettings && userSettings.token === token)
            return userSettings.promise;

        var promise = settingsRequest('GET').then(function (settings) {
            var legacy = readLegacyResolution();
            if (!legacy)
                return settings;
            clearLegacyResolution();
            return settings.resolution ? settings : saveSettings({ resolution: legacy });
        }).catch(function () {
            return {};
        });
        userSettings = { token: token, promise: promise };
        return promise;
    }

    // Guacamole builds the connect data before it connects; hold the
    // connection until the user's settings are known (at most a few seconds)
    // and put the chosen resolution and sound setting in. This covers the WebSocket tunnel
    // and the HTTP fallback alike.
    var NativeClient = window.Guacamole && window.Guacamole.Client;
    if (NativeClient) {
        var WrappedClient = function (tunnel) {
            var client = new NativeClient(tunnel);
            var connect = client.connect;
            client.connect = function (data) {
                var timeout = new Promise(function (resolve) { window.setTimeout(resolve, SETTINGS_TIMEOUT_MS, {}); });
                Promise.race([loadSettings(), timeout]).then(function (settings) {
                    data = applyResolution(data || '', parseResolution(settings.resolution));
                    connect.call(client, applySound(data, soundEnabled(settings)));
                });
            };
            return client;
        };
        WrappedClient.prototype = NativeClient.prototype;
        Object.keys(NativeClient).forEach(function (name) {
            WrappedClient[name] = NativeClient[name];
        });
        window.Guacamole.Client = WrappedClient;
    }

    /* ---------------------------------------------------------------------
     * Password change, through the same REST call as Guacamole's own
     * "Change password" form. The account needs UPDATE permission on itself.
     * ------------------------------------------------------------------- */

    function guacamoleService(name) {
        var injector = window.angular && window.angular.element(doc.body).injector();
        return injector ? injector.get(name) : null;
    }

    function changePassword(oldPassword, newPassword) {

        var auth = guacamoleService('authenticationService');
        var token = auth && auth.getCurrentToken();
        if (!token)
            return Promise.reject(new Error('You are not signed in.'));

        var url = 'api/session/data/' + encodeURIComponent(auth.getDataSource())
            + '/users/' + encodeURIComponent(auth.getCurrentUsername()) + '/password';

        return fetch(url, {
            method: 'PUT',
            headers: {
                'Content-Type': 'application/json',
                'Guacamole-Token': token
            },
            body: JSON.stringify({
                oldPassword: oldPassword,
                newPassword: newPassword
            })
        }).then(function (response) {
            if (response.ok)
                return;
            if (response.status === 403)
                throw new Error('The current password is wrong, or this account may not change its password.');
            return response.json().catch(function () { return {}; }).then(function (error) {
                throw new Error(error.message || 'Could not change the password (HTTP ' + response.status + ').');
            });
        });
    }

    /* ---------------------------------------------------------------------
     * Settings dialog
     * ------------------------------------------------------------------- */

    function element(tag, className, text) {
        var node = doc.createElement(tag);
        if (className)
            node.className = className;
        if (text)
            node.textContent = text;
        return node;
    }

    function passwordField(form, labelText, autocomplete) {
        var label = element('label', null, labelText);
        var input = element('input');
        input.type = 'password';
        input.autocomplete = autocomplete;
        input.required = true;
        label.appendChild(input);
        form.appendChild(label);
        return input;
    }

    function setStatus(node, text, state) {
        node.textContent = text || '';
        node.className = 'status' + (state ? ' ' + state : '');
    }

    function createSettingsDialog() {

        var overlay = element('div');
        overlay.id = 'session-tools-settings';

        var dialog = element('div', 'dialog');
        dialog.setAttribute('role', 'dialog');
        dialog.setAttribute('aria-modal', 'true');
        dialog.setAttribute('aria-label', 'Settings');
        overlay.appendChild(dialog);

        var header = element('div', 'header');
        header.appendChild(element('h2', null, 'Settings'));
        var close = element('button', 'close', '\u00d7');
        close.type = 'button';
        close.title = 'Close';
        header.appendChild(close);
        dialog.appendChild(header);

        // Resolution
        var resolutionForm = element('form');
        resolutionForm.appendChild(element('h3', null, 'Screen resolution'));
        var select = element('select');
        select.appendChild(new Option('Fit browser window', ''));
        RESOLUTIONS.forEach(function (group) {
            var optgroup = element('optgroup');
            optgroup.label = group[0];
            group[1].forEach(function (size) {
                optgroup.appendChild(new Option(size[0] + ' \u00d7 ' + size[1], size[0] + 'x' + size[1]));
            });
            select.appendChild(optgroup);
        });
        resolutionForm.appendChild(select);
        resolutionForm.appendChild(element('p', 'hint', 'Saved for your account on every device. Applying reconnects the session; open programs stay open.'));
        var resolutionStatus = element('p', 'status');
        resolutionStatus.setAttribute('aria-live', 'polite');
        resolutionForm.appendChild(resolutionStatus);
        var apply = element('button', 'primary', 'Apply');
        apply.type = 'submit';
        resolutionForm.appendChild(apply);
        dialog.appendChild(resolutionForm);

        // Password
        var passwordForm = element('form');
        passwordForm.appendChild(element('h3', null, 'Change password'));
        var current = passwordField(passwordForm, 'Current password', 'current-password');
        var next = passwordField(passwordForm, 'New password', 'new-password');
        var confirm = passwordField(passwordForm, 'Confirm new password', 'new-password');
        next.minLength = MIN_PASSWORD_LENGTH;
        var status = element('p', 'status');
        status.setAttribute('aria-live', 'polite');
        passwordForm.appendChild(status);
        var save = element('button', 'primary', 'Change password');
        save.type = 'submit';
        passwordForm.appendChild(save);
        dialog.appendChild(passwordForm);

        function open() {
            select.disabled = true;
            setStatus(resolutionStatus);
            loadSettings().then(function (settings) {
                select.value = parseResolution(settings.resolution) ? settings.resolution : '';
                if (select.selectedIndex < 0)
                    select.value = '';
                select.disabled = false;
            });
            passwordForm.reset();
            setStatus(status);
            overlay.classList.add('visible');
        }

        function hide() {
            overlay.classList.remove('visible');
            if (overlay.contains(doc.activeElement))
                doc.activeElement.blur();
        }

        resolutionForm.addEventListener('submit', function (e) {
            e.preventDefault();
            apply.disabled = true;
            setStatus(resolutionStatus, 'Saving\u2026');
            saveSettings({ resolution: select.value || 'auto' }).then(function () {
                // Reconnect so the new size is sent to the remote desktop
                window.location.reload();
            }, function (error) {
                setStatus(resolutionStatus, error.message, 'error');
                apply.disabled = false;
            });
        });

        passwordForm.addEventListener('submit', function (e) {
            e.preventDefault();
            if (next.value.length < MIN_PASSWORD_LENGTH)
                return setStatus(status, 'The new password needs at least ' + MIN_PASSWORD_LENGTH + ' characters.', 'error');
            if (next.value !== confirm.value)
                return setStatus(status, 'The new passwords do not match.', 'error');
            if (next.value === current.value)
                return setStatus(status, 'The new password is the same as the current one.', 'error');

            save.disabled = true;
            setStatus(status, 'Saving\u2026');
            changePassword(current.value, next.value).then(function () {
                passwordForm.reset();
                setStatus(status, 'Password changed.', 'success');
            }, function (error) {
                setStatus(status, error.message, 'error');
            }).then(function () {
                save.disabled = false;
            });
        });

        close.addEventListener('click', hide);
        overlay.addEventListener('click', function (e) {
            if (e.target === overlay)
                hide();
        });

        // While the dialog is open, keep keystrokes away from the remote
        // desktop. Guacamole listens on the document; stopping the events at
        // the window first leaves typing into the dialog's fields intact.
        ['keydown', 'keypress', 'keyup', 'input',
            'compositionstart', 'compositionupdate', 'compositionend'].forEach(function (type) {
            window.addEventListener(type, function (e) {
                if (!overlay.classList.contains('visible'))
                    return;
                e.stopPropagation();
                if (type === 'keydown' && e.key === 'Escape')
                    hide();
            }, true);
        });

        // Keep clicks and touches away from the remote display underneath
        ['pointerdown', 'pointerup', 'mousedown', 'mouseup', 'click', 'touchstart', 'touchend'].forEach(function (type) {
            overlay.addEventListener(type, function (e) { e.stopPropagation(); });
        });

        doc.body.appendChild(overlay);

        return { open: open, hide: hide };
    }

    /* ---------------------------------------------------------------------
     * Panels next to the widget (keys, clipboard). One is open at a time.
     * ------------------------------------------------------------------- */

    function addButton(row, text, title, onPress) {
        var button = element('button', null, text);
        button.type = 'button';
        button.title = title;
        button.addEventListener('click', onPress);
        row.appendChild(button);
        return button;
    }

    function createPanel(widget, id, onHide) {

        var panel = element('div', 'session-tools-panel');
        panel.id = id;

        // Keep clicks and touches away from the remote display underneath
        ['pointerdown', 'pointerup', 'mousedown', 'mouseup', 'click', 'touchstart', 'touchend'].forEach(function (type) {
            panel.addEventListener(type, function (e) { e.stopPropagation(); });
        });

        doc.body.appendChild(panel);

        function isOpen() {
            return panel.classList.contains('visible');
        }

        // Next to the widget: above it when there is room, else below
        function position() {
            if (!isOpen())
                return;
            var anchor = widget.getBoundingClientRect();
            var width = panel.offsetWidth;
            var height = panel.offsetHeight;
            var top = anchor.top - height - 8 >= 0 ? anchor.top - height - 8 : anchor.bottom + 8;
            panel.style.left = clamp(anchor.right - width, 0, window.innerWidth - width) + 'px';
            panel.style.top = clamp(top, 0, window.innerHeight - height) + 'px';
        }

        window.addEventListener('resize', position);

        return {
            element: panel,
            isOpen: isOpen,
            position: position,
            show: function () {
                panel.classList.add('visible');
                position();
            },
            hide: function () {
                panel.classList.remove('visible');
                if (onHide)
                    onHide();
            }
        };
    }

    /* ---------------------------------------------------------------------
     * Keys panel
     *
     * Keys go to the remote desktop through Guacamole's own "synthetic key"
     * events, the same path as its on-screen keyboard. Ctrl, Alt and Shift
     * latch: they stay pressed until the next other key, from the panel or
     * the physical keyboard, or until tapped again.
     * ------------------------------------------------------------------- */

    function sendKey(pressed, keysym) {
        var rootScope = guacamoleService('$rootScope');
        if (rootScope)
            rootScope.$broadcast(pressed ? 'guacSyntheticKeydown' : 'guacSyntheticKeyup', keysym);
    }

    function createKeysPanel(widget) {

        var latched = {};
        var modifierButtons = {};

        function releaseModifiers() {
            Object.keys(latched).forEach(function (keysym) {
                sendKey(false, parseInt(keysym, 10));
                modifierButtons[keysym].classList.remove('active');
            });
            latched = {};
        }

        var keys = createPanel(widget, 'session-tools-keys', releaseModifiers);
        var panel = keys.element;

        function pressKey(keysym) {
            sendKey(true, keysym);
            sendKey(false, keysym);
            releaseModifiers();
        }

        var functionKeys = element('div', 'keys function-keys');
        for (var n = 1; n <= 12; n++) {
            (function (keysym, name) {
                addButton(functionKeys, name, name, function () { pressKey(keysym); });
            })(KEY_F1 + n - 1, 'F' + n);
        }
        panel.appendChild(functionKeys);

        var modifiers = element('div', 'keys');
        [[KEY_CTRL, 'Ctrl'], [KEY_ALT, 'Alt'], [KEY_SHIFT, 'Shift']].forEach(function (key) {
            var keysym = key[0];
            modifierButtons[keysym] = addButton(modifiers, key[1], 'Hold ' + key[1] + ' for the next key', function () {
                if (latched[keysym]) {
                    delete latched[keysym];
                    sendKey(false, keysym);
                    modifierButtons[keysym].classList.remove('active');
                }
                else {
                    latched[keysym] = true;
                    sendKey(true, keysym);
                    modifierButtons[keysym].classList.add('active');
                }
            });
        });
        addButton(modifiers, 'Del', 'Delete', function () { pressKey(KEY_DELETE); });
        panel.appendChild(modifiers);

        var combos = element('div', 'keys');
        addButton(combos, 'Ctrl + Alt + Del', 'Send Ctrl+Alt+Del', function () {
            [KEY_CTRL, KEY_ALT, KEY_DELETE].forEach(function (keysym) { sendKey(true, keysym); });
            [KEY_DELETE, KEY_ALT, KEY_CTRL].forEach(function (keysym) { sendKey(false, keysym); });
            releaseModifiers();
        }).className = 'wide';
        panel.appendChild(combos);

        // A latched modifier applies to the next key typed on the physical
        // keyboard too; release it once Guacamole has sent that key
        window.addEventListener('keyup', function (e) {
            if (Object.keys(latched).length && !/^(Control|Alt|Shift|Meta|AltGraph)$/.test(e.key))
                window.setTimeout(releaseModifiers, 50);
        }, true);

        // Keep keyboard focus where it is
        panel.addEventListener('mousedown', function (e) { e.preventDefault(); });

        return keys;
    }

    /* ---------------------------------------------------------------------
     * Clipboard panel
     *
     * Guacamole syncs the clipboard by reading the browser's clipboard on
     * its own, which mobile browsers (Android) refuse. Here the user pastes
     * into a text box instead and sends it: onto the remote clipboard
     * (optionally followed by Ctrl+V), or typed key by key for places that
     * ignore the clipboard. Text copied on the remote desktop shows up in
     * the box, to copy onto this device.
     *
     * A switch in the panel sends whatever is copied on this device to the
     * remote clipboard automatically and pastes it (Ctrl+V) into whatever
     * has focus on the remote desktop. It needs a browser that grants
     * permanent clipboard access (Chrome, Edge, Chrome on Android); it is a
     * per-device choice because that permission is.
     * ------------------------------------------------------------------- */

    // Keysym for a character: Latin-1 maps directly, the rest of Unicode
    // goes through the 0x01000000 range
    function keysymForCharacter(character) {
        var codePoint = character.codePointAt(0);
        if (character === '\n')
            return 0xFF0D;
        if (character === '\t')
            return 0xFF09;
        if (codePoint >= 0x20 && codePoint <= 0x7E || codePoint >= 0xA0 && codePoint <= 0xFF)
            return codePoint;
        return 0x01000000 | codePoint;
    }

    function createClipboardPanel(widget) {

        var clipboard = createPanel(widget, 'session-tools-clipboard');
        var panel = clipboard.element;

        panel.appendChild(element('div', 'title', 'Clipboard'));

        var text = element('textarea');
        text.rows = 4;
        text.spellcheck = false;
        text.placeholder = 'Paste or type text here, then send it to the remote computer.';
        panel.appendChild(text);

        var autoLabel = element('label', 'auto');
        var autoSwitch = element('input');
        autoSwitch.type = 'checkbox';
        autoLabel.appendChild(autoSwitch);
        autoLabel.appendChild(doc.createTextNode('Auto-paste what I copy on this device'));
        autoLabel.title = 'Text copied on this device is sent to the remote computer and pasted (Ctrl+V) where the cursor is';
        panel.appendChild(autoLabel);

        var status = element('p', 'status');
        status.setAttribute('aria-live', 'polite');

        function setPanelStatus(message, state) {
            setStatus(status, message, state);
            clipboard.position();
        }

        // Newest text seen on either side, so nothing is sent back and forth
        var lastText = null;

        function sendToRemote(value) {
            var service = guacamoleService('clipboardService');
            var ClipboardData = guacamoleService('ClipboardData');
            if (!service || !ClipboardData)
                return Promise.reject();
            lastText = value;
            return service.setClipboard(new ClipboardData({ type: 'text/plain', data: value }));
        }

        // Ctrl+V on the remote desktop, once the remote clipboard has had a
        // moment to update
        function pasteOnRemote() {
            window.setTimeout(function () {
                sendKey(true, KEY_CTRL);
                sendKey(true, 0x76);
                sendKey(false, 0x76);
                sendKey(false, KEY_CTRL);
            }, 300);
        }

        var actions = element('div', 'actions');

        addButton(actions, 'From device', 'Paste the clipboard of this device into the box', function () {
            if (!navigator.clipboard || !navigator.clipboard.readText)
                return setPanelStatus('Long-press the box and choose Paste.');
            navigator.clipboard.readText().then(function (value) {
                text.value = value;
                setPanelStatus(value ? '' : 'The clipboard of this device is empty.');
            }, function () {
                setPanelStatus('The browser did not allow it. Long-press the box and choose Paste.', 'error');
            });
        });

        addButton(actions, 'Copy to device', 'Copy the text in the box to the clipboard of this device', function () {
            function copied() { setPanelStatus('Copied to this device.', 'success'); }
            function fallback() {
                text.select();
                if (doc.execCommand('copy'))
                    copied();
                else
                    setPanelStatus('Long-press the box and choose Copy.', 'error');
            }
            if (navigator.clipboard && navigator.clipboard.writeText)
                navigator.clipboard.writeText(text.value).then(copied, fallback);
            else
                fallback();
        });

        addButton(actions, 'Send', 'Put the text on the remote clipboard', function () {
            sendToRemote(text.value).then(function () {
                setPanelStatus('Sent. Paste it on the remote computer with Ctrl+V.', 'success');
            }, function () {
                setPanelStatus('Not connected.', 'error');
            });
        });

        addButton(actions, 'Send + paste', 'Put the text on the remote clipboard and press Ctrl+V', function () {
            sendToRemote(text.value).then(function () {
                pasteOnRemote();
                setPanelStatus('Sent and pasted.', 'success');
            }, function () {
                setPanelStatus('Not connected.', 'error');
            });
        });

        addButton(actions, 'Type', 'Type the text on the remote computer, key by key (works where pasting does not, such as the Windows login screen)', function () {
            Array.from(text.value.replace(/\r/g, '')).forEach(function (character) {
                var keysym = keysymForCharacter(character);
                sendKey(true, keysym);
                sendKey(false, keysym);
            });
            setPanelStatus('Typed.', 'success');
        });

        panel.appendChild(actions);
        panel.appendChild(status);

        // Typing in the box must not reach the remote desktop. Guacamole
        // listens on the document; stopping the events at the window first
        // leaves typing into the box intact.
        ['keydown', 'keypress', 'keyup', 'input',
            'compositionstart', 'compositionupdate', 'compositionend'].forEach(function (type) {
            window.addEventListener(type, function (e) {
                if (panel.contains(e.target))
                    e.stopPropagation();
            }, true);
        });

        // Show text copied on the remote desktop, unless the user is editing
        var watching = false;
        function watchRemoteClipboard() {
            var rootScope = !watching && guacamoleService('$rootScope');
            if (!rootScope)
                return watching;
            watching = true;
            rootScope.$on('guacClipboard', function (event, data) {
                if (!data.source || typeof data.data !== 'string')
                    return;
                lastText = data.data;
                if (doc.activeElement !== text)
                    text.value = data.data;
            });
            return true;
        }

        /* Auto-paste */

        function readPermission() {
            if (!navigator.permissions || !navigator.permissions.query)
                return Promise.resolve('unsupported');
            return navigator.permissions.query({ name: 'clipboard-read' }).then(function (result) {
                return result.state;
            }, function () {
                return 'unsupported';
            });
        }

        function saveAutoSwitch() {
            try {
                localStorage.setItem(AUTO_CLIPBOARD_KEY, autoSwitch.checked ? '1' : '0');
            }
            catch (e) {}
        }

        function disableAuto(message) {
            autoSwitch.checked = false;
            saveAutoSwitch();
            setPanelStatus(message, 'error');
        }

        // Reads this device's clipboard and, when it has changed, sends and
        // pastes it
        var reading = false;
        function syncFromDevice() {
            if (reading)
                return Promise.resolve();
            reading = true;
            return navigator.clipboard.readText().then(function (value) {
                reading = false;
                if (!value || value === lastText)
                    return;
                if (doc.activeElement !== text)
                    text.value = value;
                return sendToRemote(value).then(function () {
                    pasteOnRemote();
                    setPanelStatus('Copied text pasted on the remote computer.', 'success');
                }, function () {});
            }, function (error) {
                reading = false;
                throw error;
            });
        }

        // Only while a session is open and this page is in front; reading
        // the clipboard of a background page fails anyway
        function autoTick() {
            if (!autoSwitch.checked || reading || doc.visibilityState !== 'visible' || !doc.hasFocus()
                    || !/^#\/client\//.test(window.location.hash))
                return;
            readPermission().then(function (state) {
                if (state === 'granted' && autoSwitch.checked)
                    syncFromDevice().catch(function () {});
            });
        }

        autoSwitch.addEventListener('change', function () {
            saveAutoSwitch();
            setPanelStatus();
            if (!autoSwitch.checked)
                return;
            if (!navigator.clipboard || !navigator.clipboard.readText)
                return disableAuto('This browser cannot read the clipboard. Use "From device" instead.');
            // A tap is when the browser may ask for permission; ask now
            lastText = null;
            syncFromDevice().then(readPermission).then(function (state) {
                if (state !== 'granted')
                    disableAuto('This browser allows reading the clipboard only on a tap. Use "From device" instead.');
                else if (!status.textContent)
                    setPanelStatus('On. Text you copy on this device is pasted automatically.', 'success');
            }, function () {
                disableAuto('The browser did not allow reading the clipboard. Allow it in the site settings, then try again.');
            });
        });

        try {
            autoSwitch.checked = localStorage.getItem(AUTO_CLIPBOARD_KEY) === '1';
        }
        catch (e) {}
        window.setInterval(autoTick, AUTO_CLIPBOARD_MS);
        window.addEventListener('focus', autoTick);
        doc.addEventListener('visibilitychange', autoTick);

        var show = clipboard.show;
        clipboard.show = function () {
            setStatus(status);
            show();
        };
        clipboard.watchRemoteClipboard = watchRemoteClipboard;

        return clipboard;
    }

    /* ---------------------------------------------------------------------
     * Files panel
     *
     * Browses the drive the connection shares with the remote computer
     * (for RDP: "Enable drive" under Device Redirection), through the same
     * Guacamole calls as the file browser in its Ctrl+Alt+Shift menu.
     * Upload sends files from this device into the folder shown; a click on
     * a file downloads it. Guacamole shows upload progress in its own
     * transfer dialog.
     * ------------------------------------------------------------------- */

    // Client of the session on screen; with several tiled sessions, the
    // focused one
    function currentClient() {
        var match = /^#\/client\/([^?\/]+)/.exec(window.location.hash);
        var manager = match && guacamoleService('guacClientManager');
        if (!manager)
            return null;
        var id = decodeURIComponent(match[1]);
        var groups = manager.getManagedClientGroups().filter(function (group) {
            return group.clients.map(function (client) { return client.id; }).join('.') === id;
        });
        if (!groups.length || !groups[0].clients.length)
            return null;
        var clients = groups[0].clients;
        return clients.filter(function (client) { return client.clientProperties.focused; })[0] || clients[0];
    }

    function createFilesPanel(widget) {

        var files = createPanel(widget, 'session-tools-files');
        var panel = files.element;

        var title = element('div', 'title', 'Files');
        panel.appendChild(title);

        var bar = element('div', 'location');
        var up = addButton(bar, '\u2191', 'Up one folder', function () {
            if (filesystem && filesystem.currentDirectory.parent)
                openDirectory(filesystem.currentDirectory.parent);
        });
        var path = element('span', 'path');
        bar.appendChild(path);
        panel.appendChild(bar);

        var list = element('ul', 'list');
        panel.appendChild(list);

        var picker = element('input');
        picker.type = 'file';
        picker.multiple = true;
        picker.hidden = true;
        panel.appendChild(picker);

        var actions = element('div', 'actions');
        var upload = addButton(actions, 'Upload', 'Send files from this device to the folder shown', function () {
            picker.click();
        });
        var refresh = addButton(actions, 'Refresh', 'Reload the folder', function () {
            if (filesystem)
                openDirectory(filesystem.currentDirectory);
        });
        panel.appendChild(actions);

        var status = element('p', 'status');
        status.setAttribute('aria-live', 'polite');
        panel.appendChild(status);

        var hint = element('p', 'hint');
        panel.appendChild(hint);

        function setPanelStatus(message, state) {
            setStatus(status, message, state);
            files.position();
        }

        // Session and drive on display, and the folder listing last drawn
        var client = null;
        var filesystem = null;
        var shownFiles = null;

        function openDirectory(directory) {
            var ManagedFilesystem = guacamoleService('ManagedFilesystem');
            if (ManagedFilesystem)
                ManagedFilesystem.changeDirectory(filesystem, directory);
            shownFiles = null;
            render();
        }

        function directoryPath(directory) {
            var names = [];
            for (var node = directory; node && node.parent; node = node.parent)
                names.unshift(node.name);
            return filesystem.name + (names.length ? ' / ' + names.join(' / ') : '');
        }

        function render() {

            var connected = !!(client && filesystem);
            upload.disabled = refresh.disabled = !connected;
            up.disabled = !connected || !filesystem.currentDirectory.parent;
            bar.hidden = !connected;
            list.hidden = !connected;
            if (!connected) {
                hint.textContent = client
                    ? 'This connection has no shared drive. Ask your administrator to enable one.'
                    : 'Not connected.';
                files.position();
                return;
            }

            hint.textContent = 'To get a file from the remote computer, copy it into '
                + filesystem.name + ' there, then click it here. Files put in its Download folder download by themselves.';

            var directory = filesystem.currentDirectory;
            path.textContent = directoryPath(directory);
            path.title = path.textContent;
            shownFiles = directory.files;
            list.textContent = '';

            var entries = Object.keys(shownFiles).map(function (name) { return shownFiles[name]; });
            entries.sort(function (a, b) {
                var aFolder = a.type === 'DIRECTORY', bFolder = b.type === 'DIRECTORY';
                return aFolder !== bFolder ? (aFolder ? -1 : 1) : a.name.localeCompare(b.name);
            });

            if (!entries.length)
                list.appendChild(element('li', 'empty', 'This folder is empty.'));

            entries.forEach(function (file) {
                var folder = file.type === 'DIRECTORY';
                var item = element('li', folder ? 'folder' : 'file');
                addButton(item, file.name + (folder ? '/' : ''),
                    folder ? 'Open ' + file.name : 'Download ' + file.name, function () {
                        if (folder)
                            return openDirectory(file);
                        var ManagedFilesystem = guacamoleService('ManagedFilesystem');
                        if (!ManagedFilesystem)
                            return;
                        ManagedFilesystem.downloadFile(filesystem, file.streamName);
                        setPanelStatus('Downloading ' + file.name + '\u2026');
                    });
                list.appendChild(item);
            });

            files.position();
        }

        // Pick up the session's drive, and folder contents as they arrive
        function poll() {
            checkUploads();
            if (!files.isOpen())
                return;
            var next = currentClient();
            var nextFilesystem = next && next.filesystems.length ? next.filesystems[0] : null;
            if (next !== client || nextFilesystem !== filesystem) {
                client = next;
                filesystem = nextFilesystem;
                render();
            }
            else if (filesystem && filesystem.currentDirectory.files !== shownFiles)
                render();
        }

        picker.addEventListener('change', function () {
            var chosen = Array.from(picker.files);
            picker.value = '';
            var ManagedClient = guacamoleService('ManagedClient');
            var rootScope = guacamoleService('$rootScope');
            if (!chosen.length || !client || !filesystem || !ManagedClient || !rootScope)
                return;
            var directory = filesystem.currentDirectory;
            var target = client;
            rootScope.$evalAsync(function () {
                var started = target.uploads.length;
                chosen.forEach(function (file) {
                    ManagedClient.uploadFile(target, file, filesystem, directory);
                });
                pending = pending.concat(target.uploads.slice(started));
            });
            setPanelStatus(chosen.length === 1
                ? 'Uploading ' + chosen[0].name + '\u2026'
                : 'Uploading ' + chosen.length + ' files\u2026');
        });

        // Uploads started from this panel that have not finished yet
        var pending = [];

        // Report uploads that failed; Guacamole's transfer dialog has details
        function checkUploads() {
            pending = pending.filter(function (upload) {
                var state = upload.transferState.streamState;
                if (state === 'ERROR')
                    setPanelStatus('Could not upload ' + upload.filename
                        + '. The file may be too large, or the drive full.', 'error');
                return state !== 'ERROR' && state !== 'CLOSED';
            });
        }

        // Show finished uploads in the listing
        var watching = false;
        function watchUploads() {
            var rootScope = !watching && guacamoleService('$rootScope');
            if (!rootScope)
                return watching;
            watching = true;
            rootScope.$on('guacUploadComplete', function (event, name) {
                if (!files.isOpen() || !filesystem)
                    return;
                setPanelStatus('Uploaded ' + name + '.', 'success');
                openDirectory(filesystem.currentDirectory);
            });
            return true;
        }

        window.setInterval(poll, 500);

        var show = files.show;
        files.show = function () {
            setStatus(status);
            client = currentClient();
            filesystem = client && client.filesystems.length ? client.filesystems[0] : null;
            show();
            // Fresh listing each time the panel opens
            if (filesystem)
                openDirectory(filesystem.currentDirectory);
            else
                render();
        };
        files.watchUploads = watchUploads;

        return files;
    }

    /* ---------------------------------------------------------------------
     * Sound panel
     *
     * Switching the sound reconnects the session, so the widget button only
     * opens this panel and the switch happens on a second, explicit click.
     * ------------------------------------------------------------------- */

    function createSoundPanel(widget, showSound) {

        var sound = createPanel(widget, 'session-tools-sound');
        var panel = sound.element;

        panel.appendChild(element('div', 'title', 'Sound'));

        var state = element('p', 'state');
        panel.appendChild(state);

        var actions = element('div', 'actions');
        var toggle = addButton(actions, '', '', function () {
            toggle.disabled = true;
            setPanelStatus('Reconnecting…');
            saveSettings({ audio: enabled ? 'off' : 'on' }).then(function () {
                // Reconnect so the remote computer starts or stops the sound
                window.location.reload();
            }, function (error) {
                setPanelStatus(error.message, 'error');
                toggle.disabled = false;
            });
        });
        addButton(actions, 'Cancel', 'Keep the sound as it is', function () {
            sound.hide();
        });
        panel.appendChild(actions);

        var status = element('p', 'status');
        status.setAttribute('aria-live', 'polite');
        panel.appendChild(status);

        panel.appendChild(element('p', 'hint',
            'Switching reconnects the session, which takes a few seconds. '
            + 'Sound off saves bandwidth on slow connections.'));

        function setPanelStatus(message, state) {
            setStatus(status, message, state);
            sound.position();
        }

        var enabled = true;

        function render() {
            state.textContent = enabled
                ? 'Sound from the remote computer is on.'
                : 'Sound from the remote computer is off.';
            toggle.textContent = enabled ? 'Turn sound off' : 'Turn sound on';
            toggle.title = toggle.textContent + ' (reconnects)';
            showSound(enabled);
            sound.position();
        }

        render();
        loadSettings().then(function (saved) {
            enabled = soundEnabled(saved);
            render();
        });

        var show = sound.show;
        sound.show = function () {
            setStatus(status);
            toggle.disabled = false;
            show();
        };

        return sound;
    }

    /* ---------------------------------------------------------------------
     * Widget placement
     * ------------------------------------------------------------------- */

    function clamp(value, min, max) {
        return Math.max(min, Math.min(max, value));
    }

    function placeWidget(widget, left, top) {
        left = clamp(left, 0, window.innerWidth - widget.offsetWidth);
        top = clamp(top, 0, window.innerHeight - widget.offsetHeight);
        widget.style.left = left + 'px';
        widget.style.top = top + 'px';
        widget.style.right = 'auto';
        widget.style.bottom = 'auto';
    }

    function restorePosition(widget) {
        try {
            var saved = JSON.parse(localStorage.getItem(STORAGE_KEY));
            if (saved)
                placeWidget(widget, saved.left, saved.top);
        }
        catch (e) {}
    }

    function savePosition(widget) {
        try {
            localStorage.setItem(STORAGE_KEY, JSON.stringify({
                left: widget.offsetLeft,
                top: widget.offsetTop
            }));
        }
        catch (e) {}
    }

    /* ---------------------------------------------------------------------
     * Widget
     * ------------------------------------------------------------------- */

    function createWidget(settings) {

        var widget = doc.createElement('div');
        widget.id = 'session-tools';

        var meter = doc.createElement('div');
        meter.className = 'ping';
        var dot = doc.createElement('span');
        dot.className = 'ping-dot';
        var label = doc.createElement('span');
        meter.appendChild(dot);
        meter.appendChild(label);
        widget.appendChild(meter);

        var button = null;
        if (fullscreenSupported) {
            button = doc.createElement('button');
            button.type = 'button';
            button.className = 'fullscreen';
            updateFullscreenIcon(button);
            widget.appendChild(button);
            doc.addEventListener('fullscreenchange', function () { updateFullscreenIcon(button); });
            doc.addEventListener('webkitfullscreenchange', function () { updateFullscreenIcon(button); });
        }

        var keysButton = doc.createElement('button');
        keysButton.type = 'button';
        keysButton.className = 'keys-toggle';
        keysButton.title = 'Keys (F1-F12, Ctrl, Alt, Del, Shift)';
        keysButton.innerHTML = ICON_KEYBOARD;
        widget.appendChild(keysButton);

        var clipboardButton = doc.createElement('button');
        clipboardButton.type = 'button';
        clipboardButton.className = 'clipboard-toggle';
        clipboardButton.title = 'Clipboard';
        clipboardButton.innerHTML = ICON_CLIPBOARD;
        widget.appendChild(clipboardButton);

        var filesButton = doc.createElement('button');
        filesButton.type = 'button';
        filesButton.className = 'files-toggle';
        filesButton.title = 'Files (upload and download)';
        filesButton.innerHTML = ICON_FOLDER;
        widget.appendChild(filesButton);

        var soundButton = doc.createElement('button');
        soundButton.type = 'button';
        soundButton.className = 'sound-toggle';
        widget.appendChild(soundButton);

        function showSound(enabled) {
            soundButton.innerHTML = enabled ? ICON_SOUND_ON : ICON_SOUND_OFF;
            soundButton.classList.toggle('off', !enabled);
            soundButton.title = enabled ? 'Sound on' : 'Sound off';
        }
        showSound(true);

        var keys = createKeysPanel(widget);
        var clipboard = createClipboardPanel(widget);
        var files = createFilesPanel(widget);
        var sound = createSoundPanel(widget, showSound);
        var panels = [keys, clipboard, files, sound];

        function togglePanel(panel) {
            var open = panel.isOpen();
            panels.forEach(function (other) { other.hide(); });
            if (!open)
                panel.show();
        }

        var settingsButton = doc.createElement('button');
        settingsButton.type = 'button';
        settingsButton.className = 'settings';
        settingsButton.title = 'Settings';
        settingsButton.innerHTML = ICON_SETTINGS;
        widget.appendChild(settingsButton);

        // Drag anywhere on the widget; a tap on a button triggers it
        var start = null;
        var dragging = false;

        widget.addEventListener('pointerdown', function (e) {
            e.stopPropagation();
            start = {
                x: e.clientX,
                y: e.clientY,
                left: widget.offsetLeft,
                top: widget.offsetTop,
                target: e.target
            };
            dragging = false;
            widget.setPointerCapture(e.pointerId);
        });

        widget.addEventListener('pointermove', function (e) {
            if (!start)
                return;
            var dx = e.clientX - start.x;
            var dy = e.clientY - start.y;
            if (!dragging && Math.abs(dx) + Math.abs(dy) < DRAG_THRESHOLD)
                return;
            dragging = true;
            widget.classList.add('dragging');
            placeWidget(widget, start.left + dx, start.top + dy);
            panels.forEach(function (panel) { panel.position(); });
        });

        widget.addEventListener('pointerup', function (e) {
            e.stopPropagation();
            if (!start)
                return;
            if (dragging)
                savePosition(widget);
            else if (button && button.contains(start.target))
                toggleFullscreen();
            else if (keysButton.contains(start.target))
                togglePanel(keys);
            else if (clipboardButton.contains(start.target))
                togglePanel(clipboard);
            else if (filesButton.contains(start.target))
                togglePanel(files);
            else if (soundButton.contains(start.target))
                togglePanel(sound);
            else if (settingsButton.contains(start.target))
                settings.open();
            start = null;
            dragging = false;
            widget.classList.remove('dragging');
        });

        widget.addEventListener('pointercancel', function () {
            start = null;
            dragging = false;
            widget.classList.remove('dragging');
        });

        // Keep clicks and touches away from the remote display underneath
        ['mousedown', 'mouseup', 'click', 'touchstart', 'touchend'].forEach(function (type) {
            widget.addEventListener(type, function (e) { e.stopPropagation(); });
        });

        // Keep the widget on screen after rotation or resize
        window.addEventListener('resize', function () {
            if (widget.style.left)
                placeWidget(widget, widget.offsetLeft, widget.offsetTop);
        });

        doc.body.appendChild(widget);
        updatePing(meter, label);
        window.setInterval(function () { updatePing(meter, label); }, 1000);

        widget.hidePanels = function () {
            panels.forEach(function (panel) { panel.hide(); });
        };
        widget.watchRemoteClipboard = clipboard.watchRemoteClipboard;
        widget.watchUploads = files.watchUploads;
        return widget;
    }

    function init() {
        var settings = createSettingsDialog();
        var widget = createWidget(settings);

        // Show the widget only while a remote session is open
        function updateVisibility() {
            var inSession = /^#\/client\//.test(window.location.hash);
            widget.classList.toggle('visible', inSession);
            if (inSession)
                restorePosition(widget);
            else {
                settings.hide();
                widget.hidePanels();
            }
        }

        // Guacamole's services exist once its app has started; wait for it
        // to follow text copied on the remote desktop and finished uploads
        var watcher = window.setInterval(function () {
            var clipboardWatched = widget.watchRemoteClipboard();
            if (widget.watchUploads() && clipboardWatched)
                window.clearInterval(watcher);
        }, 1000);

        window.addEventListener('hashchange', updateVisibility);
        updateVisibility();
    }

    if (doc.readyState === 'loading')
        doc.addEventListener('DOMContentLoaded', init);
    else
        init();

})();
