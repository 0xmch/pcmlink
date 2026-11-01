"""pcmlink-mixer itself, in a terminal, against made-up receivers. No real receiver is
ever connected to: --dir points at sockets served from here."""
import codecs
import json
import os
import re
import select
import signal
import socket
import struct
import tempfile
import threading
import time
import unittest

try:
    import fcntl
    import pty
    import termios
except ImportError as missing:
    raise unittest.SkipTest("needs a Unix terminal") from missing

HERE = os.path.dirname(os.path.abspath(__file__))
BINARY = os.environ.get("PCMLINK_MIXER_BIN") or os.path.join(HERE, "..", "mixer", "target", "release", "pcmlink-mixer")
LEFT, RIGHT, DOWN = b"\x1b[D", b"\x1b[C", b"\x1b[B"
TOKEN = re.compile(r"\x1b\[([?<]?)([0-9;]*)([@-~])")


class Screen:
    """Enough of a terminal for what ratatui writes."""

    def __init__(self, rows, cols):
        self.rows, self.cols = rows, cols
        self.decoder = codecs.getincrementaldecoder("utf-8")(errors="replace")
        self.pending = ""
        self.cells = [[" "] * cols for _ in range(rows)]
        self.y = self.x = 0

    def feed(self, data):
        text = self.pending + self.decoder.decode(data)
        self.pending = ""
        at = 0
        while at < len(text):
            if text[at] != "\x1b":
                if text[at] >= " " and self.y < self.rows and self.x < self.cols:
                    self.cells[self.y][self.x] = text[at]
                    self.x += 1
                at += 1
                continue
            token = TOKEN.match(text, at)
            if not token:
                self.pending = text[at:]
                return
            private, params, final = token.groups()
            if not private and final == "H":
                parts = params.split(";") if params else ["1", "1"]
                self.y, self.x = int(parts[0]) - 1, int(parts[1]) - 1
            elif not private and final == "J":
                self.cells = [[" "] * self.cols for _ in range(self.rows)]
            at = token.end()

    def text(self):
        return "\n".join("".join(row) for row in self.cells)


class FakeReceiver:
    """Speaks pcmlink-rx's control socket, and keeps what it is told."""

    def __init__(self, directory, port):
        self.port = port
        self.gain = 1.0
        self.mute = False
        self.commands = []
        self.stop = threading.Event()
        self.server = socket.socket(socket.AF_UNIX)
        self.server.bind(os.path.join(directory, f"{port}.sock"))
        self.server.listen(1)
        self.server.settimeout(0.05)

    def status(self):
        return {
            "port": self.port, "pid": 1, "up_s": 100, "gain": self.gain, "mute": self.mute,
            "peak": [0, 0], "rms": [0, 0], "margin_ms": 10, "buf_ms": 20, "low_ms": 15,
            "age_ms": 1, "packets": 1000, "lost": 0, "under": 0, "drift_ppm": 0,
            "peer": "127.0.0.1", "device": "Test output", "rate": 48000, "on": True, "error": None,
        }

    def run(self):
        client = None
        while not self.stop.is_set():
            if client is None:
                try:
                    client, _ = self.server.accept()
                    client.settimeout(0.04)
                except TimeoutError:
                    continue
            try:
                client.sendall((json.dumps(self.status()) + "\n").encode())
                for line in client.recv(4096).decode().splitlines():
                    self.commands.append(line)
                    word, _, value = line.partition(" ")
                    if word == "volume":
                        self.gain = float(value)
                    elif word == "mute":
                        self.mute = value == "1"
            except TimeoutError:
                pass
            except OSError:
                client.close()
                client = None
        if client:
            client.close()
        self.server.close()


class Mixer:
    def __init__(self, directory, closed_port, rows=40, cols=120):
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.environ["TERM"] = "xterm-256color"
            os.execv(BINARY, [BINARY, "--dir", directory, "--agent-port", str(closed_port)])
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        self.screen = Screen(rows, cols)
        self.raw = b""
        self.left = False

    def read(self, seconds):
        end = time.perf_counter() + seconds
        while (left := end - time.perf_counter()) > 0:
            if select.select([self.fd], [], [], left)[0]:
                try:
                    data = os.read(self.fd, 1 << 16)
                except OSError:
                    return
                if not data:
                    return
                self.raw += data
                self.screen.feed(data)

    def until(self, what, seconds=4.0):
        end = time.perf_counter() + seconds
        while time.perf_counter() < end:
            if what(self):
                return True
            self.read(0.01)
        return what(self)

    def press(self, key):
        os.write(self.fd, key)

    def latency(self, key):
        """Milliseconds from pressing a key to the screen starting to change."""
        self.press(key)
        start = time.perf_counter()
        if not select.select([self.fd], [], [], 1.0)[0]:
            return 1000.0
        data = os.read(self.fd, 1 << 16)
        took = (time.perf_counter() - start) * 1000
        self.screen.feed(data)
        return took

    def quit(self, key=b"q"):
        """True if it left when asked."""
        if self.left:
            return True
        self.press(key)
        end = time.time() + 3
        while time.time() < end:
            self.read(0.02)
            if os.waitpid(self.pid, os.WNOHANG)[0]:
                self.left = True
                os.close(self.fd)
                return True
        os.kill(self.pid, signal.SIGKILL)
        os.waitpid(self.pid, 0)
        self.left = True
        os.close(self.fd)
        return False


class Case(unittest.TestCase):
    def setUp(self):
        if not os.path.exists(BINARY):
            self.skipTest("cargo build --release in mixer/ first")
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        with socket.socket() as probe:
            probe.bind(("127.0.0.1", 0))
            closed_port = probe.getsockname()[1]
        self.receivers = [FakeReceiver(scratch.name, port) for port in (5004, 5005, 5006)]
        self.mixer = Mixer(scratch.name, closed_port)
        self.addCleanup(self.mixer.quit)
        for receiver in self.receivers:
            thread = threading.Thread(target=receiver.run, daemon=True)
            thread.start()
            self.addCleanup(thread.join)
            self.addCleanup(receiver.stop.set)
        self.assertTrue(self.mixer.until(lambda m: "stream :5004" in m.screen.text() and ":5006" in m.screen.text()), self.mixer.screen.text())
        self.mixer.read(0.2)


class Keys(Case):
    def test_a_key_reaches_the_screen_without_waiting_for_a_frame(self):
        taken = []
        for i in range(40):
            taken.append(self.mixer.latency(RIGHT if i % 4 < 2 else LEFT))
            self.mixer.read(0.012 + (i % 7) * 0.007)
        taken.sort()
        self.assertLess(taken[len(taken) // 2], 8, taken)

    def test_moving_between_strips_tells_the_receivers_nothing(self):
        for key in (RIGHT, RIGHT, LEFT, LEFT):
            self.mixer.press(key)
            self.mixer.read(0.05)
        self.mixer.read(0.1)
        self.assertEqual([r.commands for r in self.receivers], [[], [], []])

    def test_keys_that_arrive_together_all_count(self):
        self.mixer.press(RIGHT * 2)
        self.assertTrue(self.mixer.until(lambda m: "stream :5006" in m.screen.text(), 1.0), self.mixer.screen.text())
        self.mixer.press(DOWN * 3)
        first, _, last = self.receivers
        self.assertTrue(self.mixer.until(lambda m: len(last.commands) == 3, 1.0), last.commands)
        self.assertEqual(first.commands, [])
        self.assertTrue(all(c.startswith("volume ") for c in last.commands), last.commands)
        self.assertLess(last.gain, 1.0)

    def test_a_solo_is_undone_on_the_way_out(self):
        first, second, third = self.receivers
        self.mixer.press(b"s")
        self.assertTrue(self.mixer.until(lambda m: second.mute and third.mute, 1.0))
        self.assertFalse(first.mute)
        self.assertTrue(self.mixer.quit())
        deadline = time.time() + 1
        while time.time() < deadline and (second.mute or third.mute):
            time.sleep(0.01)
        self.assertFalse(second.mute or third.mute)

    def test_q_leaves(self):
        self.assertTrue(self.mixer.quit(b"q"))

    def test_ctrl_c_leaves(self):
        self.assertTrue(self.mixer.quit(b"\x03"))


class Mouse(Case):
    def test_only_what_is_used_is_asked_for(self):
        self.assertIn(b"\x1b[?1000h", self.mixer.raw)
        self.assertIn(b"\x1b[?1006h", self.mixer.raw)
        self.assertNotIn(b"\x1b[?1003h", self.mixer.raw, "every movement of the mouse")
        self.assertTrue(self.mixer.quit())
        self.assertIn(b"\x1b[?1000l", self.mixer.raw)

    def test_the_wheel_nudges_the_strip_under_it(self):
        line = next(i for i, text in enumerate(self.mixer.screen.text().split("\n")) if ":5004" in text)
        column = self.mixer.screen.text().split("\n")[line].index(":5004") + 2
        self.mixer.press(f"\x1b[<65;{column + 1};{line + 1}M".encode())
        first = self.receivers[0]
        self.assertTrue(self.mixer.until(lambda m: len(first.commands) == 1, 1.0), first.commands)
        self.assertLess(first.gain, 1.0)


if __name__ == "__main__":
    unittest.main()
