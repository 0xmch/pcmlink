"""The agent's protocol, its access rule, and what it reads from PulseAudio."""
import json
import os
import socket
import sys
import threading
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import pcmlink_agent as agent  # noqa: E402


class Fake:
    name = "fake"

    def __init__(self):
        self.started = self.stopped = 0
        self.volume = {}
        self.items = [{"id": "1", "name": "player", "active": True, "volume": 1.0,
                       "mute": False, "peak": [0.5, 0.25]}]

    def start(self): self.started += 1
    def stop(self): self.stopped += 1
    def refresh(self): pass
    def poll(self): pass
    def apps(self): return self.items

    def set_volume(self, app_id, value):
        if app_id != "1":
            raise OSError(f"no application {app_id}")
        self.volume[app_id] = value

    def set_mute(self, app_id, value):
        self.items[0]["mute"] = value


class Serving(unittest.TestCase):
    def setUp(self):
        probe = socket.create_server(("127.0.0.1", 0))
        self.port = probe.getsockname()[1]
        probe.close()
        self.backend = Fake()
        self.stop = threading.Event()
        self.thread = threading.Thread(
            target=agent.serve, args=(self.backend, self.port, set(), self.stop, "127.0.0.1"))
        self.thread.start()
        self.addCleanup(self.thread.join)
        self.addCleanup(self.stop.set)

    def connect(self):
        for _ in range(50):
            try:
                sock = socket.create_connection(("127.0.0.1", self.port), timeout=2)
                self.addCleanup(sock.close)
                return sock, sock.makefile("r")
            except OSError:
                threading.Event().wait(0.02)
        self.fail("agent did not start")

    def test_says_hello_then_streams_apps(self):
        _, lines = self.connect()
        hello = json.loads(lines.readline())
        self.assertEqual(hello["hello"]["backend"], "fake")
        self.assertIn("host", hello["hello"])
        self.assertEqual(json.loads(lines.readline())["apps"][0]["name"], "player")

    def test_commands_reach_the_backend_and_errors_come_back(self):
        sock, lines = self.connect()
        sock.sendall(b'{"cmd":"volume","id":"1","value":0.5}\n{"cmd":"mute","id":"1","value":true}\n')
        sock.sendall(b'{"cmd":"volume","id":"9","value":0.5}\nnot json\n')
        errors = []
        for _ in range(200):
            message = json.loads(lines.readline())
            if "error" in message:
                errors.append(message["error"])
            if len(errors) == 2:
                break
        self.assertEqual(self.backend.volume, {"1": 0.5})
        self.assertTrue(self.backend.items[0]["mute"])
        self.assertIn("no application 9", errors[0])

    def test_backend_runs_only_while_someone_is_connected(self):
        self.assertEqual(self.backend.started, 0)
        sock, lines = self.connect()
        lines.readline()
        self.assertEqual(self.backend.started, 1)
        lines.close()
        sock.close()
        for _ in range(100):
            if self.backend.stopped:
                break
            threading.Event().wait(0.02)
        self.assertEqual(self.backend.stopped, 1)


class Access(unittest.TestCase):
    def test_only_this_machine_and_named_addresses(self):
        allowed = {"192.0.2.7"}
        self.assertTrue(agent.accepts("192.0.2.7", allowed))
        self.assertTrue(agent.accepts("127.0.0.1", allowed))
        self.assertTrue(agent.accepts("::1", allowed))
        self.assertFalse(agent.accepts("192.0.2.8", allowed))


class PulseParsing(unittest.TestCase):
    ENTRY = {
        "index": 42, "corked": False, "mute": True,
        "volume": {"front-left": {"value": 32768}, "front-right": {"value": 32768}},
        "properties": {"application.name": "Firefox", "application.process.id": "77",
                       "application.process.binary": "firefox", "media.name": "A song"},
    }

    def test_entry(self):
        item = agent.Pulse.parse(self.ENTRY)
        self.assertEqual(item["id"], "42")
        self.assertEqual(item["name"], "Firefox")
        self.assertEqual(item["pid"], 77)
        self.assertEqual(item["media"], "A song")
        self.assertTrue(item["active"])
        self.assertTrue(item["mute"])
        self.assertAlmostEqual(item["volume"], 0.125)  # half on Pulse's cubic scale

    def test_hidden_by_name_or_exe(self):
        self.assertIsNone(agent.Pulse.parse(self.ENTRY, {"firefox"}))

    def test_volume_round_trip(self):
        self.assertEqual(agent.Pulse.to_level(1.0), agent.Pulse.NORM)
        self.assertAlmostEqual(agent.Pulse.to_amplitude(agent.Pulse.to_level(0.25) / agent.Pulse.NORM), 0.25, 3)

    def test_a_sparse_entry_still_parses(self):
        item = agent.Pulse.parse({"index": 1})
        self.assertEqual(item["name"], "stream 1")
        self.assertEqual(item["volume"], 1.0)


class WindowsMediaMatch(unittest.TestCase):
    def test_app_ids_match_the_exe(self):
        media = [{"app": "Chrome", "title": "A video", "artist": "Someone"}]
        self.assertEqual(agent.Wasapi._media_for("chrome", media), "A video - Someone")
        self.assertIsNone(agent.Wasapi._media_for("spotify", media))
        self.assertIsNone(agent.Wasapi._media_for(None, media))


if __name__ == "__main__":
    unittest.main()
