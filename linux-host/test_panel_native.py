"""Native runtime boundary tests; never enumerate/open devices or capture pixels."""
import dataclasses
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch
from panel_model import PanelController, ROOT, SessionSettings


class NativeBackendTests(unittest.TestCase):
    def test_all_gtk_actions_spawn_native_binary_only(self):
        controller = PanelController()
        settings = SessionSettings('/dev/dri/card0', 54, 1, elevate=True)
        with patch.object(controller, 'launch') as launch:
            controller.discover()
            controller.create_output()
            controller.configure_output('/dev/dri/card0', 54, '2360x1640', '1.5')
            controller.start(settings)
        commands = [c.args[0] for c in launch.call_args_list]
        self.assertEqual(commands, [
            [str(ROOT / 'xdisplay-backend'), 'discover'],
            [str(ROOT / 'xdisplay-backend'), 'create-output', '--confirm'],
            [str(ROOT / 'xdisplay-backend'), 'configure-output', '--confirm', '--device',
             '/dev/dri/card0', '--connector', '54', '--resolution', '2360x1640', '--scale', '1.5'],
            [str(ROOT / 'xdisplay-backend'), 'stream', *settings.arguments()]])
        for command in commands:
            self.assertFalse(any(value.endswith('.py') or 'python' in value.lower() for value in command))

    def test_diagnostic_uses_separate_native_binary(self):
        controller = PanelController()
        settings = SessionSettings('/dev/dri/card0', 54, 1, elevate=True)
        with patch.object(controller, 'launch') as launch:
            controller.start_diagnostic(settings, Path('/tmp/fresh-roi-run'))
        launch.assert_called_once_with([
            str(ROOT / 'rust-backend' / 'target' / 'roi-diagnostic' / 'release' / 'xdisplay-backend'),
            'stream', *settings.arguments(), '--diagnostic-dir', '/tmp/fresh-roi-run'])

    def test_invalid_frontend_settings_rejected_before_launch(self):
        with patch.object(PanelController, 'launch') as launch:
            with self.assertRaises(ValueError):
                PanelController().start(dataclasses.replace(SessionSettings('/dev/dri/card0', 54, 1), frames=301))
        launch.assert_not_called()

    def test_executable_is_elf_and_needs_no_python_in_path(self):
        binary = ROOT / 'xdisplay-backend'
        self.assertTrue(os.access(binary, os.X_OK))
        with binary.open('rb') as file:
            self.assertEqual(file.read(4), b'\x7fELF')
        with tempfile.TemporaryDirectory() as directory:
            result = subprocess.run([str(binary), '--version'], cwd=directory,
                                    env={'PATH': '/no-interpreters-here'},
                                    capture_output=True, timeout=5, check=True)
        self.assertIn(b'Rust; JPEG-only', result.stdout)
        self.assertNotIn(b'Python', result.stderr)

    def test_old_python_backend_entry_is_absent(self):
        self.assertFalse((ROOT / 'panel_service.py').exists())
        self.assertFalse((ROOT / 'panel_output.py').exists())

    def test_real_native_invalid_request_reports_json_without_hardware(self):
        controller = PanelController()
        controller.launch([str(ROOT / 'xdisplay-backend'), 'create-output'])
        controller.thread.join(5)
        self.assertFalse(controller.thread.is_alive())
        events = controller.mailbox.drain()
        self.assertTrue(any(e['event'] == 'error' for e in events))
        self.assertEqual(events[-1], {'event': 'process_exit', 'code': 1})
        self.assertIsNone(controller.process)
        self.assertFalse(controller.busy)


if __name__ == '__main__':
    unittest.main()
