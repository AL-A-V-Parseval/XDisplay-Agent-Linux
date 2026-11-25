# XDisplay-Agent-Linux

A Linux host for using an iPad as a **wired, independent second display** with the official Splashtop Wired XDisplay app. It sends a VKMS virtual output over USB rather than mirroring a physical screen. This is an independent project, not affiliated with Splashtop.

## Requirements

- Linux with DRM/VKMS, a **niri** Wayland session, and an active `usbmuxd` service. The host does not start the service or pair the iPad for you.
- Python 3 with PyGObject, GTK 4 (4.10 or newer), and libadwaita (1.4 or newer). Automatic mode needs a StatusNotifierItem tray host; manual mode does not.
- To build: a current stable Rust toolchain with Cargo (tested with Rust 1.100), Make, a C compiler, `pkg-config`, and the development headers for libdrm and libturbojpeg.
- An iPad with the official Wired XDisplay app and a data-capable cable. Only connect one candidate USB device at a time. A graphical Polkit agent is needed if a VKMS operation requires temporary authorization.

## Release downloads (Linux x86_64)

Download from [GitHub Releases](https://github.com/AL-A-V-Parseval/XDisplay-Agent-Linux/releases). These builds target Ubuntu 24.04 or compatible systems with glibc 2.39+; niri, VKMS and a running usbmuxd are still required.

- **`.deb`**: install with `sudo apt install ./xdisplay-agent-linux_0.1.0_amd64.deb`, then launch `xdisplay-agent-linux` or use the application menu. This also installs the system reader.
- **`.tar.gz`**: unpack it and run `./xdisplay-agent-linux` from the extracted directory.
- **`.AppImage`**: `chmod +x XDisplay-Agent-Linux-*.AppImage`, then run the downloaded file. If FUSE is unavailable, pass `--appimage-extract-and-run`.

The archive and AppImage use system Python/PyGObject, GTK/libadwaita, libdrm and libturbojpeg. On Ubuntu 24.04, install `python3-gi gir1.2-gtk-4.0 gir1.2-adw-1 libdrm2 libturbojpeg0 pkexec kmod usbmuxd` with `apt`. Use `--check-runtime` to check the GUI and native dependencies without accessing hardware. When using temporary authorization, install the reader from the matching tar archive as described below; the portable formats do not install privileged components automatically.

## Build and run

```sh
make
/usr/bin/python3 -B linux-host/panel.py
```

The build writes executable files only inside the checkout. Do not launch the panel or Rust backend with `sudo`.

1. Open Wired XDisplay on the iPad and connect it by USB.
2. Choose a virtual-output resolution and scale in the panel, then confirm automatic connection. The selected resolution must match the app's negotiated dimensions.
3. Close the window to keep the session in the tray. Use the tray menu to reopen, pause, or quit. If the cable is unplugged, the backend releases its capture and encoding resources before waiting for reconnection.

With a working tray host, closing the window hides it even when idle; use Quit in the tray menu or Ctrl+Q to exit. Without a tray host, closing the window exits normally. Temporary VKMS reader authorization is selected by default in the panel and can be turned off; Polkit authorization is still required when selected.

Manual mode can discover an existing VKMS output and run a bounded stream. The panel disables a virtual output it enabled when streaming ends or on explicit exit. Automatic mode does the same before waiting to reconnect. Previously enabled outputs are not disabled; the kernel module is not unloaded, and the last active desktop display is protected. The host rejects ambiguous USB or VKMS targets and does not install a service or configure login autostart.

## One authorization per automatic session

Temporary reader authorization requires the dedicated helper in a root-owned system directory. The `.deb` already installs it. Otherwise, build as a normal user or extract the matching tar archive, then install only this binary once:

```sh
make
sudo install -d -o root -g root -m 0755 /usr/lib/xdisplay-agent-linux
sudo install -o root -g root -m 0755 linux-host/vkms-read /usr/lib/xdisplay-agent-linux/vkms-read
```

After reopening the panel, start automatic connection. The first successful iPad handshake requests Polkit authorization; subsequent reconnections reuse the same helper. Disconnect closes DRM handles and releases frame buffers before output cleanup. Pausing, quitting, or ten minutes without a capture session ends the helper; the next connection may require authorization again. If VKMS is not loaded, loading it can require a separate authorization. Manual test mode still uses one reader per test. No password, reusable Polkit grant, or service is installed; the system copy is not setuid. Reinstall it when updating the helper.

## Scope and testing

JPEG video only; no H.264, touch injection, audio, or Android support in this repository. Continuous tray operation and reconnection are covered by offline tests; long-running real-iPad tray use has not been validated. The ordinary stream does not save frames. Optional diagnostics are separate and not enabled in the default build.

```sh
make test
```

Tests use synthetic peers and frames; no iPad or VKMS device is required. Running the full GTK tests also requires `dbus-run-session` and Xvfb. See [LICENSE](LICENSE) for the project's WTFPL v2 terms. Dependencies retain their own licenses.
