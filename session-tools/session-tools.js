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
 * Drag the widget to move it; its position is remembered per browser.
 */
(function () {

    var STORAGE_KEY = 'session-tools-position';
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
            var socket = arguments.length > 1
                ? new NativeWebSocket(url, protocols)
                : new NativeWebSocket(url);
            if (/\/websocket-tunnel/.test(url)) {
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

    function createWidget() {

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

        // Drag anywhere on the widget; a tap on the button toggles fullscreen
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
        });

        widget.addEventListener('pointerup', function (e) {
            e.stopPropagation();
            if (!start)
                return;
            if (dragging)
                savePosition(widget);
            else if (button && button.contains(start.target))
                toggleFullscreen();
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

        return widget;
    }

    function init() {
        var widget = createWidget();

        // Show the widget only while a remote session is open
        function updateVisibility() {
            var inSession = /^#\/client\//.test(window.location.hash);
            widget.classList.toggle('visible', inSession);
            if (inSession)
                restorePosition(widget);
        }

        window.addEventListener('hashchange', updateVisibility);
        updateVisibility();
    }

    if (doc.readyState === 'loading')
        doc.addEventListener('DOMContentLoaded', init);
    else
        init();

})();
