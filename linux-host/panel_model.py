"""GTK-independent settings, bounded event mailbox and owned child controller.

Importing this module never enumerates hardware or starts capture.
"""
from __future__ import annotations

from collections import deque
from dataclasses import dataclass
import json
from pathlib import Path
import re
import signal
import subprocess
import threading

ROOT = Path(__file__).resolve().parent
OUTPUT_RESOLUTIONS = ('1024x768', '1600x1200', '1920x1080', '2360x1640')
OUTPUT_SCALES = ('1', '1.25', '1.5', '1.75', '2')


@dataclass(frozen=True)
class SessionSettings:
    device: str
    connector: int
    device_id: int
    quality: int = 85
    fps: int = 60
    transport: str = 'socket-direct'
    elevate: bool = False
    frames: int = 300
    seconds: int = 15

    def validate(self):
        if not isinstance(self.device, str) or not re.fullmatch(r'/dev/dri/card[0-9]+', self.device):
            raise ValueError('必须选择明确的 VKMS DRM 节点')
        for value in (self.connector, self.device_id):
            if type(value) is not int or not 0 < value <= 0xffffffff:
                raise ValueError('设备和连接器 ID 必须有效')
        if type(self.quality) is not int or self.quality not in (75, 85, 90):
            raise ValueError('仅支持 Q75、Q85、Q90')
        if type(self.fps) is not int or self.fps not in (30, 60):
            raise ValueError('目标帧率只能是 30 或 60')
        if self.transport not in ('socket-direct', 'socket') or type(self.elevate) is not bool:
            raise ValueError('无效的传输方式或权限设置')
        if type(self.frames) is not int or not 1 <= self.frames <= 300:
            raise ValueError('每次最多 300 帧')
        if type(self.seconds) is not int or not 1 <= self.seconds <= 15:
            raise ValueError('每次会话最多 15 秒，不含准备和清理')
        return self

    def arguments(self):
        self.validate()
        args = ['--device', self.device, '--connector', str(self.connector),
                '--device-id', str(self.device_id), '--quality', str(self.quality),
                '--fps', str(self.fps), '--transport', self.transport,
                '--frames', str(self.frames), '--seconds', str(self.seconds)]
        if self.elevate:
            args.append('--elevate')
        return args


class EventMailbox:
    """Coalesce telemetry; bound control/log events without storing video data."""
    def __init__(self):
        self.lock = threading.Lock()
        self.events = deque(maxlen=64)
        self.latest_metrics = None

    def put(self, event):
        with self.lock:
            if event.get('event') == 'metrics':
                self.latest_metrics = event
            else:
                self.events.append(event)

    def drain(self):
        with self.lock:
            events = list(self.events)
            self.events.clear()
            if self.latest_metrics is not None:
                events.append(self.latest_metrics)
                self.latest_metrics = None
            return events


class PanelController:
    """One owned backend process. Stop sends SIGINT only to our unreaped child.

    No process-group kills or force-killing privileged helpers. The backend closes
    its source and reaps children before it exits; the UI waits for that exit.
    """
    def __init__(self, mailbox=None):
        self.mailbox = mailbox or EventMailbox()
        self.lock = threading.Lock()
        self.process = None
        self.thread = None
        self.busy = False
        self.stop_requested = False
        self.signal_sent = False

    def launch(self, command):
        with self.lock:
            if self.busy:
                raise RuntimeError('已有操作正在运行')
            self.busy = True
            self.stop_requested = False
            self.signal_sent = False
        self.thread = threading.Thread(target=self._run, args=(tuple(command),), daemon=True)
        self.thread.start()

    def discover(self):
        self.launch([str(ROOT / 'xdisplay-backend'), 'discover'])

    def create_output(self):
        self.launch([str(ROOT / 'xdisplay-backend'), 'create-output', '--confirm'])

    def configure_output(self, device, connector, resolution, scale):
        if (not isinstance(device, str) or not re.fullmatch(r'/dev/dri/card[0-9]+', device)
                or type(connector) is not int or not 0 < connector <= 0xffffffff
                or resolution not in OUTPUT_RESOLUTIONS or scale not in OUTPUT_SCALES):
            raise ValueError('无效的虚拟输出设置')
        self.launch([str(ROOT / 'xdisplay-backend'), 'configure-output', '--confirm',
                     '--device', device, '--connector', str(connector),
                     '--resolution', resolution, '--scale', scale])

    def start_auto(self, resolution, scale, *, elevate=False):
        if resolution not in OUTPUT_RESOLUTIONS or scale not in OUTPUT_SCALES or type(elevate) is not bool:
            raise ValueError('无效的自动连接设置')
        args = [str(ROOT / 'xdisplay-backend'), 'auto', '--confirm',
                '--resolution', resolution, '--scale', scale]
        if elevate:
            args.append('--elevate')
        self.launch(args)

    def start(self, settings):
        self.launch([str(ROOT / 'xdisplay-backend'), 'stream', *settings.arguments()])

    def start_diagnostic(self, settings, directory):
        self.launch([str(ROOT / 'rust-backend' / 'target' / 'roi-diagnostic' / 'release' / 'xdisplay-backend'),
                     'stream', *settings.arguments(), '--diagnostic-dir', str(directory)])

    def _run(self, command):
        process = None
        code = -1
        try:
            # Only the controller creates the child. No shell expansion.
            process = subprocess.Popen(command, stdout=subprocess.PIPE,
                                       stderr=subprocess.STDOUT, stdin=subprocess.DEVNULL)
            with self.lock:
                self.process = process
                if self.stop_requested:
                    self._interrupt_locked(process)
            while True:
                line = process.stdout.readline(65537)
                if not line:
                    break
                if len(line) > 65536:
                    raise ValueError('后端状态行超出限制')
                try:
                    event = json.loads(line)
                except (ValueError, UnicodeError):
                    # Do not expose arbitrary backend/device text in the GUI.
                    continue
                if isinstance(event, dict) and isinstance(event.get('event'), str):
                    self.mailbox.put(event)
            code = process.wait()
        except Exception as exc:
            self.mailbox.put({'event': 'error', 'message': '无法运行后端：' + type(exc).__name__})
        finally:
            if process is not None:
                if process.poll() is None:
                    with self.lock:
                        self._interrupt_locked(process)
                    process.wait()  # cleanup can wait for bounded privileged helper
                if process.stdout:
                    process.stdout.close()
                code = process.returncode
            with self.lock:
                self.process = None
                self.busy = False
                self.mailbox.put({'event': 'process_exit', 'code': code})

    def drain_events(self):
        with self.lock:
            return self.mailbox.drain(), self.busy

    def _interrupt_locked(self, process):
        if not self.signal_sent and process.poll() is None:
            try:
                process.send_signal(signal.SIGINT)
                self.signal_sent = True
            except ProcessLookupError:
                pass

    def stop(self):
        with self.lock:
            self.stop_requested = True
            if self.process is not None:
                self._interrupt_locked(self.process)
