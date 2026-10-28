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



def page(url, title, body):
    return {
        "tool": "fetch",
        "args": {"url": url},
        "result": {"content": [{"type": "text", "text": f"Title: {title}\nURL: {url}\n\n{body}"}]},
    }


def search(request_id, query, **extra):
    return {
        "jsonrpc": "2.0",
        "id": request_id,
        "method": "tools/call",
        "params": {"name": "search", "arguments": {"query": query, **extra}},
    }


class FrozenPagesTest(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.search = os.path.join(self.dir.name, "search.json")
        self.fetch = os.path.join(self.dir.name, "fetch.json")
        recorded = {"content": [{"type": "text", "text": "as recorded"}]}
        with open(self.search, "w", encoding="utf-8") as f:
            json.dump({"tools": [], "calls": [{"tool": "search", "args": {"query": "rust"}, "result": recorded}]}, f)
        with open(self.fetch, "w", encoding="utf-8") as f:
            json.dump(
                {
                    "tools": [],
                    "calls": [
                        page("https://en.wikipedia.org/wiki/Rust_shooting_incident", "Rust shooting incident - Wikipedia",
                             "On the set of the film Rust a prop gun was discharged."),
                        page("https://blog.rust-lang.org/", "The Rust Programming Language Blog",
                             "Announcing Rust 1.82.0. The Rust team is happy to announce a new version of the programming language."),
                    ],
                },
                f,
            )

    def tearDown(self):
        self.dir.cleanup()

    def ask(self, *requests, pages=True):
        args = ["--tape", self.search] + (["--pages", self.fetch] if pages else [])
        return [r["result"] for r in session(args, *requests)]

    def text(self, result):
        return result["content"][0]["text"]

    def test_a_search_on_the_tape_is_replayed_exactly(self):
        self.assertEqual(self.text(self.ask(search(1, "rust"))[0]), "as recorded")

    def test_a_reworded_search_is_answered_from_the_recorded_pages(self):
        result = self.ask(search(1, "Rust programming language news 2024", backend="brave"))[0]
        text = self.text(result)
        self.assertNotIn("isError", result)
        self.assertTrue(text.startswith("Backend: recorded pages"), text)
        # The title match ranks the language's blog above the film.
        self.assertLess(text.index("blog.rust-lang.org"), text.index("Rust_shooting_incident"))

    def test_a_search_matching_no_page_has_no_results(self):
        result = self.ask(search(1, "postgres release"))[0]
        self.assertTrue(result["isError"])
        self.assertIn("no results from recorded pages", self.text(result))

    def test_without_pages_a_miss_is_still_not_on_tape(self):
        result = self.ask(search(1, "Rust programming language"), pages=False)[0]
        self.assertIn("not on tape", self.text(result))

    def test_a_page_is_found_by_its_address_whatever_else_was_asked(self):
        fetch = {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": "fetch", "arguments": {"url": "https://blog.rust-lang.org", "max_length": 8000}},
        }
        result = session(["--tape", self.fetch], fetch)[0]["result"]
        self.assertIn("Announcing Rust 1.82.0", self.text(result))

if __name__ == "__main__":
    unittest.main()
