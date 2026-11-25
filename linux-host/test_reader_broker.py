"""Exercise the real C broker without root, DRM device access or Polkit."""
import array
from contextlib import contextmanager
import os
from pathlib import Path
import socket
import struct
import subprocess
import unittest

HELPER = Path(__file__).resolve().with_name('vkms-read')


@contextmanager
def broker():
    control, peer = socket.socketpair(type=socket.SOCK_SEQPACKET)
    with control, peer:
        control.settimeout(3)
        with subprocess.Popen([HELPER, '--broker-v1'], stdout=peer,
                              stdin=subprocess.DEVNULL, stderr=subprocess.PIPE) as process:
            peer.close()
            try:
                if control.recv(2) != b'R':
                    raise AssertionError('helper did not become ready')
                yield control, process
            finally:
                control.close()
                try:
                    process.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
                    raise


def capture(control, data, payload):
    return control.sendmsg([payload], [(socket.SOL_SOCKET, socket.SCM_RIGHTS, array.array('i', [data.fileno()]))])


@unittest.skipIf(os.geteuid() == 0, 'broker requires a normal-user controller')
class ReaderBrokerTests(unittest.TestCase):
    def test_same_process_accepts_two_sessions_then_exits_on_eof(self):
        with broker() as (control, process):
            for card in (1, 2):
                client, data = socket.socketpair()
                with client, data:
                    client.settimeout(3)
                    # A zero connector exercises refusal without opening a DRM node.
                    self.assertEqual(capture(control, data, struct.pack('<II', card, 0)), 8)
                    data.close()
                    self.assertEqual(control.recv(2), b'D')
                    self.assertEqual(client.recv(1), b'')
                    self.assertIsNone(process.poll())
        self.assertEqual(process.returncode, 0)

    def test_rejects_invalid_capture_socket_and_payload(self):
        for mode, size in ((socket.SOCK_DGRAM, 8), (socket.SOCK_STREAM, 9)):
            with self.subTest(mode=mode, size=size), broker() as (control, process):
                client, data = socket.socketpair(type=mode)
                with client, data:
                    capture(control, data, bytes(size))
                    self.assertEqual(process.wait(timeout=3), 2)

    def test_rejects_non_socket_control(self):
        result = subprocess.run([HELPER, '--broker-v1'], capture_output=True, timeout=3)
        self.assertEqual(result.returncode, 2)


if __name__ == '__main__':
    unittest.main()
