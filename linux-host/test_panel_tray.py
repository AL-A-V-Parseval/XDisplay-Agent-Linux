"""Offline tray protocol and command tests; use only an isolated D-Bus and fakes."""
import os
from pathlib import Path
import shutil
import subprocess
import sys
import time
import unittest
from unittest.mock import Mock, patch

from panel_model import PanelController, ROOT

try:
    from gi.repository import Gio, GLib
except ImportError:
    Gio = GLib = None
else:
    from panel_tray import ITEM, ITEM_PATH, MENU, MENU_PATH, PROPERTIES, WATCHER, WATCHER_PATH, PanelTray


class AutoCommandTests(unittest.TestCase):
    def test_auto_uses_single_native_backend_with_selected_mode(self):
        controller = PanelController()
        with patch.object(controller, 'launch') as launch:
            controller.start_auto('1920x1080', '1.25')
            controller.start_auto('2360x1640', '1.5', elevate=True)
        self.assertEqual([call.args[0] for call in launch.call_args_list], [
            [str(ROOT / 'xdisplay-backend'), 'auto', '--confirm', '--resolution', '1920x1080', '--scale', '1.25'],
            [str(ROOT / 'xdisplay-backend'), 'auto', '--confirm', '--resolution', '2360x1640', '--scale', '1.5',
             '--elevate'],
        ])

    def test_invalid_auto_settings_do_not_launch(self):
        controller = PanelController()
        with patch.object(controller, 'launch') as launch:
            for resolution, scale, elevate in [('1x1', '1', False), ('1024x768', '0', True), ('1024x768', '1', 1)]:
                with self.assertRaises(ValueError):
                    controller.start_auto(resolution, scale, elevate=elevate)
            launch.assert_not_called()


@unittest.skipUnless(Gio and shutil.which('dbus-daemon'), 'Gio and a private dbus-daemon required')
class TrayProtocolTests(unittest.TestCase):
    def setUp(self):
        bus = Gio.TestDBus.new(Gio.TestDBusFlags.NONE)
        bus.up()
        self.addCleanup(bus.down)
        flags = Gio.DBusConnectionFlags.AUTHENTICATION_CLIENT | Gio.DBusConnectionFlags.MESSAGE_BUS_CONNECTION
        self.host, self.client = [Gio.DBusConnection.new_for_address_sync(bus.get_bus_address(), flags, None, None)
                                  for _ in range(2)]
        for conn in (self.host, self.client):
            self.addCleanup(conn.close_sync, None)
        self.available, self.show, self.pause, self.quit = Mock(), Mock(), Mock(), Mock()
        self.registered = []
        self.has_host = False
        self.tray = PanelTray(self.client, show=self.show, pause=self.pause, quit=self.quit,
                              availability=self.available)
        self.addCleanup(self.tray.close)

    def wait_for(self, predicate):
        end = time.monotonic() + 3
        context = GLib.MainContext.default()
        while time.monotonic() < end:
            if predicate():
                return
            if context.pending():
                context.iteration(False)
            else:
                time.sleep(0.001)
        self.fail('D-Bus callback did not complete')

    def call(self, path, interface, method, args=None):
        received = []
        self.host.call(self.client.get_unique_name(), path, interface, method, args,
                       None, Gio.DBusCallFlags.NONE, 2000, None,
                       lambda conn, result: received.append(result))
        self.wait_for(lambda: received)
        return self.host.call_finish(received[0]).unpack()

    def publish_watcher(self):
        interface = Gio.DBusNodeInfo.new_for_xml(f'''
        <node><interface name="{WATCHER}">
          <method name="RegisterStatusNotifierItem"><arg type="s" direction="in"/></method>
          <property name="IsStatusNotifierHostRegistered" type="b" access="read"/>
        </interface></node>''').interfaces[0]

        def register(_conn, sender, _path, _interface, _method, params, invocation):
            self.registered.append((sender, params.unpack()[0]))
            invocation.return_value(None)

        self.host.register_object(WATCHER_PATH, interface, register,
                                  lambda *_: GLib.Variant('b', self.has_host), None)
        self.host.call_sync('org.freedesktop.DBus', '/org/freedesktop/DBus', 'org.freedesktop.DBus',
                            'RequestName', GLib.Variant('(su)', (WATCHER, 0)), None,
                            Gio.DBusCallFlags.NONE, 2000, None)

    def host_signal(self, member):
        self.host.emit_signal(None, WATCHER_PATH, WATCHER, member, None)

    def test_requires_watcher_registration_and_registered_host(self):
        self.wait_for(lambda: self.available.called)
        self.assertFalse(self.tray.ready)
        self.available.reset_mock()
        self.publish_watcher()
        self.wait_for(lambda: self.available.called)
        self.assertEqual(self.registered, [(self.client.get_unique_name(), ITEM_PATH)])
        self.assertFalse(self.tray.ready)
        self.has_host = True
        self.host_signal('StatusNotifierHostRegistered')
        self.wait_for(lambda: self.tray.ready)
        self.has_host = False
        self.host_signal('StatusNotifierHostUnregistered')
        self.wait_for(lambda: not self.tray.ready)
        self.has_host = True
        self.host_signal('StatusNotifierHostRegistered')
        self.wait_for(lambda: self.tray.ready)
        self.host.call_sync('org.freedesktop.DBus', '/org/freedesktop/DBus', 'org.freedesktop.DBus',
                            'ReleaseName', GLib.Variant('(s)', (WATCHER,)), None,
                            Gio.DBusCallFlags.NONE, 2000, None)
        self.wait_for(lambda: not self.tray.ready)

    def test_menu_shapes_activation_pause_and_quit(self):
        self.assertEqual(self.call(ITEM_PATH, PROPERTIES, 'Get', GLib.Variant('(ss)', (ITEM, 'Menu')),
                                   ), (MENU_PATH,))
        self.call(ITEM_PATH, ITEM, 'Activate', GLib.Variant('(ii)', (0, 0)))
        self.show.assert_called_once_with()
        self.call(MENU_PATH, MENU, 'Event', GLib.Variant('(isvu)', (3, 'clicked', GLib.Variant('s', ''), 0)))
        self.pause.assert_not_called()
        self.tray.update('等待 USB iPad', True)
        revision, (root, properties, children) = self.call(
            MENU_PATH, MENU, 'GetLayout', GLib.Variant('(iias)', (0, 1, [])))
        self.assertEqual((revision, root, properties['children-display']), (1, 0, 'submenu'))
        by_id = {i: props for i, props, _children in children}
        self.assertEqual(by_id[1]['label'], '等待 USB iPad')
        self.assertTrue(by_id[3]['enabled'])
        self.call(MENU_PATH, MENU, 'EventGroup', GLib.Variant('(a(isvu))',
                  ([(i, 'clicked', GLib.Variant('s', ''), 0) for i in (3, 4)],)))
        self.pause.assert_called_once_with()
        self.quit.assert_called_once_with()


def exercise_gtk_lifecycle():
    # This process runs on a private X server and D-Bus, with no native backend.
    from panel import PanelApplication, PanelWindow

    app = PanelApplication()
    assert app.register(None)
    app.activate()
    window = app.get_windows()[0]
    assert not window.auto_button.get_sensitive()
    assert window.transport.get_selected() == 0
    assert window.resolution.get_sensitive()
    assert not window.apply_output_button.get_sensitive()

    controller = window.controller
    with patch('panel.Gtk.AlertDialog') as alert, patch.object(controller, 'start_auto') as start:
        window.confirm_auto()
        alert.assert_not_called()
        window.tray._set_ready(True, '')
        window.confirm_auto()
        dialog = alert.return_value
        answer = dialog.choose.call_args.args[2]
        dialog.choose_finish.return_value = 1
        window.tray._set_ready(False, 'No host')
        answer(dialog, None)
        start.assert_not_called()
        window.tray._set_ready(True, '')
        window.confirm_auto()
        answer = dialog.choose.call_args.args[2]
        answer(dialog, None)
        start.assert_called_once_with('2360x1640', '1.5', elevate=False)

    controller.busy = True
    window.close()
    assert not window.get_visible() and not controller.stop_requested
    app.activate()
    assert app.get_windows() == [window] and window.get_visible()
    window.close()
    window.tray._set_ready(False, 'No host')
    assert window.get_visible() and controller.stop_requested
    assert not window.auto_button.get_sensitive()
    assert window.poll() == GLib.SOURCE_CONTINUE
    controller.busy = False
    controller.mailbox.put({'event': 'process_exit', 'code': 0})
    window.poll()
    assert not window.auto_active and window.get_visible() and not window.auto_button.get_sensitive()

    # Tray Pause must not make the panel exit; explicit quit must await child cleanup.
    window.auto_active = True
    controller.busy = True
    controller.stop_requested = False
    window.request_quit()
    assert controller.stop_requested and window.poll() == GLib.SOURCE_CONTINUE
    assert app.get_windows() == [window]
    GLib.source_remove(window.timer)  # Poll manually in this fixture.
    controller.busy = False
    controller.mailbox.put({'event': 'cleanup', 'complete': True})
    controller.mailbox.put({'event': 'process_exit', 'code': 0})
    assert window.poll() == GLib.SOURCE_REMOVE and not app.get_windows()

    for kwargs in ({'preview': True}, {'diagnostic_dir': Path('/unused-diagnostic')}):
        app = PanelApplication(**kwargs)
        app.set_application_id('io.github.wired_display.Fixture' + ('Preview' if app.preview else 'Diagnostic'))
        assert app.register(None)
        window = PanelWindow(app, preview=app.preview)
        assert window.auto_button is None and window.tray is None
        assert window.transport.get_selected() == int(app.diagnostic_dir is not None)
        window.request_quit()


@unittest.skipUnless(Gio and all(shutil.which(tool) for tool in ('dbus-run-session', 'xvfb-run', 'Xvfb', 'xauth')),
                     'Gio, private D-Bus and Xvfb required')
class GtkLifecycleTests(unittest.TestCase):
    def test_hide_pause_and_quit_with_owned_child(self):
        result = subprocess.run(['dbus-run-session', '--', 'xvfb-run', '-a', sys.executable, '-B',
                                 __file__, '--gtk-fixture'], capture_output=True, text=True, timeout=15,
                                env={**os.environ, 'GDK_BACKEND': 'x11', 'GTK_A11Y': 'none'})
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)


if __name__ == '__main__':
    if sys.argv[1:] == ['--gtk-fixture']:
        exercise_gtk_lifecycle()
    else:
        unittest.main()
