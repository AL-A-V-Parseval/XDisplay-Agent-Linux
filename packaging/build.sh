#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
test "$(uname -m)" = x86_64
version=$(linux-host/xdisplay-backend --version | awk '{print $2}')
[[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]
: "${APPIMAGETOOL:?Set APPIMAGETOOL to the verified appimagetool executable}"
export APPIMAGE_EXTRACT_AND_RUN=1
export SOURCE_DATE_EPOCH=$(git log -1 --format=%ct)
name=XDisplay-Agent-Linux-${version}-x86_64
id=io.github.wired_display.ControlPanel
out="$PWD/dist"
mkdir -p "$out"
work=$(mktemp -d)
app="$work/$name"
deb="$work/deb"

install -d "$app/linux-host"
install -m755 packaging/launch.sh "$app/xdisplay-agent-linux"
install -m755 linux-host/{xdisplay-backend,vkms-probe,vkms-read,turbo-jpeg} "$app/linux-host/"
install -m644 linux-host/{panel.py,panel_model.py,panel_tray.py} "$app/linux-host/"
install -m644 LICENSE README.md "$app/"
tar --sort=name --owner=0 --group=0 --numeric-owner --mtime="@$SOURCE_DATE_EPOCH" \
    -C "$work" -cf - "$name" | gzip -n > "$out/$name.tar.gz"

install -d "$deb"/{DEBIAN,usr/bin,usr/lib/xdisplay-agent-linux,usr/share/applications,usr/share/icons/hicolor/scalable/apps,usr/share/doc/xdisplay-agent-linux}
cp -r "$app/linux-host" "$deb/usr/lib/xdisplay-agent-linux/"
install -m755 packaging/launch.sh "$deb/usr/lib/xdisplay-agent-linux/xdisplay-agent-linux"
ln -s ../lib/xdisplay-agent-linux/xdisplay-agent-linux "$deb/usr/bin/xdisplay-agent-linux"
install -m755 linux-host/vkms-read "$deb/usr/lib/xdisplay-agent-linux/vkms-read"
install -m644 "packaging/$id.desktop" "$deb/usr/share/applications/"
install -m644 "packaging/$id.svg" "$deb/usr/share/icons/hicolor/scalable/apps/"
install -m644 LICENSE "$deb/usr/share/doc/xdisplay-agent-linux/copyright"
install -m644 README.md "$deb/usr/share/doc/xdisplay-agent-linux/"
printf '%s\n' 'Package: xdisplay-agent-linux' "Version: $version" 'Architecture: amd64' \
    'Maintainer: AL-A-V-Parseval <138087423+AL-A-V-Parseval@users.noreply.github.com>' \
    'Section: utils' 'Priority: optional' \
    'Depends: libc6 (>= 2.39), libgcc-s1, libdrm2, libturbojpeg0, python3, python3-gi, gir1.2-gtk-4.0 (>= 4.10), gir1.2-adw-1 (>= 1.4), pkexec, kmod, usbmuxd' \
    'Description: Linux host for a wired iPad second display using niri and VKMS' \
    > "$deb/DEBIAN/control"
dpkg-deb --root-owner-group --build "$deb" "$out/xdisplay-agent-linux_${version}_amd64.deb"

mv "$app/xdisplay-agent-linux" "$app/AppRun"
install -m644 "packaging/$id.desktop" "$app/"
rsvg-convert -w 128 -h 128 "packaging/$id.svg" -o "$app/$id.png"
ln -s "$id.png" "$app/.DirIcon"
ARCH=x86_64 "$APPIMAGETOOL" --no-appstream "$app" "$out/$name.AppImage"
chmod +x "$out/$name.AppImage"

# Smoke-test the archive and package layouts, and the actual no-FUSE AppImage runtime.
mkdir "$work/check"
tar -xzf "$out/$name.tar.gz" -C "$work/check"
"$work/check/$name/xdisplay-agent-linux" --check-runtime
dpkg-deb -x "$out/xdisplay-agent-linux_${version}_amd64.deb" "$work/check/deb"
"$work/check/deb/usr/bin/xdisplay-agent-linux" --check-runtime
"$out/$name.AppImage" --appimage-extract-and-run --check-runtime
(cd "$out" && sha256sum -- "$name.tar.gz" "xdisplay-agent-linux_${version}_amd64.deb" "$name.AppImage" > SHA256SUMS)
