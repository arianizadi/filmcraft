"""Regression tests for private pairing files and refusal of conflicting app sessions."""

import argparse
import importlib.util
import io
import json
import os
from pathlib import Path
import socket
import tempfile
import threading
import unittest
from contextlib import redirect_stdout
from unittest.mock import patch


SPEC = importlib.util.spec_from_file_location("agent_workflow", Path(__file__).with_name("start-agent-workflow.py"))
workflow = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(workflow)


class WorkflowTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.directory = workflow.support_directory(Path(self.temporary.name) / "Agent")

    def test_token_is_private_persistent_and_never_printed(self):
        output = io.StringIO()
        with redirect_stdout(output):
            first = workflow.token_for(self.directory, create=True)
            second = workflow.token_for(self.directory, create=True)
        self.assertTrue(first == second)
        self.assertEqual(len(first), 64)
        self.assertEqual(output.getvalue(), "")
        self.assertEqual((self.directory / "control-token").stat().st_mode & 0o777, 0o600)
        self.assertEqual(self.directory.stat().st_mode & 0o777, 0o700)

    def test_symlinks_shared_permissions_and_oversized_tokens_are_rejected(self):
        outside = Path(self.temporary.name) / "outside"
        outside.write_text("untouched")
        token = self.directory / "control-token"
        token.symlink_to(outside)
        with self.assertRaises(OSError):
            workflow.token_for(self.directory, create=True)
        self.assertEqual(outside.read_text(), "untouched")
        token.unlink()
        workflow.token_for(self.directory, create=True)
        os.chmod(token, 0o644)
        with self.assertRaises(workflow.WorkflowError):
            workflow.token_for(self.directory)
        os.chmod(token, 0o600)
        token.write_text("a" * 64 + "\n " + "z" * 100)
        with self.assertRaises(workflow.WorkflowError):
            workflow.token_for(self.directory)
        link = Path(self.temporary.name) / "alias"
        link.symlink_to(self.directory, target_is_directory=True)
        with self.assertRaises(workflow.WorkflowError):
            workflow.support_directory(link)

    def test_an_occupied_port_never_launches_or_attaches(self):
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            listener.listen()
            args = argparse.Namespace(port=listener.getsockname()[1])
            with patch.object(workflow.subprocess, "Popen") as launch:
                with self.assertRaises(workflow.WorkflowError):
                    workflow.start(args, self.directory, Path("/unused"), Path("/unused"))
                launch.assert_not_called()
        self.assertFalse((self.directory / "control-token").exists())

    def test_protocol_authentication_and_session_identity_are_checked(self):
        token = workflow.token_for(self.directory, create=True)
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            listener.listen()
            port = listener.getsockname()[1]

            def reply():
                for session in ("owned", "different"):
                    with listener.accept()[0] as connection:
                        with connection.makefile("rb") as stream:
                            request = json.loads(stream.readline(4096))
                        authenticated = request.get("token") == token
                        response = {"id": 1, "ok": authenticated, "result": {"sessionId": session, "permission": "propose"}}
                        connection.sendall((json.dumps(response) + "\n").encode())

            thread = threading.Thread(target=reply, daemon=True)
            thread.start()
            workflow.write_connection(self.directory, {"port": port, "sessionId": "owned"})
            self.assertEqual(workflow.paired_session(self.directory, port, token)["sessionId"], "owned")
            with self.assertRaises(workflow.WorkflowError):
                workflow.paired_session(self.directory, port, token)
            thread.join(timeout=2)
            self.assertFalse(thread.is_alive())

    def test_config_uses_absolute_paths_and_contains_no_token(self):
        output = io.StringIO()
        arguments = ["start-agent-workflow.py", "config", "--app", "/usr/bin/true", "--cli", "/usr/bin/true", "--support-dir", str(self.directory)]
        with patch.object(workflow.sys, "argv", arguments), redirect_stdout(output):
            self.assertEqual(workflow.main(), 0)
        config = json.loads(output.getvalue())["mcpServers"]["filmcraft"]
        self.assertTrue(Path(config["command"]).is_absolute())
        self.assertTrue(Path(config["args"][0]).is_absolute())
        self.assertNotIn("FILMCRAFT_CONTROL_TOKEN", output.getvalue())
        self.assertFalse((self.directory / "control-token").exists())


if __name__ == "__main__":
    unittest.main()
