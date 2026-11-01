"""pcmlink agent: which applications make up a machine's audio.

A stream is one mix, so the receiver can only say which machine it comes from.
The agent runs on that machine and tells a mixer what is playing there, with a
level, and lets the mixer change each application's volume and mute, using the
operating system's own per-application controls. Nothing about the stream
changes.

A mixer finds the agent by itself: it takes the address a stream's packets come
from and connects to this port there. A machine without an agent simply shows
no applications.

The protocol is one JSON object per line over TCP, so a machine this program has
no backend for can get an agent in any language:

    agent -> mixer   {"hello": {"host": "name", "os": "Windows", "backend": "wasapi"}}
                     {"apps": [{"id": "1234", "name": "TEKKEN 8", "exe": "Polaris",
                                "pid": 1234, "active": true, "volume": 1.0,
                                "mute": false, "peak": [0.1, 0.1], "title": "...",
                                "media": "..."}]}      about 30 times a second
                     {"error": "..."} or {"notice": "..."}
    mixer -> agent   {"cmd": "volume", "id": "1234", "value": 0.5}   linear, 0 to 1
                     {"cmd": "mute", "id": "1234", "value": true}
                     {"cmd": "restart-sender"}

"peak", "title" and "media" may be null where a platform cannot say.

The agent does nothing while no mixer is connected. It only accepts connections
from the addresses it is told to allow, plus this machine.

On Windows it must run in the logged-on desktop session. Windows lists a
session's applications only to that session, and a process in session 0 sees
neither them nor their audio; `pcmlink service install agent` arranges this.
"""
from __future__ import annotations

import contextlib
import json
import os
import platform
import selectors
import shutil
import socket
import subprocess
import sys
import threading
import time

PORT = 5003
TICK = 1 / 30           # how often levels go out
REFRESH = 1.0           # how often the list of applications is rebuilt
SENDER = "pcmlink-send" # the sender's service name on every platform

# Programs that capture the mix show up among those playing it, at the level of
# the whole mix, though they make no sound. An agent can't tell them apart from
# a program playing alone, so they are named: this one is our own sender, and
# --hide adds others on a given machine.
HIDDEN_BY_DEFAULT = ("gst-launch-1.0",)


# ---------------------------------------------------------------- backends
#
# A backend needs: name; start() and stop() around having any client; refresh(),
# about once a second, to rebuild its list; poll(), every tick, to read levels;
# apps(), the list as the protocol sends it; set_volume(id, value); set_mute(id, value).

class NoBackend:
    """A machine with no per-application control: it still reports its name."""

    name = "none"

    def start(self): pass
    def stop(self): pass
    def refresh(self): pass
    def poll(self): pass
    def apps(self): return []
    def set_volume(self, app_id, value): raise OSError("no per-application volume here")
    def set_mute(self, app_id, value): raise OSError("no per-application mute here")


class Pulse:
    """PulseAudio or PipeWire, through pactl and parec. Every stream is its own
    entry here, so two tabs of a browser are two strips."""

    name = "pulse"
    NORM = 65536  # PA_VOLUME_NORM

    def __init__(self, hide):
        self.hide = hide
        self.items = {}
        self.monitors = {}

    @staticmethod
    def available():
        return bool(shutil.which("pactl") and shutil.which("parec"))

    def start(self):
        self.items = {}

    def stop(self):
        for mon in self.monitors.values():
            mon["proc"].kill()
            mon["proc"].wait()
        self.monitors = {}
        self.items = {}

    def _run(self, *args):
        return subprocess.run(["pactl", *args], capture_output=True, text=True, timeout=2)

    def refresh(self):
        try:
            r = self._run("-f", "json", "list", "sink-inputs")
            listed = json.loads(r.stdout or "[]")
        except (OSError, ValueError, subprocess.TimeoutExpired):
            return
        items = {}
        for entry in listed:
            item = self.parse(entry, self.hide)
            if item:
                items[item["id"]] = item
        self.items = items
        for app_id in [i for i in self.monitors if i not in items]:
            self.monitors[app_id]["proc"].kill()
            self.monitors[app_id]["proc"].wait()
            del self.monitors[app_id]
        for app_id in items:
            if app_id not in self.monitors:
                self._monitor(app_id)

    @classmethod
    def parse(cls, entry, hide=()):
        """A protocol entry from one element of `pactl -f json list sink-inputs`."""
        props = entry.get("properties", {})
        exe = props.get("application.process.binary") or ""
        name = props.get("application.name") or exe or f"stream {entry.get('index')}"
        if exe.lower() in hide or name.lower() in hide:
            return None
        volumes = [v.get("value", cls.NORM) for v in entry.get("volume", {}).values()]
        level = max(volumes) / cls.NORM if volumes else 1.0
        pid = props.get("application.process.id")
        return {
            "id": str(entry.get("index")),
            "name": name,
            "exe": exe or None,
            "pid": int(pid) if str(pid).isdigit() else None,
            "active": not entry.get("corked", False),
            "volume": cls.to_amplitude(level),
            "mute": bool(entry.get("mute", False)),
            "peak": None,
            "title": None,
            "media": props.get("media.name"),
        }

    # PulseAudio's volume scale is cubic in amplitude.
    @staticmethod
    def to_amplitude(level):
        return level ** 3

    @classmethod
    def to_level(cls, amplitude):
        return round(max(0.0, amplitude) ** (1 / 3) * cls.NORM)

    def _monitor(self, app_id):
        try:
            proc = subprocess.Popen(
                ["parec", f"--monitor-stream={app_id}", "--raw", "--format=s16le",
                 "--rate=8000", "--channels=2", "--latency-msec=30"],
                stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
        except OSError:
            return
        os.set_blocking(proc.stdout.fileno(), False)
        self.monitors[app_id] = {"proc": proc, "rest": b""}

    def poll(self):
        from array import array
        for app_id, mon in self.monitors.items():
            try:
                data = mon["proc"].stdout.read()
            except OSError:
                data = None
            item = self.items.get(app_id)
            if not data or item is None:
                continue
            data = mon["rest"] + data
            usable = len(data) - len(data) % 4
            mon["rest"] = data[usable:]
            samples = array("h", data[:usable])
            if sys.byteorder == "big":
                samples.byteswap()
            if samples:
                item["peak"] = [max(map(abs, samples[0::2])) / 32768,
                                max(map(abs, samples[1::2])) / 32768]

    def apps(self):
        return list(self.items.values())

    def _set(self, what, app_id, value):
        if app_id not in self.items:
            raise OSError(f"no stream {app_id}")
        r = self._run(what, app_id, value)
        if r.returncode != 0:
            raise OSError(r.stderr.strip() or f"pactl {what} failed")

    def set_volume(self, app_id, value):
        self._set("set-sink-input-volume", app_id, str(self.to_level(value)))
        self.items[app_id]["volume"] = max(0.0, value)

    def set_mute(self, app_id, value):
        self._set("set-sink-input-mute", app_id, "1" if value else "0")
        self.items[app_id]["mute"] = bool(value)


class Wasapi:
    """Windows audio sessions, through Core Audio's COM interfaces. ctypes only,
    so the agent needs nothing beyond Python itself."""

    name = "wasapi"

    def __init__(self, hide):
        import ctypes as C
        from ctypes import wintypes as W
        self.C, self.W = C, W
        self.hide = hide
        self.groups = {}
        self.described = {}
        self.media = _WindowsMedia()
        self.ole32 = C.WinDLL("ole32")
        self.kernel32 = C.WinDLL("kernel32", use_last_error=True)
        self.user32 = C.WinDLL("user32")
        self.version = C.WinDLL("version")
        self.protos = {}
        self.ole32.CoInitializeEx(None, 0)  # multithreaded; all calls are on this thread
        self.ole32.CoTaskMemFree.argtypes = [C.c_void_p]
        self.ole32.CoTaskMemFree.restype = None
        self.enumerator = self._create("BCDE0395-E52F-467C-8E3D-C4579291692E",
                                       "A95664D2-9614-4F35-A746-DE8DB63617E6")

    # A COM interface is a pointer to a table of function pointers, whose first
    # argument is the interface itself. `call` picks one by its position in the
    # interface's header file.

    def _guid(self, text):
        import uuid
        return (self.C.c_ubyte * 16).from_buffer_copy(uuid.UUID(text).bytes_le)

    def _create(self, clsid, iid):
        C = self.C
        out = C.c_void_p()
        hr = self.ole32.CoCreateInstance(C.byref(self._guid(clsid)), None, 23,
                                         C.byref(self._guid(iid)), C.byref(out))
        if hr < 0:
            raise OSError(f"CoCreateInstance failed: {hr & 0xffffffff:#x}")
        return out

    def call(self, ptr, slot, argtypes, *args):
        C = self.C
        proto = self.protos.get(argtypes)
        if proto is None:
            proto = self.protos[argtypes] = C.WINFUNCTYPE(C.c_long, C.c_void_p, *argtypes)
        table = C.cast(ptr, C.POINTER(C.POINTER(C.c_void_p)))[0]
        return proto(table[slot])(ptr, *args)

    def out_ptr(self, ptr, slot, *before):
        """Call a method whose last argument receives an interface pointer."""
        C = self.C
        out = C.c_void_p()
        types = tuple(type(a) for a in before) + (C.c_void_p,)
        hr = self.call(ptr, slot, types, *before, C.byref(out))
        if hr < 0:
            raise OSError(f"COM call failed: {hr & 0xffffffff:#x}")
        return out

    def release(self, ptr):
        if ptr:
            self.call(ptr, 2, ())

    def qi(self, ptr, iid):
        C = self.C
        out = C.c_void_p()
        hr = self.call(ptr, 0, (C.c_void_p, C.c_void_p), C.byref(self._guid(iid)), C.byref(out))
        if hr < 0:
            raise OSError(f"QueryInterface failed: {hr & 0xffffffff:#x}")
        return out

    def _string(self, ptr, slot):
        C = self.C
        out = C.c_void_p()
        if self.call(ptr, slot, (C.c_void_p,), C.byref(out)) < 0 or not out:
            return ""
        text = C.wstring_at(out.value)
        self.ole32.CoTaskMemFree(out)
        return text

    # ------------------------------------------------ processes

    def _processes(self):
        """pid -> (parent pid, exe file name without .exe), from a Toolhelp snapshot."""
        C, W = self.C, self.W

        class Entry(C.Structure):
            _fields_ = [("size", W.DWORD), ("usage", W.DWORD), ("pid", W.DWORD),
                        ("heap", C.c_size_t), ("module", W.DWORD), ("threads", W.DWORD),
                        ("parent", W.DWORD), ("priority", W.LONG), ("flags", W.DWORD),
                        ("exe", W.WCHAR * 260)]
        k = self.kernel32
        k.CreateToolhelp32Snapshot.restype = W.HANDLE
        k.CreateToolhelp32Snapshot.argtypes = (W.DWORD, W.DWORD)
        k.Process32FirstW.argtypes = (W.HANDLE, C.POINTER(Entry))
        k.Process32NextW.argtypes = (W.HANDLE, C.POINTER(Entry))
        k.CloseHandle.argtypes = (W.HANDLE,)
        snap = k.CreateToolhelp32Snapshot(2, 0)  # TH32CS_SNAPPROCESS
        out = {}
        if snap in (None, -1, W.HANDLE(-1).value):
            return out
        entry = Entry()
        entry.size = C.sizeof(Entry)
        ok = k.Process32FirstW(snap, C.byref(entry))
        while ok:
            exe = entry.exe
            out[entry.pid] = (entry.parent, exe[:-4] if exe.lower().endswith(".exe") else exe)
            ok = k.Process32NextW(snap, C.byref(entry))
        k.CloseHandle(snap)
        return out

    def _windows(self):
        """pid -> the title of its first visible top-level window."""
        C, W = self.C, self.W
        u = self.user32
        titles = {}

        @C.WINFUNCTYPE(W.BOOL, W.HWND, W.LPARAM)
        def visit(hwnd, _):
            if u.IsWindowVisible(hwnd) and not u.GetWindow(hwnd, 4):  # GW_OWNER
                pid = W.DWORD()
                u.GetWindowThreadProcessId(hwnd, C.byref(pid))
                if pid.value not in titles:
                    buf = C.create_unicode_buffer(256)
                    if u.GetWindowTextW(hwnd, buf, 256):
                        titles[pid.value] = buf.value.strip()
            return True

        u.GetWindow.argtypes = (W.HWND, W.UINT)
        u.GetWindow.restype = W.HWND
        u.IsWindowVisible.argtypes = (W.HWND,)
        u.GetWindowThreadProcessId.argtypes = (W.HWND, C.POINTER(W.DWORD))
        u.GetWindowTextW.argtypes = (W.HWND, W.LPWSTR, C.c_int)
        u.EnumWindows.argtypes = (C.c_void_p, W.LPARAM)
        u.EnumWindows(C.cast(visit, C.c_void_p), 0)
        return titles

    def _description(self, pid):
        """The exe's FileDescription: what Windows' own volume mixer calls it."""
        if pid in self.described:
            return self.described[pid]
        self.described[pid] = text = self._read_description(pid)
        return text

    def _read_description(self, pid):
        C, W = self.C, self.W
        k, v = self.kernel32, self.version
        k.OpenProcess.restype = W.HANDLE
        k.OpenProcess.argtypes = (W.DWORD, W.BOOL, W.DWORD)
        k.QueryFullProcessImageNameW.argtypes = (W.HANDLE, W.DWORD, W.LPWSTR, C.POINTER(W.DWORD))
        v.GetFileVersionInfoSizeW.argtypes = (W.LPCWSTR, C.POINTER(W.DWORD))
        v.GetFileVersionInfoW.argtypes = (W.LPCWSTR, W.DWORD, W.DWORD, C.c_void_p)
        v.VerQueryValueW.argtypes = (C.c_void_p, W.LPCWSTR, C.POINTER(C.c_void_p), C.POINTER(W.UINT))
        proc = k.OpenProcess(0x1000, False, pid)  # PROCESS_QUERY_LIMITED_INFORMATION
        if not proc:
            return ""
        path = C.create_unicode_buffer(1024)
        size = W.DWORD(1024)
        ok = k.QueryFullProcessImageNameW(proc, 0, path, C.byref(size))
        k.CloseHandle(proc)
        if not ok:
            return ""
        length = v.GetFileVersionInfoSizeW(path.value, None)
        if not length:
            return ""
        data = C.create_string_buffer(length)
        if not v.GetFileVersionInfoW(path.value, 0, length, data):
            return ""
        out = C.c_void_p()
        got = W.UINT()
        if not v.VerQueryValueW(data, "\\VarFileInfo\\Translation", C.byref(out), C.byref(got)) or got.value < 4:
            return ""
        lang = C.cast(out, C.POINTER(C.c_ushort))
        key = f"\\StringFileInfo\\{lang[0]:04x}{lang[1]:04x}\\FileDescription"
        if not v.VerQueryValueW(data, key, C.byref(out), C.byref(got)) or got.value < 1:
            return ""
        return C.wstring_at(out.value).strip()

    @staticmethod
    def _root(pid, procs):
        """The topmost ancestor running the same exe: a browser's audio plays from
        a helper process, and the application is its main process."""
        exe = procs[pid][1] if pid in procs else None
        for _ in range(16):
            parent = procs.get(pid, (0, ""))[0]
            if parent == pid or parent not in procs or procs[parent][1] != exe:
                break
            pid = parent
        return pid

    # ------------------------------------------------ the backend's interface

    def start(self):
        self.media.start()

    def stop(self):
        self.media.stop()
        self._drop()
        self.described = {}

    def _drop(self):
        for group in self.groups.values():
            for session in group["sessions"]:
                self.release(session["volume"])
                self.release(session["meter"])
        self.groups = {}

    def refresh(self):
        C = self.C
        procs = self._processes()
        windows = self._windows()
        media = self.media.current()
        previous = self.groups
        groups = {}
        endpoints = C.c_void_p()
        # eRender, DEVICE_STATE_ACTIVE
        if self.call(self.enumerator, 3, (C.c_int, C.c_ulong, C.c_void_p), 0, 1, C.byref(endpoints)) < 0:
            return
        count = C.c_uint()
        self.call(endpoints, 3, (C.c_void_p,), C.byref(count))
        for i in range(count.value):
            device = self.out_ptr(endpoints, 4, C.c_uint(i))
            with contextlib.suppress(OSError):
                self._read_device(device, procs, windows, media, groups)
            self.release(device)
        self.release(endpoints)
        # Levels stay on screen between refreshes.
        for app_id, group in groups.items():
            if app_id in previous:
                group["item"]["peak"] = previous[app_id]["item"]["peak"]
        self.groups, old = groups, previous
        for group in old.values():
            for session in group["sessions"]:
                self.release(session["volume"])
                self.release(session["meter"])

    def _read_device(self, device, procs, windows, media, groups):
        C = self.C
        # A new session manager each time: an existing one's enumerator does not
        # reliably show sessions created after it was activated.
        iid = self._guid("77AA99A0-1BD6-484F-8BC7-2C654C9A9B6F")
        manager = C.c_void_p()
        # CLSCTX_ALL
        hr = self.call(device, 3, (C.c_void_p, C.c_ulong, C.c_void_p, C.c_void_p),
                       C.byref(iid), 23, None, C.byref(manager))
        if hr < 0:
            return
        sessions = self.out_ptr(manager, 5)
        self.release(manager)
        count = C.c_int()
        self.call(sessions, 3, (C.c_void_p,), C.byref(count))
        for i in range(count.value):
            control = self.out_ptr(sessions, 4, C.c_int(i))
            with contextlib.suppress(OSError):
                self._read_session(control, procs, windows, media, groups)
            self.release(control)
        self.release(sessions)

    def _read_session(self, control, procs, windows, media, groups):
        C = self.C
        state = C.c_int()
        self.call(control, 3, (C.c_void_p,), C.byref(state))
        if state.value == 2:  # AudioSessionStateExpired
            return
        second = self.qi(control, "bfb7ff88-7239-4fc9-8fa2-07c950be9c6d")  # IAudioSessionControl2
        pid_out = C.c_ulong()
        self.call(second, 14, (C.c_void_p,), C.byref(pid_out))
        system = self.call(second, 15, ()) == 0  # S_OK from IsSystemSoundsSession
        display = self._string(second, 4)
        self.release(second)
        pid = pid_out.value

        if system:
            root, exe, name = 0, None, "System sounds"
        else:
            root = self._root(pid, procs)
            exe = procs.get(pid, (0, None))[1]
            name = ""
            if display and not display.startswith("@"):
                name = display
            name = name or self._description(pid) or windows.get(root) or windows.get(pid) or exe or f"pid {pid}"
        if (exe or "").lower() in self.hide or name.lower() in self.hide:
            return

        volume = self.qi(control, "87CE5498-68D6-44E5-9215-6DA47EF883D8")   # ISimpleAudioVolume
        meter = self.qi(control, "C02216F6-8C67-4B5B-9D00-D008E73E0064")    # IAudioMeterInformation
        level = C.c_float()
        muted = C.c_int()
        self.call(volume, 4, (C.c_void_p,), C.byref(level))
        self.call(volume, 6, (C.c_void_p,), C.byref(muted))

        app_id = "system" if system else str(root)
        group = groups.get(app_id)
        if group is None:
            group = groups[app_id] = {"sessions": [], "item": {
                "id": app_id, "name": name, "exe": exe, "pid": None if system else root,
                "active": False, "volume": level.value, "mute": True, "peak": [0.0, 0.0],
                "title": windows.get(root) or windows.get(pid),
                "media": self._media_for(exe, media),
            }}
        group["sessions"].append({"volume": volume, "meter": meter})
        item = group["item"]
        item["active"] = item["active"] or state.value == 1
        item["mute"] = item["mute"] and bool(muted.value)

    @staticmethod
    def _media_for(exe, media):
        """What Windows' media controls say this program is playing. They name
        programs by app id ("Chrome", "Spotify.exe"), not by process."""
        if not exe:
            return None
        stem = exe.lower()
        for entry in media:
            if stem in entry.get("app", "").lower():
                parts = [entry.get("title"), entry.get("artist")]
                return " - ".join(p for p in parts if p) or None
        return None

    def poll(self):
        C = self.C
        values = (C.c_float * 16)()
        for group in self.groups.values():
            left = right = 0.0
            for session in group["sessions"]:
                channels = C.c_uint()
                if self.call(session["meter"], 4, (C.c_void_p,), C.byref(channels)) < 0:
                    continue
                n = min(channels.value, 16)
                if n and self.call(session["meter"], 5, (C.c_uint, C.c_void_p), n, values) >= 0:
                    left = max(left, *values[0:n:2])
                    right = max(right, *(values[1:n:2] if n > 1 else values[0:1]))
            group["item"]["peak"] = [left, right]

    def apps(self):
        return [g["item"] for g in self.groups.values()]

    def _sessions(self, app_id):
        group = self.groups.get(app_id)
        if group is None:
            raise OSError(f"no application {app_id}")
        return group

    def set_volume(self, app_id, value):
        C = self.C
        group = self._sessions(app_id)
        value = min(max(float(value), 0.0), 1.0)
        for session in group["sessions"]:
            hr = self.call(session["volume"], 3, (C.c_float, C.c_void_p), value, None)
            if hr < 0:
                raise OSError(f"SetMasterVolume failed: {hr & 0xffffffff:#x}")
        group["item"]["volume"] = value

    def set_mute(self, app_id, value):
        C = self.C
        group = self._sessions(app_id)
        for session in group["sessions"]:
            hr = self.call(session["volume"], 5, (C.c_int, C.c_void_p), 1 if value else 0, None)
            if hr < 0:
                raise OSError(f"SetMute failed: {hr & 0xffffffff:#x}")
        group["item"]["mute"] = bool(value)


class _WindowsMedia:
    """Titles from Windows' media controls (what its volume flyout shows: a video
    or track, even in a background tab). They are WinRT, which PowerShell can
    reach and plain Python cannot, so a hidden PowerShell runs while a mixer is
    connected and ends with this process."""

    SCRIPT = r"""
[Console]::OutputEncoding = [Text.Encoding]::UTF8
Add-Type -AssemblyName System.Runtime.WindowsRuntime
$as = ([System.WindowsRuntimeSystemExtensions].GetMethods() | Where-Object {
  $_.Name -eq 'AsTask' -and $_.GetParameters().Count -eq 1 -and
  $_.GetParameters()[0].ParameterType.Name -eq 'IAsyncOperation`1' })[0]
function Wait-Op($op, $type) {
  $task = $as.MakeGenericMethod($type).Invoke($null, @($op))
  [void]$task.Wait(3000)
  $task.Result
}
$M = [Windows.Media.Control.GlobalSystemMediaTransportControlsSessionManager,Windows.Media.Control,ContentType=WindowsRuntime]
$P = [Windows.Media.Control.GlobalSystemMediaTransportControlsSessionMediaProperties,Windows.Media.Control,ContentType=WindowsRuntime]
$manager = Wait-Op ($M::RequestAsync()) $M
while (Get-Process -Id PARENT -ErrorAction SilentlyContinue) {
  $list = @()
  foreach ($s in $manager.GetSessions()) {
    try {
      $p = Wait-Op ($s.TryGetMediaPropertiesAsync()) $P
      $list += @{ app = $s.SourceAppUserModelId; title = $p.Title; artist = $p.Artist;
                  status = "$($s.GetPlaybackInfo().PlaybackStatus)" }
    } catch {}
  }
  ConvertTo-Json -Compress -InputObject @($list)
  Start-Sleep -Seconds 2
}
"""

    def __init__(self):
        self.proc = None
        self.latest = []

    def start(self):
        import base64
        if self.proc is not None:
            return
        script = self.SCRIPT.replace("PARENT", str(os.getpid()))
        encoded = base64.b64encode(script.encode("utf-16-le")).decode()
        try:
            self.proc = subprocess.Popen(
                ["powershell", "-NoProfile", "-NonInteractive", "-EncodedCommand", encoded],
                stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                creationflags=subprocess.CREATE_NO_WINDOW)
        except OSError:
            return
        threading.Thread(target=self._read, args=(self.proc,), daemon=True).start()

    def _read(self, proc):
        for line in proc.stdout:
            try:
                value = json.loads(line.decode("utf-8", "replace"))
            except ValueError:
                continue
            self.latest = value if isinstance(value, list) else []

    def stop(self):
        if self.proc is not None:
            self.proc.kill()
            self.proc.wait()
            self.proc = None
        self.latest = []

    def current(self):
        return self.latest


def choose_backend(hide):
    system = platform.system()
    if system == "Windows":
        return Wasapi(hide)
    if system == "Linux" and Pulse.available():
        return Pulse(hide)
    return NoBackend()


def restart_sender():
    """Restart this machine's pcmlink sender, however it is installed here."""
    system = platform.system()
    if system == "Windows":
        # The sender's job object takes its gst-launch down with it.
        flags = subprocess.CREATE_NO_WINDOW
        subprocess.run(["schtasks", "/end", "/tn", SENDER], capture_output=True, creationflags=flags)
        r = subprocess.run(["schtasks", "/run", "/tn", SENDER], capture_output=True, text=True,
                           creationflags=flags)
        return r.returncode, r.stderr or r.stdout
    if system == "Linux":
        r = subprocess.run(["systemctl", "--user", "restart", f"{SENDER}.service"],
                           capture_output=True, text=True)
        return r.returncode, r.stderr
    return 1, f"restarting a sender is not implemented on {system}"


# ---------------------------------------------------------------- the server

class Client:
    def __init__(self, sock):
        self.sock = sock
        self.pending = b""

    def send(self, data):
        """False if the client should go: it has gone, or it can't keep up, and a
        half-written line could not be finished anyway. It can connect again."""
        try:
            return self.sock.send(data) == len(data)
        except OSError:
            return False


def handle(line, backend, reply):
    """Carry out one line from a client. Errors go back to that client."""
    try:
        msg = json.loads(line)
        cmd = msg["cmd"]
        if cmd == "volume":
            backend.set_volume(str(msg["id"]), float(msg["value"]))
        elif cmd == "mute":
            backend.set_mute(str(msg["id"]), bool(msg["value"]))
        elif cmd == "restart-sender":
            code, text = restart_sender()
            if code != 0:
                raise OSError(text.strip() or f"exit {code}")
            reply({"notice": "sender restarted"})
        else:
            reply({"error": f"unknown command {cmd}"})
    except (ValueError, KeyError, TypeError, OSError, subprocess.SubprocessError) as exc:
        reply({"error": str(exc) or repr(exc)})


def encode(message):
    return (json.dumps(message, ensure_ascii=False) + "\n").encode()


def accepts(address, allowed):
    """Who may connect: this machine, and the addresses named."""
    return address in allowed or address == "::1" or address.startswith("127.")


def resolve_hosts(names):
    """The addresses of `names`; one that doesn't resolve is reported and skipped."""
    out = set()
    for name in names:
        try:
            out.update(info[4][0] for info in socket.getaddrinfo(name, None))
        except OSError as exc:
            print(f"pcmlink agent: cannot resolve {name}: {exc}", flush=True)
    return out


def serve(backend, port, allowed, stop=None, bind="0.0.0.0"):
    """Run until `stop` (a threading.Event, for tests) is set."""
    hello = encode({"hello": {"host": socket.gethostname().split(".")[0], "os": platform.system(),
                              "backend": backend.name}})
    listener = socket.create_server((bind, port), reuse_port=False)
    listener.setblocking(False)
    selector = selectors.DefaultSelector()
    selector.register(listener, selectors.EVENT_READ)
    clients = {}
    next_tick = next_refresh = 0.0
    print(f"pcmlink agent: listening on port {port} (backend {backend.name})", flush=True)

    def drop(client):
        selector.unregister(client.sock)
        client.sock.close()
        del clients[client.sock]
        if not clients:
            backend.stop()

    while stop is None or not stop.is_set():
        wait = max(0.0, next_tick - time.monotonic()) if clients else (0.1 if stop else None)
        for key, _ in selector.select(wait):
            if key.fileobj is listener:
                try:
                    sock, address = listener.accept()
                except OSError:
                    continue
                if not accepts(address[0], allowed):
                    sock.close()
                    continue
                sock.setblocking(False)
                sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
                if not clients:
                    backend.start()
                    next_refresh = 0.0
                client = clients[sock] = Client(sock)
                selector.register(sock, selectors.EVENT_READ)
                if not client.send(hello):
                    drop(client)
                continue
            client = clients[key.fileobj]
            try:
                data = client.sock.recv(4096)
            except OSError:
                data = b""
            if not data or len(client.pending) > 65536:
                drop(client)
                continue
            client.pending += data
            *lines, client.pending = client.pending.split(b"\n")
            for line in lines:
                handle(line, backend, lambda m, c=client: c.send(encode(m)))

        if not clients:
            continue
        now = time.monotonic()
        if now < next_tick:
            continue
        next_tick = now + TICK
        if now >= next_refresh:
            next_refresh = now + REFRESH
            backend.refresh()
        backend.poll()
        message = encode({"apps": backend.apps()})
        for client in list(clients.values()):
            if not client.send(message):
                drop(client)

    for client in list(clients.values()):
        drop(client)
    listener.close()
    selector.close()


def main(port=PORT, allow=(), hide=()):
    hidden = {h.lower() for h in HIDDEN_BY_DEFAULT} | {h.lower() for h in hide}
    backend = choose_backend(hidden)
    serve(backend, port, resolve_hosts(allow))
    return 0
