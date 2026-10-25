#!/usr/bin/env python3
"""Offline tests for the search sidecar.

Every parser is fed a captured-shaped payload, so the suite never touches the
network. The live behaviour that a unit test cannot cover (whether an engine
blocks this host) is exactly why the backends are explicit and reported.
"""

from __future__ import annotations

import io
import json
import os
import sys
import unittest
from contextlib import redirect_stdout

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import server  # noqa: E402


WIKIPEDIA_PAYLOAD = {
    "query": {
        "search": [
            {
                "title": "Rust (programming language)",
                "snippet": 'Rust is a <span class="searchmatch">general-purpose</span> '
                "programming language &amp; more",
            },
            {"title": "Rust", "snippet": "Rust is an iron oxide."},
            {"title": "", "snippet": "ignored: no title"},
        ]
    }
}

INSTANT_PAYLOAD = {
    "Heading": "Rust (programming language)",
    "AbstractText": "Rust is a general-purpose programming language.",
    "AbstractURL": "https://en.wikipedia.org/wiki/Rust_(programming_language)",
    "Results": [
        {"Text": "Rust documentation", "FirstURL": "https://doc.rust-lang.org/"}
    ],
    "RelatedTopics": [
        {
            "Topics": [
                {"Text": "Rust on GitHub", "FirstURL": "https://github.com/rust-lang"}
            ]
        },
        {"Text": "Rust (disambiguation)", "FirstURL": "https://example.org/rust"},
        {"Name": "a topic without a URL"},
    ],
}

SEARX_PAYLOAD = {
    "results": [
        {
            "title": "Rust <b>Lang</b>",
            "url": "https://www.rust-lang.org/",
            "content": "A language empowering everyone.",
        },
        {
            "title": "Redirected",
            "url": "//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fpage&rut=x",
            "content": "",
        },
        {"title": "no url", "content": ""},
    ]
}

BRAVE_PAYLOAD = {
    "web": {
        "results": [
            {
                "title": "Rust Programming Language",
                "url": "https://www.rust-lang.org/",
                "description": "A language empowering everyone.",
            }
        ]
    }
}


class WikipediaParsingTests(unittest.TestCase):
    def test_results_are_mapped_to_title_url_snippet(self):
        results = server.parse_wikipedia(WIKIPEDIA_PAYLOAD)
        self.assertEqual(len(results), 2)
        self.assertEqual(results[0].title, "Rust (programming language)")
        self.assertEqual(
            results[0].url,
            "https://en.wikipedia.org/wiki/Rust_(programming_language)",
        )
        self.assertEqual(
            results[0].snippet,
            "Rust is a general-purpose programming language & more",
        )

    def test_a_missing_title_is_skipped(self):
        results = server.parse_wikipedia(WIKIPEDIA_PAYLOAD)
        self.assertNotIn("", [r.title for r in results])

    def test_spaces_become_underscores_but_parens_survive(self):
        results = server.parse_wikipedia(
            {"query": {"search": [{"title": "Rust (programming language)"}]}}
        )
        self.assertNotIn(" ", results[0].url)
        self.assertIn("(programming_language)", results[0].url)


class InstantParsingTests(unittest.TestCase):
    def test_abstract_results_and_related_topics_are_all_used(self):
        results = server.parse_instant(INSTANT_PAYLOAD)
        urls = [r.url for r in results]
        self.assertIn("https://en.wikipedia.org/wiki/Rust_(programming_language)", urls)
        self.assertIn("https://doc.rust-lang.org/", urls)
        # Nested `Topics` must be flattened, not dropped.
        self.assertIn("https://github.com/rust-lang", urls)
        self.assertIn("https://example.org/rust", urls)

    def test_an_empty_payload_yields_no_results(self):
        self.assertEqual(server.parse_instant({}), [])


class SearxParsingTests(unittest.TestCase):
    def test_redirect_wrappers_are_unwrapped(self):
        results = server.parse_searx(SEARX_PAYLOAD)
        self.assertIn("https://example.com/page", [r.url for r in results])

    def test_tags_are_stripped_from_titles(self):
        results = server.parse_searx(SEARX_PAYLOAD)
        self.assertEqual(results[0].title, "Rust Lang")

    def test_entries_without_a_url_are_skipped(self):
        results = server.parse_searx(SEARX_PAYLOAD)
        self.assertEqual(len(results), 2)


class BraveParsingTests(unittest.TestCase):
    def test_web_results_are_mapped(self):
        results = server.parse_brave(BRAVE_PAYLOAD)
        self.assertEqual(len(results), 1)
        self.assertEqual(results[0].url, "https://www.rust-lang.org/")

    def test_an_empty_payload_yields_no_results(self):
        self.assertEqual(server.parse_brave({}), [])


class UrlTests(unittest.TestCase):
    def test_duckduckgo_redirect_is_unwrapped(self):
        wrapped = "//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fa%3Fb%3D1&rut=z"
        self.assertEqual(server.unwrap_redirect(wrapped), "https://example.com/a?b=1")

    def test_a_plain_url_is_unchanged(self):
        self.assertEqual(
            server.unwrap_redirect("https://example.com/x"), "https://example.com/x"
        )

    def test_protocol_relative_urls_get_https(self):
        self.assertEqual(
            server.unwrap_redirect("//example.com/x"), "https://example.com/x"
        )

    def test_duplicates_are_removed(self):
        results = server.dedupe(
            [
                server.Result("a", "https://example.com"),
                server.Result("b", "https://example.com"),
                server.Result("c", "https://other.example"),
            ]
        )
        self.assertEqual([r.url for r in results], ["https://example.com", "https://other.example"])


class RenderingTests(unittest.TestCase):
    def test_backend_and_query_are_reported(self):
        out = server.render(
            "wikipedia", "rust", [server.Result("Rust", "https://x.example", "s")], 3000
        )
        self.assertTrue(out.startswith("Backend: wikipedia\nQuery: rust"))
        self.assertIn("1. Rust", out)
        self.assertIn("   https://x.example", out)

    def test_output_is_bounded(self):
        results = [server.Result("t" * 200, "https://x.example/" + str(i), "s" * 200) for i in range(10)]
        out = server.render("wikipedia", "q", results, 500)
        self.assertLessEqual(len(out), 500)
        self.assertIn("[Results truncated...]", out)

    def test_a_snippet_is_optional(self):
        out = server.render("wikipedia", "q", [server.Result("T", "https://x.example")], 300)
        self.assertIn("1. T", out)
        self.assertIn("   https://x.example", out)


class ClampTests(unittest.TestCase):
    def test_result_count_is_bounded(self):
        self.assertEqual(server.clamp_results(None), server.DEFAULT_MAX_RESULTS)
        self.assertEqual(server.clamp_results(0), server.DEFAULT_MAX_RESULTS)
        self.assertEqual(server.clamp_results(-3), server.DEFAULT_MAX_RESULTS)
        self.assertEqual(server.clamp_results("nonsense"), server.DEFAULT_MAX_RESULTS)
        self.assertEqual(server.clamp_results(500), server.MAX_ALLOWED_RESULTS)

    def test_length_is_bounded(self):
        self.assertEqual(server.clamp_length(None), server.DEFAULT_MAX_LENGTH)
        self.assertEqual(server.clamp_length(0), server.DEFAULT_MAX_LENGTH)
        self.assertEqual(server.clamp_length(10**9), server.MAX_ALLOWED_LENGTH)


class QueryTests(unittest.TestCase):
    """MediaWiki is a keyword index; question framing pushes the wrong page
    to the top. Measured on \"who won the 2026 nobel prize in physics\"."""

    def test_a_who_question_becomes_its_subject(self):
        self.assertEqual(
            server.keywords("who won the 2026 nobel prize in physics"),
            "2026 nobel prize in physics",
        )

    def test_a_what_question_loses_its_opening(self):
        self.assertEqual(
            server.keywords("what is the latest version of rust"),
            "latest version of rust",
        )

    def test_a_plain_phrase_is_unchanged(self):
        self.assertEqual(server.keywords("millennium prize problems"), "millennium prize problems")

    def test_only_the_opening_is_touched(self):
        # \"The Who\" as a subject must survive.
        self.assertEqual(server.keywords("which albums did the who release"), "albums did the who release")

    def test_an_empty_question_is_not_emptied_further(self):
        self.assertEqual(server.keywords("   "), "")

    def test_the_wikipedia_backend_searches_the_normalised_terms(self):
        seen = {}

        def fake_get(url, headers=None, timeout=15):
            seen["url"] = url
            return {"query": {"search": []}}

        saved = server._get_json
        server._get_json = fake_get
        try:
            server.search_wikipedia("who won the 2026 nobel prize in physics", 3)
        finally:
            server._get_json = saved
        self.assertIn("2026+nobel+prize+in+physics", seen["url"])
        self.assertNotIn("who", seen["url"])


class DispatchTests(unittest.TestCase):
    """The fallback chain, without touching the network."""

    def setUp(self):
        self.calls = []
        self.saved = {
            "wikipedia": server.search_wikipedia,
            "instant": server.search_instant,
            "searx": server.search_searx,
            "brave": server.search_brave,
        }

    def tearDown(self):
        server.search_wikipedia = self.saved["wikipedia"]
        server.search_instant = self.saved["instant"]
        server.search_searx = self.saved["searx"]
        server.search_brave = self.saved["brave"]

    def test_auto_prefers_a_configured_general_backend(self):
        server.search_wikipedia = lambda q, n: self.calls.append("wikipedia") or []
        server.search_searx = lambda base, q, n: self.calls.append("searx") or [
            server.Result("S", "https://searx.example")
        ]
        out = server.search(
            "q", env={"JARVIS_SEARX_URL": "https://searx.example"}
        )
        self.assertEqual(self.calls, ["searx"])
        self.assertTrue(out.startswith("Backend: searx"))

    def test_auto_falls_back_to_instant_when_wikipedia_is_empty(self):
        server.search_wikipedia = lambda q, n: []
        server.search_instant = lambda q, n: [server.Result("I", "https://i.example")]
        out = server.search("q", env={})
        self.assertTrue(out.startswith("Backend: instant"))

    def test_auto_reports_the_backend_that_produced_results(self):
        server.search_wikipedia = lambda q, n: [server.Result("W", "https://w.example")]
        out = server.search("q", env={})
        self.assertTrue(out.startswith("Backend: wikipedia"))

    def test_no_results_is_an_error_that_names_what_was_tried(self):
        server.search_wikipedia = lambda q, n: []
        server.search_instant = lambda q, n: []
        with self.assertRaises(server.SearchError) as ctx:
            server.search("q", env={})
        self.assertIn("wikipedia", str(ctx.exception))
        self.assertIn("instant", str(ctx.exception))

    def test_a_failing_backend_does_not_prevent_the_fallback(self):
        def boom(q, n):
            raise RuntimeError("blocked")

        server.search_wikipedia = boom
        server.search_instant = lambda q, n: [server.Result("I", "https://i.example")]
        out = server.search("q", env={})
        self.assertTrue(out.startswith("Backend: instant"))

    def test_an_explicit_backend_does_not_silently_switch(self):
        """Asking for Wikipedia and getting Instant Answers would misreport
        where the answer came from."""
        server.search_wikipedia = lambda q, n: []
        server.search_instant = lambda q, n: [server.Result("I", "https://i.example")]
        with self.assertRaises(server.SearchError):
            server.search("q", backend="wikipedia", env={})

    def test_searx_without_a_url_is_a_clear_error(self):
        with self.assertRaises(server.SearchError) as ctx:
            server.search("q", backend="searx", env={})
        self.assertIn("JARVIS_SEARX_URL", str(ctx.exception))

    def test_brave_without_a_key_is_a_clear_error(self):
        with self.assertRaises(server.SearchError) as ctx:
            server.search("q", backend="brave", env={})
        self.assertIn("JARVIS_BRAVE_KEY", str(ctx.exception))

    def test_an_unknown_backend_is_rejected(self):
        with self.assertRaises(server.SearchError):
            server.search("q", backend="google", env={})

    def test_an_empty_query_is_rejected(self):
        with self.assertRaises(ValueError):
            server.search("   ", env={})

    def test_results_respect_max_results(self):
        server.search_wikipedia = lambda q, n: [
            server.Result(str(i), f"https://x.example/{i}") for i in range(10)
        ]
        out = server.search("q", max_results=2, env={})
        self.assertIn("1. ", out)
        self.assertIn("2. ", out)
        self.assertNotIn("3. ", out)


class ProtocolTests(unittest.TestCase):
    def call(self, request):
        buffer = io.StringIO()
        with redirect_stdout(buffer):
            server.handle_request(request)
        lines = [line for line in buffer.getvalue().splitlines() if line.strip()]
        return json.loads(lines[0]) if lines else None

    def test_initialize_advertises_tools(self):
        response = self.call({"jsonrpc": "2.0", "id": 1, "method": "initialize"})
        self.assertEqual(response["id"], 1)
        self.assertEqual(response["result"]["serverInfo"]["name"], "search")
        self.assertIn("tools", response["result"]["capabilities"])

    def test_tools_list_describes_the_query_argument(self):
        response = self.call({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})
        tools = response["result"]["tools"]
        self.assertEqual(len(tools), 1)
        self.assertEqual(tools[0]["name"], "search")
        self.assertIn("query", tools[0]["inputSchema"]["required"])

    def test_unknown_tool_is_an_error(self):
        response = self.call(
            {"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "nope"}}
        )
        self.assertIn("unknown tool", response["error"]["message"])

    def test_a_failed_search_is_a_tool_error_not_a_crash(self):
        saved = server.search
        server.search = lambda *a, **k: (_ for _ in ()).throw(server.SearchError("blocked"))
        try:
            response = self.call(
                {
                    "jsonrpc": "2.0",
                    "id": 4,
                    "method": "tools/call",
                    "params": {"name": "search", "arguments": {"query": "q"}},
                }
            )
        finally:
            server.search = saved
        self.assertTrue(response["result"]["isError"])
        self.assertIn("blocked", response["result"]["content"][0]["text"])

    def test_a_successful_search_returns_text_content(self):
        saved = server.search
        server.search = lambda *a, **k: "Backend: wikipedia\nQuery: q"
        try:
            response = self.call(
                {
                    "jsonrpc": "2.0",
                    "id": 5,
                    "method": "tools/call",
                    "params": {"name": "search", "arguments": {"query": "q"}},
                }
            )
        finally:
            server.search = saved
        self.assertEqual(response["result"]["content"][0]["type"], "text")
        self.assertIn("Backend: wikipedia", response["result"]["content"][0]["text"])

    def test_an_unknown_method_is_an_error(self):
        response = self.call({"jsonrpc": "2.0", "id": 6, "method": "nope"})
        self.assertIn("method not found", response["error"]["message"])


if __name__ == "__main__":
    unittest.main()
