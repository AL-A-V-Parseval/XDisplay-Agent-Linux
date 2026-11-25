#!/bin/sh
set -eu
entry=$(readlink -f -- "$0")
base=$(dirname -- "$entry")
if [ "${1:-}" = --backend-version ]; then
    exec "$base/linux-host/xdisplay-backend" --version
fi
if [ "${1:-}" = --check-runtime ]; then
    for helper in xdisplay-backend vkms-probe vkms-read turbo-jpeg; do
        test -x "$base/linux-host/$helper"
        libs=$(ldd "$base/linux-host/$helper")
        if printf '%s\n' "$libs" | grep -q 'not found'; then
            printf '%s\n' "$libs" >&2
            exit 1
        fi
    done
    /usr/bin/python3 -B -c 'import gi; gi.require_version("Gtk", "4.0"); gi.require_version("Adw", "1"); from gi.repository import Adw, Gtk; assert Gtk.check_version(4,10,0) is None and Adw.get_major_version() == 1 and Adw.get_minor_version() >= 4'
    exec "$base/linux-host/xdisplay-backend" --version
fi
exec /usr/bin/python3 -B "$base/linux-host/panel.py" "$@"
