"""Contract tests for agent_radio.

Everything runs against a throwaway AGENT_RADIO_DIR, so no git worktree is
required and nothing touches a real store.
"""

from __future__ import annotations

import contextlib
import io
import json
import os
import tempfile
import unittest
from pathlib import Path

import agent_radio


class RadioTestCase(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self._env = dict(os.environ)
        os.environ["AGENT_RADIO_DIR"] = self._tmp.name
        os.environ.pop("AGENT_RADIO_AGENT", None)

    def tearDown(self) -> None:
        os.environ.clear()
        os.environ.update(self._env)
        self._tmp.cleanup()

    def run_cli(self, *argv: str) -> str:
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            agent_radio.build_parser().parse_args(argv).func(
                agent_radio.build_parser().parse_args(argv)
            )
        return out.getvalue()

    def messages(self) -> list[dict]:
        path = Path(self._tmp.name) / "messages.jsonl"
        if not path.exists():
            return []
        return [json.loads(l) for l in path.read_text().splitlines() if l.strip()]


class StoreEnvTests(RadioTestCase):
    def test_store_honors_agent_radio_dir_outside_git(self) -> None:
        self.assertEqual(agent_radio.store_root(), Path(self._tmp.name))

    def test_send_writes_message_to_env_store(self) -> None:
        out = self.run_cli(
            "send", "--from", "a", "--to", "b", "--kind", "FYI",
            "--body", "hello", "--branch", "",
        )
        self.assertIn("sent FYI a -> b", out)
        msgs = self.messages()
        self.assertEqual(len(msgs), 1)
        self.assertEqual(msgs[0]["body"], "hello")
        self.assertNotIn("branch", msgs[0])

    def test_identity_from_env_var(self) -> None:
        os.environ["AGENT_RADIO_AGENT"] = "envbot"
        self.run_cli("send", "--to", "b", "--body", "hi", "--branch", "")
        self.assertEqual(self.messages()[0]["from"], "envbot")

    def test_missing_identity_fails(self) -> None:
        with self.assertRaises(SystemExit):
            self.run_cli("send", "--to", "b", "--body", "hi", "--branch", "")


class FlowTests(RadioTestCase):
    def test_inbox_marks_read_and_peek_does_not(self) -> None:
        self.run_cli("send", "--from", "a", "--to", "b", "--body", "q1", "--branch", "")
        peeked = self.run_cli("inbox", "--as", "b", "--peek")
        self.assertIn("q1", peeked)
        again = self.run_cli("inbox", "--as", "b")
        self.assertIn("q1", again)
        empty = self.run_cli("inbox", "--as", "b")
        self.assertIn("empty", empty)

    def test_reply_threads_and_targets_sender(self) -> None:
        self.run_cli(
            "send", "--from", "a", "--to", "b", "--kind", "ASK",
            "--body", "can you?", "--branch", "",
        )
        self.run_cli("inbox", "--as", "b")
        self.run_cli("done", "1", "--as", "b", "--body", "shipped")
        msgs = self.messages()
        self.assertEqual(len(msgs), 2)
        reply = msgs[1]
        self.assertEqual(reply["kind"], "DONE")
        self.assertEqual(reply["to"], "a")
        self.assertEqual(reply["reply_to"], msgs[0]["id"])
        self.assertEqual(reply["thread_id"], msgs[0]["id"])

    def test_broadcast_reaches_known_agents_not_sender(self) -> None:
        self.run_cli("send", "--from", "a", "--to", "b", "--body", "x", "--branch", "")
        self.run_cli("send", "--from", "a", "--to", "all", "--body", "heads up", "--branch", "")
        b_inbox = self.run_cli("inbox", "--as", "b")
        self.assertIn("heads up", b_inbox)
        a_inbox = self.run_cli("inbox", "--as", "a")
        self.assertIn("empty", a_inbox)

    def test_status_counts_unread(self) -> None:
        self.run_cli("send", "--from", "a", "--to", "b", "--body", "x", "--branch", "")
        status = json.loads(self.run_cli("status", "--as", "b"))
        self.assertEqual(status["unread"], 1)


class GuardTests(RadioTestCase):
    def test_secret_looking_body_is_rejected(self) -> None:
        with self.assertRaises(SystemExit):
            self.run_cli(
                "send", "--from", "a", "--to", "b", "--branch", "",
                "--body", "api_key = 'sk-proj-abcdefghijklmnopqrstuvwxyz0123456789ABCD'",
            )
        self.assertEqual(self.messages(), [])

    def test_invalid_kind_rejected(self) -> None:
        with self.assertRaises(SystemExit):
            self.run_cli(
                "send", "--from", "a", "--to", "b", "--kind", "NOPE",
                "--body", "x", "--branch", "",
            )

    def test_invalid_agent_name_rejected(self) -> None:
        with self.assertRaises(SystemExit):
            self.run_cli(
                "send", "--from", "bad name!", "--to", "b", "--body", "x", "--branch", "",
            )


if __name__ == "__main__":
    unittest.main()
