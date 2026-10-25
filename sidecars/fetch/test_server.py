#!/usr/bin/env python3
"""Unit tests for the fetch MCP server.

Run with:  python3 -m unittest discover -s sidecars/fetch -p 'test_*.py'
"""

import importlib.util
import io
import json
import pathlib
import unittest
from contextlib import redirect_stdout

MODULE_PATH = pathlib.Path(__file__).with_name("server.py")
spec = importlib.util.spec_from_file_location("jarvis_fetch_server", MODULE_PATH)
server = importlib.util.module_from_spec(spec)
assert spec.loader is not None
spec.loader.exec_module(server)

SAMPLE = """<!doctype html>
<html>
  <head>
    <title>  Example   Page </title>
    <style>body { color: red }</style>
    <script>console.log("noise")</script>
  </head>
  <body>
    <h1>Heading</h1>
    <p>First paragraph.</p>
    <p>Second   paragraph with   spaces.</p>
    <ul><li>One</li><li>Two</li></ul>
  </body>
</html>"""


class ExtractionTests(unittest.TestCase):
    def test_title_is_extracted_and_collapsed(self):
        self.assertEqual(server.extract_title(SAMPLE), "Example Page")

    def test_title_entities_are_decoded(self):
        self.assertEqual(
            server.extract_title("<title>A &#8212; B &amp; C</title>"), "A — B & C"
        )

    def test_scripts_and_styles_are_removed(self):
        text = server.extract_text(SAMPLE)
        self.assertNotIn("console.log", text)
        self.assertNotIn("color: red", text)

    def test_visible_text_survives(self):
        text = server.extract_text(SAMPLE)
        self.assertIn("Heading", text)
        self.assertIn("First paragraph.", text)
        self.assertIn("Second paragraph with spaces.", text)
        self.assertIn("One", text)
        self.assertIn("Two", text)

    def test_paragraphs_are_separated(self):
        text = server.extract_text("<p>one</p><p>two</p>")
        self.assertIn("\n", text)
        self.assertEqual([line for line in text.splitlines() if line], ["one", "two"])

    def test_malformed_html_still_yields_text(self):
        text = server.extract_text("<div>hello <b>world</b><div>")
        self.assertIn("hello", text)
        self.assertIn("world", text)

    def test_main_region_is_preferred(self):
        filler = "Real content. " * 50
        html = f'<nav>{"x" * 500}</nav><main><p>{filler}</p></main>'
        text = server.extract_text(html)
        self.assertIn("Real content.", text)
        self.assertNotIn("x" * 100, text)

    def test_plain_text_is_not_tag_stripped(self):
        body = "a < b and c > d\nsecond line"
        self.assertFalse(server.looks_like_html("text/plain", body))
        out = server.format_result("http://x", None, body, 1000)
        self.assertIn("a < b and c > d", out)


class NoisePruningTests(unittest.TestCase):
    """Wikipedia puts its chrome *inside* <main>, so region selection alone is
    not enough. These cases pin down what counts as chrome."""

    ARTICLE = """
    <html><body>
      <header class="site-header">Site name</header>
      <nav>Home About Contact</nav>
      <main id="content" class="mw-body">
        <header class="mw-body-header vector-page-titlebar">
          <h1>Rust</h1>
          <div id="p-lang-btn">59 languages</div>
        </header>
        <div id="bodyContent" class="vector-body">
          <p>Rust is a general-purpose programming language.</p>
          <aside>Related links</aside>
          <footer>Copyright notice</footer>
        </div>
      </main>
    </body></html>
    """

    def test_article_text_is_kept(self):
        text = server.extract_text(self.ARTICLE)
        self.assertIn("Rust is a general-purpose programming language.", text)

    def test_nav_subtree_is_removed(self):
        text = server.extract_text(self.ARTICLE)
        self.assertNotIn("Home", text)
        self.assertNotIn("Contact", text)

    def test_chrome_classes_are_removed(self):
        text = server.extract_text(self.ARTICLE)
        self.assertNotIn("59 languages", text)

    def test_article_body_class_is_not_treated_as_chrome(self):
        # `vector-body` wraps the article; a blanket `vector-` match would
        # delete the whole page. This is the regression that made extraction
        # return an empty string.
        text = server.extract_text(self.ARTICLE)
        self.assertIn("general-purpose", text)

    def test_aside_and_footer_are_removed(self):
        text = server.extract_text(self.ARTICLE)
        self.assertNotIn("Related links", text)
        self.assertNotIn("Copyright notice", text)

    def test_inline_tags_keep_word_boundaries(self):
        # The HTML has no whitespace between the inline elements.
        text = server.extract_text("<p>Rust<a>is</a><b>a</b> language</p>")
        self.assertIn("Rust is a language", text)
        self.assertNotIn("Rustisa", text)

    def test_hidden_elements_are_never_extracted(self):
        html = (
            '<p>visible text</p>'
            '<span style="display:none">machine date</span>'
            '<span style="DISPLAY : NONE">also hidden</span>'
            '<span aria-hidden="true">icon label</span>'
            '<p hidden>secret</p>'
        )
        text = server.extract_text(html)
        self.assertIn("visible text", text)
        for hidden in ("machine date", "also hidden", "icon label", "secret"):
            self.assertNotIn(hidden, text)

    def test_citation_markers_are_removed(self):
        html = (
            '<p>Version 1.0<sup class="mw-ref reference">'
            '<a href="#c1"><span class="cite-bracket">[</span>2'
            '<span class="cite-bracket">]</span></a></sup> released.</p>'
        )
        text = server.extract_text(html)
        self.assertEqual(text, "Version 1.0 released.")

    def test_punctuation_boundary_keeps_a_space(self):
        text = server.extract_text("<p>language,<span>not</span> beginners</p>")
        self.assertIn("language, not beginners", text)

    def test_ordinary_words_are_not_mistaken_for_chrome(self):
        text = server.extract_text(
            '<div class="protocol"><p>protocol detail</p></div>'
            '<div class="stock"><p>stock detail</p></div>'
        )
        self.assertIn("protocol detail", text)
        self.assertIn("stock detail", text)

    def test_a_site_using_a_chrome_class_for_content_still_returns_text(self):
        # Everything is inside `sidebar`, which class pruning would drop. The
        # fallback must not return an empty page.
        html = '<div class="sidebar">' + "content " * 50 + "</div>"
        text = server.extract_text(html)
        self.assertIn("content", text)


class FormattingTests(unittest.TestCase):
    def test_header_and_truncation(self):
        out = server.format_result("http://example.com", "T", "y" * 500, 100)
        self.assertTrue(out.startswith("Title: T\nURL: http://example.com"))
        self.assertLessEqual(len(out), 100)
        self.assertIn("[Content truncated...]", out)

    def test_zero_length_falls_back_to_the_default(self):
        self.assertEqual(server.clamp_length(0), server.DEFAULT_MAX_LENGTH)
        self.assertEqual(server.clamp_length("0"), server.DEFAULT_MAX_LENGTH)
        self.assertEqual(server.clamp_length(-5), server.DEFAULT_MAX_LENGTH)
        self.assertEqual(server.clamp_length("nonsense"), server.DEFAULT_MAX_LENGTH)
        self.assertEqual(server.clamp_length(None), server.DEFAULT_MAX_LENGTH)

    def test_length_is_bounded_in_both_directions(self):
        self.assertEqual(server.clamp_length(100), server.MIN_ALLOWED_LENGTH)
        self.assertEqual(server.clamp_length(10**9), server.MAX_ALLOWED_LENGTH)
        self.assertEqual(server.clamp_length(3000), 3000)

    def test_short_body_is_not_truncated(self):
        out = server.format_result("http://example.com", None, "hello", 1000)
        self.assertNotIn("truncated", out)
        self.assertIn("hello", out)


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
        self.assertIn("tools", response["result"]["capabilities"])

    def test_tools_list_describes_fetch(self):
        response = self.call({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})
        tools = response["result"]["tools"]
        self.assertEqual(len(tools), 1)
        self.assertEqual(tools[0]["name"], "fetch")
        self.assertIn("url", tools[0]["inputSchema"]["required"])

    def test_initialized_notification_is_silent(self):
        self.assertIsNone(
            self.call({"jsonrpc": "2.0", "method": "notifications/initialized"})
        )

    def test_unknown_method_is_an_error(self):
        response = self.call({"jsonrpc": "2.0", "id": 3, "method": "nope"})
        self.assertEqual(response["error"]["code"], -32601)

    def test_unknown_tool_is_an_error(self):
        response = self.call(
            {
                "jsonrpc": "2.0",
                "id": 4,
                "method": "tools/call",
                "params": {"name": "other", "arguments": {}},
            }
        )
        self.assertEqual(response["error"]["code"], -32601)

    def test_fetch_error_is_reported_not_raised(self):
        response = self.call(
            {
                "jsonrpc": "2.0",
                "id": 5,
                "method": "tools/call",
                "params": {"name": "fetch", "arguments": {"url": ""}},
            }
        )
        self.assertTrue(response["result"]["isError"])
        self.assertIn("Error fetching", response["result"]["content"][0]["text"])


if __name__ == "__main__":
    unittest.main()
