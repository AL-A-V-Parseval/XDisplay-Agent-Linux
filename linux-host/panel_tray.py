"""StatusNotifierItem + dbusmenu for the GTK panel; no backend actions on the bus."""
from gi.repository import Gio, GLib

WATCHER = 'org.kde.StatusNotifierWatcher'
ITEM = 'org.kde.StatusNotifierItem'
MENU = 'com.canonical.dbusmenu'
PROPERTIES = 'org.freedesktop.DBus.Properties'
WATCHER_PATH = '/StatusNotifierWatcher'
ITEM_PATH = '/StatusNotifierItem'
MENU_PATH = '/MenuBar'
ICON = 'video-display'

INTERFACES = Gio.DBusNodeInfo.new_for_xml('''
<node>
  <interface name="org.kde.StatusNotifierItem">
    <method name="Activate"><arg type="i" direction="in"/><arg type="i" direction="in"/></method>
    <method name="SecondaryActivate"><arg type="i" direction="in"/><arg type="i" direction="in"/></method>
    <method name="ContextMenu"><arg type="i" direction="in"/><arg type="i" direction="in"/></method>
    <method name="Scroll"><arg type="i" direction="in"/><arg type="s" direction="in"/></method>
    <property name="Category" type="s" access="read"/>
    <property name="Id" type="s" access="read"/>
    <property name="Title" type="s" access="read"/>
    <property name="Status" type="s" access="read"/>
    <property name="WindowId" type="u" access="read"/>
    <property name="IconName" type="s" access="read"/>
    <property name="ToolTip" type="(sa(iiay)ss)" access="read"/>
    <property name="ItemIsMenu" type="b" access="read"/>
    <property name="Menu" type="o" access="read"/>
    <signal name="NewToolTip"/>
  </interface>
  <interface name="com.canonical.dbusmenu">
    <method name="GetLayout"><arg type="i" direction="in"/><arg type="i" direction="in"/>
      <arg type="as" direction="in"/><arg type="u" direction="out"/><arg type="(ia{sv}av)" direction="out"/></method>
    <method name="GetGroupProperties"><arg type="ai" direction="in"/><arg type="as" direction="in"/>
      <arg type="a(ia{sv})" direction="out"/></method>
    <method name="GetProperty"><arg type="i" direction="in"/><arg type="s" direction="in"/>
      <arg type="v" direction="out"/></method>
    <method name="Event"><arg type="i" direction="in"/><arg type="s" direction="in"/>
      <arg type="v" direction="in"/><arg type="u" direction="in"/></method>
    <method name="EventGroup"><arg type="a(isvu)" direction="in"/><arg type="ai" direction="out"/></method>
    <method name="AboutToShow"><arg type="i" direction="in"/><arg type="b" direction="out"/></method>
    <method name="AboutToShowGroup"><arg type="ai" direction="in"/><arg type="ai" direction="out"/>
      <arg type="ai" direction="out"/></method>
    <property name="Version" type="u" access="read"/>
    <property name="TextDirection" type="s" access="read"/>
    <property name="Status" type="s" access="read"/>
    <property name="IconThemePath" type="as" access="read"/>
    <signal name="LayoutUpdated"><arg type="u"/><arg type="i"/></signal>
    <signal name="ItemsPropertiesUpdated"><arg type="a(ia{sv})"/><arg type="a(ias)"/></signal>
  </interface>
</node>''')


class PanelTray:
    def __init__(self, connection, *, show, pause, quit, availability):
        self.bus = connection
        self.show = show
        self.availability = availability
        self.actions = {2: show, 3: pause, 4: quit}
        self.ready = False
        self.status = '自动连接未启用'
        self.can_pause = False
        self.owner = None
        self.registered = False
        self.subscription_ids = []
        self.cancellable = None
        self.host_check = 0
        self.object_ids = [
            connection.register_object(ITEM_PATH, INTERFACES.lookup_interface(ITEM),
                                       self._item_method, self._item_property, None),
            connection.register_object(MENU_PATH, INTERFACES.lookup_interface(MENU),
                                       self._menu_method, self._menu_property, None),
        ]
        self.watch_id = Gio.bus_watch_name_on_connection(
            connection, WATCHER, Gio.BusNameWatcherFlags.NONE, self._appeared, self._vanished)

    def close(self):
        Gio.bus_unwatch_name(self.watch_id)
        self._unwatch_host()
        for object_id in self.object_ids:
            self.bus.unregister_object(object_id)

    def _unwatch_host(self):
        if self.cancellable is not None:
            self.cancellable.cancel()
        for signal_id in self.subscription_ids:
            self.bus.signal_unsubscribe(signal_id)
        self.subscription_ids.clear()
        self.owner = None
        self.registered = False

    def _appeared(self, connection, name, owner):
        self.owner = owner
        self.cancellable = Gio.Cancellable()
        for interface, member in ((PROPERTIES, 'PropertiesChanged'),
                                  (WATCHER, 'StatusNotifierHostRegistered'),
                                  (WATCHER, 'StatusNotifierHostUnregistered')):
            self.subscription_ids.append(connection.signal_subscribe(
                owner, interface, member, WATCHER_PATH, None, Gio.DBusSignalFlags.NONE,
                self._host_signal))
        connection.call(owner, WATCHER_PATH, WATCHER, 'RegisterStatusNotifierItem',
                        GLib.Variant('(s)', (ITEM_PATH,)), GLib.VariantType.new('()'),
                        Gio.DBusCallFlags.NONE, 3000, self.cancellable, self._registration_done,
                        self.cancellable)

    def _registration_done(self, connection, result, request):
        if request.is_cancelled():
            return
        try:
            connection.call_finish(result)
        except GLib.Error as error:
            self._set_ready(False, f'托盘注册失败：{error.message}')
            return
        self.registered = True
        self._query_host()

    def _host_signal(self, *_):
        if self.registered:
            self._query_host()

    def _query_host(self):
        self.host_check += 1
        self.bus.call(self.owner, WATCHER_PATH, PROPERTIES, 'Get',
                      GLib.Variant('(ss)', (WATCHER, 'IsStatusNotifierHostRegistered')),
                      GLib.VariantType.new('(v)'), Gio.DBusCallFlags.NONE, 3000, self.cancellable,
                      self._host_done, (self.cancellable, self.host_check))

    def _host_done(self, connection, result, data):
        request, revision = data
        if request.is_cancelled() or revision != self.host_check:
            return
        try:
            value = connection.call_finish(result).get_child_value(0).get_variant()
        except GLib.Error as error:
            self._set_ready(False, f'无法确认托盘是否可用：{error.message}')
            return
        ready = value.get_type_string() == 'b' and value.get_boolean()
        self._set_ready(ready, '' if ready else '没有可用的系统托盘；自动连接不可用。')

    def _vanished(self, *_):
        self._unwatch_host()
        self._set_ready(False, '未找到系统托盘服务；自动连接不可用。')

    def _set_ready(self, ready, reason):
        self.ready = ready
        self.availability(ready, reason)

    def update(self, status, can_pause):
        updated = []
        if status != self.status:
            self.status = status
            updated.append((1, self._rows()[1]))
            self._emit(ITEM_PATH, ITEM, 'NewToolTip', None)
        if can_pause != self.can_pause:
            self.can_pause = can_pause
            updated.append((3, self._rows()[3]))
        if updated:
            self._emit(MENU_PATH, MENU, 'ItemsPropertiesUpdated',
                       GLib.Variant('(a(ia{sv})a(ias))', (updated, [])))

    def _emit(self, path, interface, signal, parameters):
        if not self.bus.is_closed():
            self.bus.emit_signal(None, path, interface, signal, parameters)

    def _item_property(self, _connection, _sender, _path, _interface, name):
        return {
            'Category': GLib.Variant('s', 'ApplicationStatus'),
            'Id': GLib.Variant('s', 'wired-display'),
            'Title': GLib.Variant('s', 'Wired Display'),
            'Status': GLib.Variant('s', 'Active'),
            'WindowId': GLib.Variant('u', 0),
            'IconName': GLib.Variant('s', ICON),
            'ToolTip': GLib.Variant('(sa(iiay)ss)', (ICON, [], 'Wired Display', self.status)),
            'ItemIsMenu': GLib.Variant('b', False),
            'Menu': GLib.Variant('o', MENU_PATH),
        }[name]

    def _item_method(self, _connection, _sender, _path, _interface, method, _params, invocation):
        invocation.return_value(None)
        # The host uses /MenuBar for a menu. Reveal the controls if it invokes the item directly.
        if method != 'Scroll':
            self.show()

    def _rows(self):
        return {0: {'children-display': GLib.Variant('s', 'submenu')},
                **{row_id: {'label': GLib.Variant('s', label), 'enabled': GLib.Variant('b', enabled),
                            'visible': GLib.Variant('b', True)}
                   for row_id, label, enabled in ((1, self.status, False), (2, '打开控制面板', True),
                                                   (3, '暂停自动连接', self.can_pause), (4, '退出', True))}}

    @staticmethod
    def _select(properties, names):
        return {name: value for name, value in properties.items() if not names or name in names}

    def _menu_property(self, _connection, _sender, _path, _interface, name):
        return {'Version': GLib.Variant('u', 3), 'TextDirection': GLib.Variant('s', 'ltr'),
                'Status': GLib.Variant('s', 'normal'), 'IconThemePath': GLib.Variant('as', [])}[name]

    def _menu_method(self, _connection, _sender, _path, _interface, method, params, invocation):
        rows = self._rows()
        args = params.unpack()
        if method == 'GetLayout':
            parent, depth, names = args
            if parent not in rows:
                invocation.return_dbus_error(MENU + '.UnknownItem', 'Unknown menu item')
                return
            children = [GLib.Variant('(ia{sv}av)', (i, self._select(rows[i], names), []))
                        for i in rows if i != 0] if parent == 0 and depth != 0 else []
            invocation.return_value(GLib.Variant('(u(ia{sv}av))', (
                1, (parent, self._select(rows[parent], names), children))))
        elif method == 'GetGroupProperties':
            ids, names = args
            invocation.return_value(GLib.Variant('(a(ia{sv}))', (
                [(i, self._select(rows[i], names)) for i in (ids or rows) if i in rows],)))
        elif method == 'GetProperty':
            row, name = args
            if row not in rows or name not in rows[row]:
                invocation.return_dbus_error(MENU + '.UnknownProperty', 'Unknown menu item or property')
                return
            invocation.return_value(GLib.Variant('(v)', (rows[row][name],)))
        elif method == 'AboutToShow':
            invocation.return_value(GLib.Variant('(b)', (False,)))
        elif method == 'AboutToShowGroup':
            invocation.return_value(GLib.Variant('(aiai)', ([], [i for i in args[0] if i not in rows])))
        else:
            events = [args] if method == 'Event' else args[0]
            if method == 'EventGroup':
                invocation.return_value(GLib.Variant('(ai)', ([i for i, *_ in events if i not in rows],)))
            else:
                invocation.return_value(None)
            for row, event, _data, _time in events:
                if event == 'clicked' and row in self.actions and (row != 3 or self.can_pause):
                    self.actions[row]()
