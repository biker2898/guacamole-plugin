/*
 * Replaces the Guacamole favicon and home-screen icon with the plain icons
 * bundled in this extension.
 */
(function () {
    var links = document.querySelectorAll('link[rel~="icon"], link[rel="apple-touch-icon"]');
    for (var i = 0; i < links.length; i++) {
        var href = links[i].getAttribute('href');
        var match = /images\/logo-(64|144)\.png$/.exec(href || '');
        if (match)
            links[i].setAttribute('href', 'app/ext/branding/icon-' + match[1] + '.png');
    }
})();
