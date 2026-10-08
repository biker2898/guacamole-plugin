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
 *  - A settings button. It opens a dialog to pick the screen resolution of
 *    the remote desktop and to change the user's Guacamole password.
 *
 * Drag the widget to move it; its position is remembered per browser.
 */
(function () {

    var STORAGE_KEY = 'session-tools-position';
    var RESOLUTION_KEY = 'session-tools-resolution';
    var COOKIE_MAX_AGE = 10 * 365 * 24 * 3600;
    var DRAG_THRESHOLD = 6;

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
    // Fixed resolutions offered in the settings dialog, besides "fit window"
    var RESOLUTIONS = [
        [1280, 720], [1366, 768], [1440, 900], [1600, 900],
        [1920, 1080], [2560, 1440]
    ];

    var MIN_PASSWORD_LENGTH = 8;

    // X11 keysyms, as Guacamole sends them
    var KEY_CTRL = 0xFFE3;
    var KEY_ALT = 0xFFE9;
    var KEY_SHIFT = 0xFFE1;
    var KEY_DELETE = 0xFFFF;
    var KEY_F1 = 0xFFBE;

    var ICON_KEYBOARD = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">'
        + '<rect x="2" y="5" width="20" height="14" rx="2"/><path d="M6 9h.01M10 9h.01M14 9h.01M18 9h.01M6 13h.01M18 13h.01M10 13h4M7 16h10"/></svg>';

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
            if (isTunnel)
                url = applyResolution(url);
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
     * The browser sends the desired screen size when the tunnel connects
     * (GUAC_WIDTH, GUAC_HEIGHT and GUAC_DPI in the WebSocket URL). A chosen
     * resolution replaces those values, so it applies from the next
     * connection on. Connections that set "width" and "height" themselves
     * ignore it.
     *
     * The choice is kept in a cookie on the parent domain as well as in
     * localStorage, so sites that share that domain (portal.example.com and
     * portal2.example.com) share the choice too. The cookie holds "auto" for
     * "fit browser window", so a later choice on one site overrides an older
     * one from another.
     * ------------------------------------------------------------------- */

    function readResolutionCookie() {
        var match = doc.cookie.match(new RegExp('(?:^|;\\s*)' + RESOLUTION_KEY + '=([^;]*)'));
        return match ? match[1] : null;
    }

    function writeResolutionCookie(value) {
        var attributes = '; path=/; max-age=' + COOKIE_MAX_AGE + '; samesite=lax'
            + (window.location.protocol === 'https:' ? '; secure' : '');

        // Widest domain the browser accepts; it refuses public suffixes
        var parts = window.location.hostname.split('.');
        for (var i = Math.max(parts.length - 2, 0); i > 0; i--) {
            doc.cookie = RESOLUTION_KEY + '=' + value + '; domain=' + parts.slice(i).join('.') + attributes;
            if (readResolutionCookie() === value)
                return;
        }
        doc.cookie = RESOLUTION_KEY + '=' + value + attributes;
    }

    function savedResolution() {
        var cookie = readResolutionCookie();
        var match = cookie && /^(\d+)x(\d+)$/.exec(cookie);
        if (match)
            return { width: parseInt(match[1], 10), height: parseInt(match[2], 10) };
        if (cookie === 'auto')
            return null;
        try {
            var saved = JSON.parse(localStorage.getItem(RESOLUTION_KEY));
            if (saved && saved.width > 0 && saved.height > 0)
                return saved;
        }
        catch (e) {}
        return null;
    }

    function saveResolution(resolution) {
        writeResolutionCookie(resolution ? resolution.width + 'x' + resolution.height : 'auto');
        try {
            if (resolution)
                localStorage.setItem(RESOLUTION_KEY, JSON.stringify(resolution));
            else
                localStorage.removeItem(RESOLUTION_KEY);
        }
        catch (e) {}
    }

    function setQueryParam(url, name, value) {
        var pattern = new RegExp('([?&]' + name + '=)[^&]*');
        if (pattern.test(url))
            return url.replace(pattern, '$1' + value);
        return url + (url.indexOf('?') < 0 ? '?' : '&') + name + '=' + value;
    }

    function applyResolution(url) {
        var resolution = savedResolution();
        if (!resolution)
            return url;
        url = setQueryParam(url, 'GUAC_WIDTH', resolution.width);
        url = setQueryParam(url, 'GUAC_HEIGHT', resolution.height);
        // A fixed size is meant in real pixels; keep Windows at 100% scaling
        return setQueryParam(url, 'GUAC_DPI', 96);
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
        RESOLUTIONS.forEach(function (size) {
            select.appendChild(new Option(size[0] + ' \u00d7 ' + size[1], size[0] + 'x' + size[1]));
        });
        resolutionForm.appendChild(select);
        resolutionForm.appendChild(element('p', 'hint', 'Applying reconnects the session. Open programs stay open.'));
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
            var resolution = savedResolution();
            select.value = resolution ? resolution.width + 'x' + resolution.height : '';
            if (select.selectedIndex < 0)
                select.value = '';
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
            var parts = select.value.split('x');
            saveResolution(select.value
                ? { width: parseInt(parts[0], 10), height: parseInt(parts[1], 10) }
                : null);
            // Reconnect so the new size is sent to the remote desktop
            window.location.reload();
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

        var panel = element('div');
        panel.id = 'session-tools-keys';

        var latched = {};
        var modifierButtons = {};

        function releaseModifiers() {
            Object.keys(latched).forEach(function (keysym) {
                sendKey(false, parseInt(keysym, 10));
                modifierButtons[keysym].classList.remove('active');
            });
            latched = {};
        }

        function pressKey(keysym) {
            sendKey(true, keysym);
            sendKey(false, keysym);
            releaseModifiers();
        }

        function addButton(row, text, title, onPress) {
            var button = element('button', null, text);
            button.type = 'button';
            button.title = title;
            button.addEventListener('click', onPress);
            row.appendChild(button);
            return button;
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

        // Keep focus where it is, and clicks and touches away from the
        // remote display underneath
        panel.addEventListener('mousedown', function (e) { e.preventDefault(); });
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

        function hide() {
            panel.classList.remove('visible');
            releaseModifiers();
        }

        window.addEventListener('resize', position);

        return {
            toggle: function () {
                if (isOpen())
                    hide();
                else {
                    panel.classList.add('visible');
                    position();
                }
            },
            hide: hide,
            position: position
        };
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

        var keys = createKeysPanel(widget);

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
            keys.position();
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
                keys.toggle();
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

        widget.keys = keys;
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
                widget.keys.hide();
            }
        }

        window.addEventListener('hashchange', updateVisibility);
        updateVisibility();
    }

    if (doc.readyState === 'loading')
        doc.addEventListener('DOMContentLoaded', init);
    else
        init();

})();
