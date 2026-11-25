#!/usr/bin/env python3
"""Native GTK4/libadwaita control panel. No device action until a button is used."""
from __future__ import annotations
import argparse
from collections import deque
import json
from pathlib import Path
import signal
import sys
import gi

gi.require_version('Gtk', '4.0')
gi.require_version('Adw', '1')
from gi.repository import Adw, Gio, GLib, Gtk
from panel_model import OUTPUT_RESOLUTIONS, OUTPUT_SCALES, PanelController, SessionSettings
from panel_tray import PanelTray

TRANSPORTS = ('socket', 'socket-direct')


PHASES = {
    'waiting_usb': ('等待 USB iPad', '只接受一个 USB 设备；打开官方 App 后准备独立 VKMS 输出。'),
    'waiting_app': ('等待 iPad App', '打开官方 Wired XDisplay App；握手成功后将开始传屏。'),
    'configuring': ('正在配置 VKMS', '按所选分辨率和缩放设置独立输出；系统可能请求授权。'),
    'validating': ('复核设备', '重新验证 USB 设备和 VKMS 输出，不会回退到物理屏幕。'),
    'preparing': ('准备虚拟画面', '仅读取选定 VKMS 输出。权限确认和准备可能需要一些时间。'),
    'waiting': ('等待 iPad App', '请打开官方 Wired XDisplay App。发现 USB 设备不代表握手已完成。'),
    'sending': ('主机正在发送', '下方是主机发送统计，不代表 iPad 显示帧率或端到端延迟。'),
    'cleaning': ('正在清理', '正在释放采集和编码资源；退出时也会关闭授权助手。'),
}


class PanelWindow(Adw.ApplicationWindow):
    def __init__(self, application, *, preview=False):
        super().__init__(application=application, title='Wired Display')
        self.set_default_size(700, 860)
        self.set_size_request(470, 560)
        self.preview = preview
        self.diagnostic_dir = application.diagnostic_dir
        self.diagnostic_used = False
        self.controller = PanelController()
        self.devices, self.outputs = [], []
        self.closing = False
        self.confirming = False
        self.auto_active = False
        self.tray = None
        self.tray_reason = '正在检查系统托盘…'
        self.had_error = False
        self.clean = False
        self.owned_output = None
        self.output_cleanup_running = False
        self.cleanup_after_child = False
        self.lines = deque(maxlen=60)

        toolbar = Adw.ToolbarView()
        header = Adw.HeaderBar()
        header.set_title_widget(Adw.WindowTitle(title='Wired Display', subtitle='GTK 前端 · Rust 后端'))
        toolbar.add_top_bar(header)
        scroller = Gtk.ScrolledWindow(hscrollbar_policy=Gtk.PolicyType.NEVER)
        clamp = Adw.Clamp(maximum_size=620, tightening_threshold=480)
        content = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=24,
                          margin_top=24, margin_bottom=28, margin_start=20, margin_end=20)
        clamp.set_child(content)
        scroller.set_child(clamp)
        toolbar.set_content(scroller)
        self.set_content(toolbar)

        hero = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=10)
        icon = Gtk.Image.new_from_icon_name('video-display-symbolic')
        icon.set_pixel_size(44)
        hero.append(icon)
        self.status = Gtk.Label(label='将 iPad 作为独立显示器')
        self.status.add_css_class('title-1')
        hero.append(self.status)
        self.description = Gtk.Label(label='先检查设备；缺少虚拟输出时可创建／启用，再开始有界传屏测试。',
                                     wrap=True, justify=Gtk.Justification.CENTER)
        self.description.add_css_class('dim-label')
        hero.append(self.description)
        actions = Gtk.Box(spacing=12, halign=Gtk.Align.CENTER, margin_top=8)
        self.refresh = Gtk.Button(label='检查设备')
        self.refresh.connect('clicked', self.discover)
        self.start_button = Gtk.Button(label='开始测试')
        self.start_button.add_css_class('suggested-action')
        self.start_button.set_sensitive(False)
        self.start_button.connect('clicked', self.confirm_start)
        self.stop_button = Gtk.Button(label='停止')
        self.stop_button.set_sensitive(False)
        self.stop_button.connect('clicked', self.stop)
        for button in (self.refresh, self.start_button, self.stop_button):
            actions.append(button)
        hero.append(actions)
        content.append(hero)

        if not preview and self.diagnostic_dir is None:
            automation = Adw.PreferencesGroup(title='自动连接',
                description='经确认后自动识别 USB iPad、配置 VKMS 并持续传屏。关闭窗口会收起到托盘。')
            self.auto_row = Adw.ActionRow(title='USB iPad · Socket · Q85 · 60 fps')
            self.auto_button = Gtk.Button(label='启用自动连接', valign=Gtk.Align.CENTER)
            self.auto_button.connect('clicked', self.toggle_auto)
            self.auto_row.add_suffix(self.auto_button)
            automation.add(self.auto_row)
            content.append(automation)
        else:
            self.auto_button = None

        devices = Adw.PreferencesGroup(title='手动连接', description='仅支持独立 VKMS 输出，不会采集内置屏幕。')
        self.device_row = self.combo('USB 设备', ['尚未检查'])
        self.output_row = self.combo('虚拟输出', ['尚未检查'])
        devices.add(self.device_row)
        devices.add(self.output_row)
        content.append(devices)

        virtual = Adw.PreferencesGroup(title='虚拟输出设置',
            description='手动应用到选中的 VKMS 输出；自动连接使用这里的分辨率和缩放。设置不写入开机配置。')
        virtual_row = Adw.ActionRow(title='VKMS 独立显示器',
            subtitle='新输出：2360 × 1640 · 60 Hz · 缩放 1.5；已有输出保持原设置。')
        self.create_output_button = Gtk.Button(label='创建／启用')
        self.create_output_button.set_valign(Gtk.Align.CENTER)
        self.create_output_button.set_tooltip_text('按需加载 VKMS，并在 niri 中启用独立虚拟输出')
        self.create_output_button.connect('clicked', self.confirm_create_output)
        virtual_row.add_suffix(self.create_output_button)
        virtual.add(virtual_row)
        self.resolution = self.combo('分辨率', [s.replace('x', ' × ') for s in OUTPUT_RESOLUTIONS], 3)
        self.output_scale = self.combo('缩放倍率', [s + '×' for s in OUTPUT_SCALES], 2)
        apply_row = Adw.ActionRow(title='调整已启用的虚拟屏',
            subtitle='作用于上方选中的 VKMS 输出；刷新率为 60 Hz，设置不写入配置文件。')
        self.apply_output_button = Gtk.Button(label='应用设置', valign=Gtk.Align.CENTER)
        self.apply_output_button.connect('clicked', self.confirm_configure_output)
        apply_row.add_suffix(self.apply_output_button)
        for row in (self.resolution, self.output_scale, apply_row):
            virtual.add(row)
        content.append(virtual)

        settings = Adw.PreferencesGroup(title='手动测试画面', description='JPEG · 原始分辨率 · 4:2:0 · 不启用触控或预览')
        self.quality = self.combo('画质', ['Q75 · 标准', 'Q85 · 清晰', 'Q90 · 更清晰'], 1)
        self.rate = self.combo('目标帧率', ['30 fps', '60 fps'], 1)
        self.transport = self.combo('原始帧传输', ['Socket', 'Socket Direct'], int(self.diagnostic_dir is not None))
        self.transport.set_subtitle('手动测试默认使用 Socket；一次性诊断要求 Socket Direct。')
        for row in (self.quality, self.rate, self.transport):
            settings.add(row)
        content.append(settings)

        stats = Adw.PreferencesGroup(title='会话状态', description='不传输视频帧给界面；运行时每秒最多更新一次统计。')
        grid = Gtk.Grid(column_spacing=24, row_spacing=8, column_homogeneous=True,
                        margin_top=14, margin_bottom=14, margin_start=16, margin_end=16)
        self.fps_label = self.metric(grid, 0, '主机发送', '— fps')
        self.frame_label = self.metric(grid, 1, '已发送', '0 帧')
        self.encode_label = self.metric(grid, 2, '平均编码', '— ms')
        stats.add(grid)
        content.append(stats)

        advanced = Adw.PreferencesGroup(title='安全与限制')
        self.elevate = Adw.SwitchRow(title='临时授权 VKMS 读取器',
                                    subtitle='默认开启，仍需系统授权；GTK 界面和 Rust 后端均不提权。')
        self.elevate.set_active(True)
        advanced.add(self.elevate)
        limit = Adw.ActionRow(title='开发版有界测试', subtitle='每次最多 300 帧或 15 秒会话；准备及清理时间另计。')
        advanced.add(limit)
        self.details = Adw.ExpanderRow(title='操作记录', subtitle='仅保留本次操作状态，不保存画面或设备序列号。')
        self.log_label = Gtk.Label(label='尚未执行任何设备操作。', xalign=0, wrap=True,
                                   selectable=True, margin_top=8, margin_bottom=12,
                                   margin_start=16, margin_end=16)
        self.log_label.add_css_class('caption')
        self.details.add_row(self.log_label)
        advanced.add(self.details)
        content.append(advanced)
        foot = Gtk.Label(label='仅在明确确认后启动传屏或修改显示设置；不会随登录自动运行。', wrap=True)
        foot.add_css_class('caption')
        foot.add_css_class('dim-label')
        content.append(foot)

        quit_action = Gio.SimpleAction.new('quit', None)
        quit_action.connect('activate', lambda *_: self.request_quit())
        self.add_action(quit_action)
        self.connect('close-request', self.on_close)
        self.timer = GLib.timeout_add(500, self.poll)
        self.device_row.connect('notify::selected', lambda *_: self.update_controls())
        self.output_row.connect('notify::selected', lambda *_: self.update_controls())
        if preview:
            self.status.set_label('界面预览')
            self.description.set_label('无设备访问、无传屏。预览数据仅用于检查原生控件布局。')
            self.apply_inventory({'devices': [{'device_id': 1}], 'outputs': [
                {'device': '/dev/dri/card2', 'connector': 54, 'width': 2360, 'height': 1640}],
                'warnings': []}, preview=True)
            self.refresh.set_sensitive(False)
            self.start_button.set_sensitive(False)
        if self.auto_button is not None:
            try:
                bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
            except GLib.Error as error:
                self.tray_reason = '无法连接会话 D-Bus；自动连接不可用。'
                self.log(error.message)
            else:
                self.tray = PanelTray(bus, show=self.show_panel, pause=self.pause_auto,
                                      quit=self.request_quit, availability=self.tray_changed)
        self.update_controls()

    @staticmethod
    def combo(title, values, selected=0):
        row = Adw.ComboRow(title=title, model=Gtk.StringList.new(values))
        row.set_selected(selected)
        return row

    @staticmethod
    def metric(grid, column, title, value):
        label = Gtk.Label(label=value, halign=Gtk.Align.START)
        label.add_css_class('title-2')
        caption = Gtk.Label(label=title, halign=Gtk.Align.START)
        caption.add_css_class('dim-label')
        caption.add_css_class('caption')
        grid.attach(label, column, 0, 1, 1)
        grid.attach(caption, column, 1, 1, 1)
        return label

    def log(self, message):
        self.lines.append(message[:350])
        self.log_label.set_label('\n'.join(self.lines))

    def update_controls(self):
        busy = self.controller.busy or self.auto_active or self.confirming or self.closing
        for row in (self.device_row, self.output_row, self.quality, self.rate, self.transport, self.elevate):
            row.set_sensitive(not busy)
        self.refresh.set_sensitive(not busy and not self.preview)
        self.create_output_button.set_sensitive(not busy and not self.preview and self.diagnostic_dir is None)
        has_output = self.output_row.get_selected() < len(self.outputs)
        can_configure = not busy and not self.preview and self.diagnostic_dir is None
        for widget in (self.resolution, self.output_scale):
            widget.set_sensitive(can_configure)
        self.apply_output_button.set_sensitive(can_configure and has_output)
        ready = self.device_row.get_selected() < len(self.devices) and has_output
        self.start_button.set_sensitive(not busy and ready and not self.preview and not self.diagnostic_used)
        self.stop_button.set_sensitive(self.controller.busy and not self.auto_active and not self.controller.stop_requested)
        if self.auto_button is not None:
            stopping = self.auto_active and self.controller.stop_requested
            can_pause = self.auto_active and not stopping and not self.closing
            can_start = not busy and self.tray is not None and self.tray.ready
            self.auto_button.set_sensitive(can_pause or can_start)
            self.auto_button.set_label('正在暂停…' if stopping else
                                       '暂停自动连接' if self.auto_active else '启用自动连接')
            auto_status = self.status.get_label() if self.auto_active else '自动连接未启用'
            self.auto_row.set_subtitle(self.tray_reason or auto_status)
            if self.tray is not None:
                self.tray.update(auto_status, can_pause)

    def tray_changed(self, ready, reason):
        self.tray_reason = reason
        if not ready and not self.closing:
            if not self.get_visible():
                self.present()
            self.pause_auto()
        self.update_controls()

    def show_panel(self):
        if not self.closing:
            self.present()

    def pause_auto(self):
        if self.auto_active and not self.controller.stop_requested:
            self.stop()

    def toggle_auto(self, *_):
        if self.auto_active:
            self.pause_auto()
        else:
            self.confirm_auto()

    def confirm_auto(self):
        if (self.auto_button is None or self.tray is None or not self.tray.ready
                or self.controller.busy or self.confirming or self.closing or self.auto_active):
            return
        resolution = OUTPUT_RESOLUTIONS[self.resolution.get_selected()]
        scale = OUTPUT_SCALES[self.output_scale.get_selected()]
        elevate = self.elevate.get_active()
        self.confirming = True
        self.update_controls()
        dialog = Gtk.AlertDialog(message='启用自动连接？',
            detail=f'检测到单一 USB 设备且可连接 iPad App 时，将临时配置独立 VKMS 输出为 {resolution.replace("x", "×")}、'
                   f'60 Hz、缩放 {scale}×，并在 iPad App 就绪后持续传屏。'
                   '\n\n仅使用 Socket · Q85 · 60 fps；不采集物理屏幕、不注入触控，不保存画面。'
                   '\n关闭窗口只会收起到托盘。请用托盘或控制面板暂停或退出。'
                   '\n系统可能请求 VKMS 设置授权。' + ('\n读取器也可能请求临时授权。' if elevate else ''),
            buttons=['取消', '启用'], cancel_button=0, default_button=0)

        def answered(obj, result):
            self.confirming = False
            try:
                if obj.choose_finish(result) == 1 and not self.closing and self.tray.ready:
                    self.controller.start_auto(resolution, scale, elevate=elevate)
                    self.auto_active = True
                    self.had_error = False
                    self.clean = False
                    self.status.set_label('正在启用自动连接')
                    self.description.set_label('启动本次自动连接会话。')
                    self.reset_metrics()
            except GLib.Error:
                pass
            except (OSError, ValueError, RuntimeError) as error:
                self.log(str(error))
            self.update_controls()

        dialog.choose(self, None, answered)

    def reset_metrics(self):
        self.fps_label.set_label('— fps')
        self.frame_label.set_label('0 帧')
        self.encode_label.set_label('— ms')

    def discover(self, *_):
        if self.preview or self.controller.busy or self.auto_active or self.confirming or self.closing:
            return
        self.had_error = False
        self.status.set_label('正在检查设备')
        self.description.set_label('仅检查元数据，不采集画面、不启动服务或修改显示配置。')
        try:
            self.controller.discover()
        except Exception as exc:
            self.log(str(exc))
        self.update_controls()

    def apply_inventory(self, event, *, preview=False):
        index = self.output_row.get_selected()
        previous = self.outputs[index] if index < len(self.outputs) else None
        self.devices = event.get('devices', [])
        self.outputs = event.get('outputs', [])
        self.device_row.set_model(Gtk.StringList.new([
            f'USB 设备 · ID {d["device_id"]}' for d in self.devices] or ['未发现 USB 设备']))
        self.output_row.set_model(Gtk.StringList.new([
            f'{o["width"]} × {o["height"]} · {o["device"].split("/")[-1]} / {o["connector"]}'
            for o in self.outputs] or ['未发现可用 VKMS 输出']))
        if previous is not None:
            for index, output in enumerate(self.outputs):
                if (output['device'], output['connector']) == (previous['device'], previous['connector']):
                    self.output_row.set_selected(index)
                    break
        if not preview:
            self.status.set_label('设备可供选择' if self.devices and self.outputs else '尚未就绪')
            self.description.set_label('USB 已发现；开始后仍需 iPad App 完成握手。' if self.devices and self.outputs
                                       else '连接 iPad；缺少虚拟输出可点击“创建／启用”，不自动修改系统。')
        for warning in event.get('warnings', []):
            self.log(warning)
        self.update_controls()

    def confirm_create_output(self, *_):
        if self.preview or self.controller.busy or self.auto_active or self.confirming or self.closing or self.diagnostic_dir is not None:
            return
        self.confirming = True
        self.update_controls()
        dialog = Gtk.AlertDialog(message='创建或启用独立 VKMS 输出？',
            detail='仅支持 niri 会话。已有可用 VKMS 输出将直接复用，不改分辨率或缩放。\n\n'
                   '如需加载模块，系统将请求授权固定命令 modprobe vkms enable_cursor=1；'
                   'Rust 后端和 GTK 界面不以 root 运行。新建或原先关闭的虚拟输出设置为 '
                   '2360×1640 / 60Hz / 缩放 1.5 / 正常方向。\n\n'
                   '不针对物理屏幕执行设置、不保存开机配置、不开始采集或传屏。'
                   '投屏结束或明确退出时关闭本程序启用的虚拟屏；不会卸载模块。',
            buttons=['取消', '创建／启用'], cancel_button=0, default_button=0)
        def answered(obj, result):
            self.confirming = False
            try:
                if obj.choose_finish(result) == 1 and not self.closing:
                    self.had_error = False
                    self.controller.create_output()
            except GLib.Error:
                pass
            except Exception as exc:
                self.log(str(exc))
            self.update_controls()
        dialog.choose(self, None, answered)

    def confirm_configure_output(self, *_):
        if self.preview or self.controller.busy or self.auto_active or self.confirming or self.closing or self.diagnostic_dir is not None:
            return
        try:
            output = self.outputs[self.output_row.get_selected()]
            resolution = OUTPUT_RESOLUTIONS[self.resolution.get_selected()]
            scale = OUTPUT_SCALES[self.output_scale.get_selected()]
        except IndexError:
            self.log('请先检查设备并选择虚拟输出。')
            return
        device, connector = output['device'], output['connector']
        self.confirming = True
        self.update_controls()
        dialog = Gtk.AlertDialog(message='应用虚拟输出设置？',
            detail=f'仅调整 {device} / 连接器 {connector}：'
                   f'{resolution.replace("x", "×")} / 60 Hz，缩放 {scale}×。\n\n'
                   '只对当前 niri 会话生效，不调整物理屏幕，也不会开始传屏。'
                   '如果仅部分设置成功，不会自动撤销，请重新检查设备。',
            buttons=['取消', '应用'], cancel_button=0, default_button=0)

        def answered(obj, result):
            self.confirming = False
            try:
                if obj.choose_finish(result) == 1 and not self.closing:
                    self.had_error = False
                    self.controller.configure_output(device, connector, resolution, scale)
            except GLib.Error:
                pass
            except Exception as exc:
                self.log(str(exc))
            self.update_controls()

        dialog.choose(self, None, answered)

    def confirm_start(self, *_):
        if self.preview or self.controller.busy or self.auto_active or self.confirming or self.closing or self.diagnostic_used:
            return
        try:
            output = self.outputs[self.output_row.get_selected()]
            device = self.devices[self.device_row.get_selected()]
            settings = SessionSettings(device=output['device'], connector=output['connector'],
                device_id=device['device_id'], quality=(75, 85, 90)[self.quality.get_selected()],
                fps=(30, 60)[self.rate.get_selected()],
                transport=TRANSPORTS[self.transport.get_selected()],
                elevate=self.elevate.get_active()).validate()
        except (ValueError, IndexError):
            self.log('请选择有效的设备与输出。')
            return
        saving = '不保存图片。'
        if self.diagnostic_dir is not None:
            if ((output.get('width'), output.get('height')) != (2360, 1640)
                    or (settings.quality, settings.fps, settings.transport) != (85, 60, 'socket-direct')):
                self.log('诊断仅接受 2360×1640、Q85、60 fps、Socket Direct。')
                return
            saving = ('仅对第 90、180、270 帧在结束后保存两处 512×256 的编码前后裁剪，'
                      '不保存完整画面。请使用不含隐私的滚动内容。')
        self.confirming = True
        self.update_controls()
        dialog = Gtk.AlertDialog(message='开始一次虚拟输出传屏测试？',
            detail=f'仅采集 {settings.device} / 连接器 {settings.connector}，'
                   f'原分辨率、Q{settings.quality}、目标 {settings.fps} fps。\n\n'
                   f'最多 300 帧或 15 秒会话；不采集物理屏幕、不注入触控。{saving}'
                   '准备和清理可能更久。' + ('\n将请求系统临时授权 C 读取器。' if settings.elevate else ''),
            buttons=['取消', '开始测试'], cancel_button=0, default_button=0)
        def answered(obj, result):
            self.confirming = False
            try:
                if obj.choose_finish(result) == 1 and not self.closing:
                    self.had_error = False
                    self.clean = False
                    self.reset_metrics()
                    if self.diagnostic_dir is None:
                        self.controller.start(settings)
                    else:
                        self.diagnostic_used = True
                        self.controller.start_diagnostic(settings, self.diagnostic_dir)
                    self.cleanup_after_child = True
            except GLib.Error:
                pass
            except Exception as exc:
                self.log(str(exc))
            self.update_controls()
        dialog.choose(self, None, answered)

    def stop(self, *_):
        self.controller.stop()
        self.status.set_label('正在停止')
        self.description.set_label('已请求后端停止，等待其释放资源；不强杀权限子进程。')
        self.update_controls()

    def poll(self):
        # A hidden window does not redraw telemetry. Exit/cleanup is still handled.
        events, busy = self.controller.drain_events()
        for event in events:
            kind = event['event']
            if self.diagnostic_dir is not None and kind in (
                    'diagnostic_config', 'diagnostic_sent', 'diagnostic_saved', 'diagnostic_cursor_stage', 'error',
                    'metrics', 'cleanup', 'finished', 'stopped', 'process_exit'):
                print(json.dumps(event, ensure_ascii=False), flush=True)
            if kind == 'output_owned':
                self.owned_output = event
            elif kind == 'output_cleaned':
                if self.output_cleanup_running and self.owned_output is not None:
                    owner = self.owned_output
                    self.outputs = [row for row in self.outputs if
                                    (row['device'], row['connector']) != (owner['device'], owner['connector'])]
                    self.output_row.set_model(Gtk.StringList.new([
                        f'{row["width"]} × {row["height"]} · {row["device"].split("/")[-1]} / {row["connector"]}'
                        for row in self.outputs] or ['未发现可用 VKMS 输出']))
                    self.owned_output = None
                self.log('本程序启用的 VKMS 输出已清理')
            elif kind == 'inventory':
                self.apply_inventory(event)
            elif kind == 'output_setup':
                self.status.set_label('正在设置虚拟输出')
                self.description.set_label(event.get('message', '等待系统授权或虚拟输出就绪…'))
                self.log(event.get('message', '正在创建虚拟输出'))
            elif kind == 'output_ready':
                self.status.set_label('虚拟输出已就绪')
                action = ('已应用设置' if event.get('configured') else
                          '已创建／启用' if event.get('changed') else '已复用现有')
                scale = f' · 缩放 {event["scale"]:g}×' if 'scale' in event else ''
                next_step = '等待 iPad App。' if self.auto_active else '未开始传屏。'
                self.description.set_label(
                    f'{action} {event["device"]} / {event["connector"]}，'
                    f'{event["width"]}×{event["height"]}{scale}；{next_step}')
                self.log(self.description.get_label())
            elif kind == 'phase':
                title, detail = PHASES.get(event.get('phase'), ('处理中', ''))
                self.status.set_label(title)
                self.description.set_label(detail)
                self.log(title)
                if self.auto_active and event.get('phase') == 'waiting_usb':
                    self.reset_metrics()
            elif kind == 'auto_warning':
                message = event['message']
                self.description.set_label(message)
                self.log(message)
            elif kind == 'metrics' and self.get_visible():
                self.frame_label.set_label(f'{event.get("frames", 0)} 帧')
                rate, duration = event.get('send_fps'), event.get('encode_ms')
                self.fps_label.set_label(f'{rate:.1f} fps' if rate is not None else '— fps')
                self.encode_label.set_label(f'{duration:.1f} ms' if duration is not None else '— ms')
            elif kind == 'error':
                self.had_error = True
                self.status.set_label('操作未完成')
                self.description.set_label(event.get('message', '请检查操作记录。'))
                self.log(event.get('message', '后端错误'))
            elif kind == 'cleanup':
                self.clean = event.get('complete', False)
                self.log('后端源清理完成' if self.clean else '资源清理状态未知')
            elif kind in ('finished', 'stopped'):
                if not self.auto_active and not self.had_error:
                    self.status.set_label('测试已结束' if kind == 'finished' else '已停止')
                    self.description.set_label('后端已结束；画面是否正常需在 iPad 上确认。')
            elif kind == 'process_exit':
                code = event['code']
                if self.output_cleanup_running:
                    self.output_cleanup_running = False
                elif self.cleanup_after_child:
                    self.cleanup_after_child = False
                    self.begin_output_cleanup()
                self.log(f'后端进程已退出（{code}）')
                if self.auto_active:
                    self.auto_active = False
                    if self.controller.stop_requested and code == 0 and not self.had_error:
                        self.status.set_label('自动连接已暂停')
                        self.description.set_label('后端已退出；可重新启用自动连接。')
                    elif not self.closing:
                        self.present()
                        if code == 0 and not self.had_error:
                            self.status.set_label('自动连接已结束')
                            self.description.set_label('后端已退出；可检查操作记录或重新启用。')
                if code != 0 and not self.had_error:
                    self.had_error = True
                    self.status.set_label('后端异常退出')
                    self.description.set_label('退出并不代表所有权限子进程已清理，请检查终端与系统授权状态。')
        self.update_controls()
        if self.closing and not busy and not self.controller.busy:
            if not self.quit_cleanup_attempted and self.begin_output_cleanup():
                return GLib.SOURCE_CONTINUE
            self.finish_quit()
            return GLib.SOURCE_REMOVE
        return GLib.SOURCE_CONTINUE

    def begin_output_cleanup(self):
        if self.owned_output is None or self.output_cleanup_running:
            return False
        if self.closing:
            self.quit_cleanup_attempted = True
        try:
            self.controller.cleanup_output(self.owned_output)
        except (OSError, RuntimeError) as exc:
            self.log(f'无法启动 VKMS 清理：{exc}')
            return False
        self.output_cleanup_running = True
        self.log('正在关闭本程序启用的 VKMS 输出…')
        return True

    def request_quit(self):
        if self.closing:
            return
        self.closing = True
        self.quit_cleanup_attempted = self.output_cleanup_running
        if self.controller.busy:
            if not self.output_cleanup_running:
                self.stop()
            return
        # Drain pending output ownership/exit messages in poll before leaving.
        if self.poll() == GLib.SOURCE_REMOVE:
            GLib.source_remove(self.timer)

    def finish_quit(self):
        if self.tray is not None:
            self.tray.close()
        self.get_application().quit()
        self.destroy()

    def on_close(self, *_):
        # Closing the window is not quitting while the tray can reopen it.
        if not self.closing and self.tray is not None and self.tray.ready:
            self.set_visible(False)
        else:
            self.request_quit()
        return True


class PanelApplication(Adw.Application):
    def __init__(self, *, preview=False, diagnostic_dir=None):
        flags = (Gio.ApplicationFlags.NON_UNIQUE if preview or diagnostic_dir is not None
                 else Gio.ApplicationFlags.FLAGS_NONE)
        super().__init__(application_id='io.github.wired_display.ControlPanel', flags=flags)
        self.preview = preview
        self.diagnostic_dir = diagnostic_dir

    def do_activate(self):
        windows = self.get_windows()
        window = windows[0] if windows else PanelWindow(self, preview=self.preview)
        window.show_panel()

    def do_startup(self):
        Adw.Application.do_startup(self)
        self.set_accels_for_action('win.quit', ['<Primary>q'])
        self.signal_sources = [GLib.unix_signal_add(GLib.PRIORITY_DEFAULT, sig,
                                                   self.request_quit)
                               for sig in (signal.SIGTERM, signal.SIGINT)]

    def request_quit(self):
        for window in self.get_windows():
            window.request_quit()
        return GLib.SOURCE_CONTINUE

    def do_shutdown(self):
        for source_id in self.signal_sources:
            GLib.source_remove(source_id)
        Adw.Application.do_shutdown(self)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--preview', action='store_true', help='仅显示布局，不访问设备')
    parser.add_argument('--diagnostic-dir', type=Path, help='一次性采样；仅保存局部裁剪至新的绝对路径')
    args = parser.parse_args()
    if args.diagnostic_dir is not None and (args.preview or not args.diagnostic_dir.is_absolute()):
        parser.error('诊断要求新的绝对路径，不可和预览同时使用')
    return PanelApplication(preview=args.preview, diagnostic_dir=args.diagnostic_dir).run([sys.argv[0]])


if __name__ == '__main__':
    raise SystemExit(main())
