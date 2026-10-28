"""Record through the tape proxy, then replay without the real server."""

import json
import os
import subprocess
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
TAPE = os.path.join(HERE, "tape_mcp.py")
ECHO = os.path.join(HERE, "..", "core", "tests", "fixtures", "echo_mcp.py")


def session(args, *requests):
    lines = "".join(json.dumps(r) + "\n" for r in requests)
    out = subprocess.run(
        [sys.executable, TAPE, *args], input=lines, capture_output=True, text=True, timeout=30
    ).stdout
    return [json.loads(line) for line in out.splitlines() if line.strip()]


def call(request_id, text):
    return {
        "jsonrpc": "2.0",
        "id": request_id,
        "method": "tools/call",
        "params": {"name": "echo", "arguments": {"text": text}},
    }


LIST = {"jsonrpc": "2.0", "id": 1, "method": "tools/list"}


class TapeTest(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.tape = os.path.join(self.dir.name, "echo.json")

    def tearDown(self):
        self.dir.cleanup()

    def test_a_recorded_call_replays_without_the_server(self):
        recorded = session(
            ["--tape", self.tape, "--record", "--", sys.executable, ECHO], LIST, call(2, "hi")
        )
        replayed = session(["--tape", self.tape], LIST, call(2, "hi"))
        self.assertEqual(recorded, replayed)
        self.assertEqual(replayed[1]["result"]["content"][0]["text"], "echo: hi")

    def test_a_call_not_on_the_tape_is_a_tool_error(self):
        session(["--tape", self.tape, "--record", "--", sys.executable, ECHO], LIST, call(2, "hi"))
        reply = session(["--tape", self.tape], call(2, "something else"))[0]
        self.assertTrue(reply["result"]["isError"])
        self.assertIn("not on tape", reply["result"]["content"][0]["text"])

    def test_recording_again_adds_calls_and_keeps_recorded_answers(self):
        session(["--tape", self.tape, "--record", "--", sys.executable, ECHO], LIST, call(2, "hi"))
        with open(self.tape, encoding="utf-8") as f:
            tape = json.load(f)
        tape["calls"][0]["result"]["content"][0]["text"] = "as recorded last month"
        with open(self.tape, "w", encoding="utf-8") as f:
            json.dump(tape, f)

        session(
            ["--tape", self.tape, "--record", "--", sys.executable, ECHO],
            LIST,
            call(2, "hi"),
            call(3, "new"),
        )
        replayed = session(["--tape", self.tape], call(2, "hi"), call(3, "new"))
        texts = [r["result"]["content"][0]["text"] for r in replayed]
        self.assertEqual(texts, ["as recorded last month", "echo: new"])

    def test_a_tool_error_is_passed_on_but_not_recorded(self):
        failing = {
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {"name": "boom", "arguments": {}},
        }
        recorded = session(
            ["--tape", self.tape, "--record", "--", sys.executable, ECHO], LIST, failing
        )
        self.assertTrue(recorded[1]["result"]["isError"])
        replayed = session(["--tape", self.tape], failing)[0]
        self.assertIn("not on tape", replayed["result"]["content"][0]["text"])

    def test_a_missing_tape_cannot_list_tools(self):
        reply = session(["--tape", self.tape], LIST)[0]
        self.assertIn("error", reply)


if __name__ == "__main__":
    unittest.main()
