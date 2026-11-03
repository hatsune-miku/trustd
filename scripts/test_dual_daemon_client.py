"""The real-process smoke client must use the same MCP handshake as an SDK."""
import importlib.util
import io
import json
from pathlib import Path
import unittest
from unittest.mock import Mock, patch


spec = importlib.util.spec_from_file_location(
    "dual_daemon_client", Path(__file__).with_name("test-dual-daemon.py")
)
daemon = importlib.util.module_from_spec(spec)
spec.loader.exec_module(daemon)


class ClientTests(unittest.TestCase):
    def client(self, responses):
        client = daemon.Agent.__new__(daemon.Agent)
        client.counter = 0
        client.process = Mock(stdin=io.StringIO(), stdout=io.StringIO(
            "".join(json.dumps(response) + "\n" for response in responses)
        ))
        return client

    def test_initializes_before_calling_tools_and_checks_response_ids(self):
        client = self.client([
            {"jsonrpc": "2.0", "id": 1, "result": {"protocolVersion": "2025-06-18"}},
            {"jsonrpc": "2.0", "id": 2, "result": {"isError": False}},
        ])
        with patch.object(daemon.select, "select", return_value=([client.process.stdout], [], [])):
            client.initialize()
            self.assertFalse(client.call("destroy")["result"]["isError"])
        messages = [json.loads(line) for line in client.process.stdin.getvalue().splitlines()]
        self.assertEqual([message["method"] for message in messages], [
            "initialize", "notifications/initialized", "tools/call",
        ])
        self.assertEqual(messages[0]["params"]["protocolVersion"], "2025-06-18")
        self.assertNotIn("id", messages[1])
        self.assertEqual(messages[2]["id"], 2)

    def test_rejects_failed_initialization_and_mismatched_response(self):
        for response in [
            {"jsonrpc": "2.0", "id": 1, "error": {"message": "rejected"}},
            {"jsonrpc": "2.0", "id": 9, "result": {"protocolVersion": "2025-06-18"}},
        ]:
            with self.subTest(response=response):
                client = self.client([response])
                with patch.object(daemon.select, "select", return_value=([client.process.stdout], [], [])):
                    with self.assertRaises(AssertionError):
                        client.initialize()
                self.assertEqual(len(client.process.stdin.getvalue().splitlines()), 1)


if __name__ == "__main__":
    unittest.main()
