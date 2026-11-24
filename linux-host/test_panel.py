"""Offline GTK controller regressions. Never probe or capture real devices."""
import dataclasses
import sys
import threading
import unittest
from unittest.mock import patch

from panel_model import EventMailbox, PanelController, SessionSettings


def settings(**kw):
    return dataclasses.replace(SessionSettings('/dev/dri/card2', 54, 1), **kw)


class SettingsTests(unittest.TestCase):
    def test_defaults_are_bounded_jpeg_only(self):
        args = settings().arguments()
        self.assertNotIn('--elevate', args)
        self.assertEqual(args[-4:], ['--frames', '300', '--seconds', '15'])
        self.assertNotIn('slot', args)
        self.assertNotIn('h264', args)

    def test_explicit_elevation(self):
        self.assertEqual(settings(elevate=True).arguments()[-1], '--elevate')

    def test_bad_configuration_before_spawn(self):
        invalid = [dict(device='/dev/dri/card2;id'), dict(device='/dev/dri/renderD128'),
                   dict(connector=0), dict(device_id=True), dict(quality=95), dict(quality=True),
                   dict(fps=120), dict(transport='slot'), dict(elevate=1),
                   dict(frames=301), dict(frames=True), dict(seconds=16)]
        for kw in invalid:
            with self.subTest(kw=kw), self.assertRaises(ValueError):
                settings(**kw).arguments()

    def test_mailbox_bounded_and_coalesced(self):
        box = EventMailbox()
        for n in range(200):
            box.put({'event': 'phase', 'n': n})
            box.put({'event': 'metrics', 'frames': n})
        events = box.drain()
        self.assertEqual(len(events), 65)
        self.assertEqual(events[-1]['frames'], 199)
        self.assertFalse(box.drain())

    def test_controller_no_action_on_creation(self):
        with patch('panel_model.subprocess.Popen') as popen:
            controller = PanelController()
            controller.stop()
            popen.assert_not_called()

    def test_owned_real_child_json_exit_and_no_shell(self):
        controller = PanelController()
        controller.launch([sys.executable, '-c',
                           'import json; print(json.dumps({"event":"finished","frames":0}))'])
        controller.thread.join(5)
        self.assertFalse(controller.thread.is_alive())
        self.assertFalse(controller.busy)
        self.assertIsNone(controller.process)
        events = controller.mailbox.drain()
        self.assertEqual(events[0]['event'], 'finished')
        self.assertEqual(events[-1], {'event': 'process_exit', 'code': 0})

    def test_repeated_stop_sends_one_signal_and_waits_for_cleanup(self):
        ready = threading.Event()
        class Mailbox(EventMailbox):
            def put(self, event):
                super().put(event)
                if event.get('event') == 'ready':
                    ready.set()
        controller = PanelController(Mailbox())
        code = ('import signal,time,json\n'
                'def stop(*args):\n'
                ' print(json.dumps({"event":"cleanup","complete":True}),flush=True)\n'
                ' raise SystemExit(0)\n'
                'signal.signal(signal.SIGINT,stop)\n'
                'print(json.dumps({"event":"ready"}),flush=True)\n'
                'time.sleep(20)\n')
        controller.launch([sys.executable, '-u', '-c', code])
        self.assertTrue(ready.wait(5))
        with self.assertRaises(RuntimeError):
            controller.launch([sys.executable, '-c', 'pass'])
        controller.stop()
        controller.stop()
        controller.thread.join(5)
        self.assertFalse(controller.thread.is_alive())
        events = controller.mailbox.drain()
        self.assertTrue(any(e.get('complete') for e in events))
        self.assertEqual(events[-1]['code'], 0)

    def test_spawn_failure_no_busy_leak(self):
        controller = PanelController()
        controller.launch(['/path/that/does/not/exist'])
        controller.thread.join(5)
        self.assertFalse(controller.busy)
        self.assertTrue(any(e['event'] == 'error' for e in controller.mailbox.drain()))


if __name__ == '__main__':
    unittest.main()
