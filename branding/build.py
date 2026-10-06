#!/usr/bin/env python3
"""
Builds branding.jar, a Guacamole extension that hides the Guacamole look:

  - Replaces "Apache Guacamole" / "Guacamole" with a neutral name in every UI
    string of every language (page title, login box, status and error texts).
  - Hides the Guacamole logo and the version number on the login page.
  - Swaps the Guacamole favicon for a plain one.

The texts are read from the Guacamole web app (guacamole.war), so the result
always matches the installed Guacamole version.

Usage: build.py <guacamole.war> <output.jar> <brand name>
"""
import json
import os
import re
import struct
import sys
import zipfile
import zlib

HERE = os.path.dirname(os.path.abspath(__file__))
ICON_COLOR = (55, 71, 79)


def rebrand(node, brand):
    """Keeps only the strings that mention Guacamole, with the name replaced."""
    out = {}
    for key, value in node.items():
        if isinstance(value, dict):
            child = rebrand(value, brand)
            if child:
                out[key] = child
        elif isinstance(value, str) and "Guacamole" in value:
            out[key] = re.sub(r"(Apache )?Guacamole", brand, value)
    return out


def png(size, rgb):
    """Returns a solid-color square PNG."""
    def chunk(kind, data):
        body = kind + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body))
    row = b"\x00" + bytes(rgb) * size
    return (b"\x89PNG\r\n\x1a\n"
            + chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 2, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(row * size, 9))
            + chunk(b"IEND", b""))


def main():
    if len(sys.argv) != 4:
        sys.exit(__doc__.strip())
    war_path, jar_path, brand = sys.argv[1:]

    translations = {}
    with zipfile.ZipFile(war_path) as war:
        for name in sorted(war.namelist()):
            if re.fullmatch(r"translations/[\w-]+\.json", name):
                strings = rebrand(json.loads(war.read(name)), brand)
                strings.setdefault("APP", {})["NAME"] = brand
                translations[name] = json.dumps(strings, ensure_ascii=False, indent=2)
    if not translations:
        sys.exit(f"no translations found in {war_path}")

    manifest = {
        "guacamoleVersion": "*",
        "name": "Branding",
        "namespace": "branding",
        "js": ["branding.js"],
        "css": ["branding.css"],
        "translations": sorted(translations),
        "resources": {"icon-64.png": "image/png", "icon-144.png": "image/png"},
    }

    # Write to a temporary file first so a running Guacamole never sees half a jar
    with zipfile.ZipFile(jar_path + ".tmp", "w", zipfile.ZIP_DEFLATED) as jar:
        jar.writestr("guac-manifest.json", json.dumps(manifest, indent=2))
        jar.write(os.path.join(HERE, "branding.js"), "branding.js")
        jar.write(os.path.join(HERE, "branding.css"), "branding.css")
        for name, content in translations.items():
            jar.writestr(name, content)
        jar.writestr("icon-64.png", png(64, ICON_COLOR))
        jar.writestr("icon-144.png", png(144, ICON_COLOR))
    os.replace(jar_path + ".tmp", jar_path)

    print(f'==> built {jar_path}: brand "{brand}", {len(translations)} languages')


if __name__ == "__main__":
    main()
