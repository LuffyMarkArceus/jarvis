#!/usr/bin/env python3
"""Jarvis fetch MCP server (stdio, JSON-RPC line-delimited).

One tool: ``fetch(url)`` -> readable text.

HTML cleanup lives here, not in the orchestrator, because it is specific to
this tool. The orchestrator should not grow a tag stripper for every MCP
server that returns a document.

The module is import-safe: the stdio loop only runs under ``__main__`` so the
extraction helpers can be unit tested without a subprocess.
"""

from __future__ import annotations

import json
import re
import sys
import urllib.request
from html import unescape
from html.parser import HTMLParser
from typing import Optional

PROTOCOL_VERSION = "2024-11-05"
DEFAULT_MAX_LENGTH = 6000
# A model can pass 0, a negative number, or a wild value. Bounding it here
# keeps a bad argument from emptying the result or dumping a megabyte into
# the prompt.
MAX_ALLOWED_LENGTH = 20000
MIN_ALLOWED_LENGTH = 500
USER_AGENT = "Mozilla/5.0 (compatible; Jarvis/1.0)"

# Tags that never contribute readable article text. `nav`/`aside`/`footer`
# are skipped as subtrees, not just as empty elements: on modern Wikipedia the
# entire language list, table of contents and page-tools menu live *inside*
# `<main>`, so selecting the main region alone still returns site chrome
# ahead of the article.
SKIP_TAGS = {
    "script",
    "style",
    "noscript",
    "svg",
    "head",
    "template",
    "nav",
    "aside",
    "footer",
    "form",
    "iframe",
    "canvas",
    "video",
    "audio",
}

# Class/id/role fragments that mark chrome rather than content. These are
# matched as substrings because they are long enough not to collide with
# ordinary words.
NOISE_FRAGMENTS = (
    # Wikipedia/Vector chrome. Deliberately specific: `vector-body` is the
    # article itself, so a blanket `vector-` match would delete the page.
    "vector-page-titlebar",
    "vector-toc",
    "vector-main-menu",
    "vector-page-tools",
    "vector-appearance",
    "vector-user-links",
    "vector-search",
    "vector-sticky-header",
    "vector-dropdown",
    "mw-portlet",
    "mw-editsection",
    "printfooter",
    "catlinks",
    "navbox",
    "noprint",
    "mw-indicators",
    "mw-jump-link",
    "mw-ref",
    # Generic site chrome.
    "site-header",
    "site-footer",
    "breadcrumb",
    "sidebar",
    # MediaWiki portal boxes (`p-lang`, `p-views`, ...).
    "p-lang",
    "p-namespaces",
    "p-views",
    "p-actions",
    "p-personal",
    "p-search",
    "p-tb",
    "cookie-banner",
    "advert",
)

# Whole-word noise markers. Matched as tokens, so `protocol` and `stock` are
# not mistaken for `toc`.
NOISE_TOKENS = {
    # Citation markers (`<sup class="reference">[1]</sup>`) are noise in
    # running text: they interrupt sentences and cost tokens.
    "reference",
    "nav",
    "navbar",
    "navigation",
    "menu",
    "toc",
    "breadcrumb",
    "breadcrumbs",
    "advert",
    "ads",
    "share",
    "social",
}

VOID_TAGS = {
    "area",
    "base",
    "br",
    "col",
    "embed",
    "hr",
    "img",
    "input",
    "link",
    "meta",
    "param",
    "source",
    "track",
    "wbr",
}

BLOCK_TAGS = {
    "p",
    "div",
    "br",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "li",
    "tr",
    "section",
    "article",
    "header",
    "blockquote",
    "dd",
    "dt",
    "pre",
    "figcaption",
}


def _is_noise(
    tag: str, attrs: list[tuple[str, Optional[str]]], prune_classes: bool = True
) -> bool:
    """Is this element site chrome that should be dropped with its subtree?"""
    if tag in SKIP_TAGS:
        return True

    # Hidden content is never part of the readable page. Wikipedia keeps a
    # machine-readable duplicate of dates in `style="display:none"`, which
    # otherwise appears in the text as a second, slightly different date.
    for key, value in attrs:
        if key == "hidden":
            return True
        if key == "aria-hidden" and (value or "").lower() == "true":
            return True
        if key == "style" and re.search(
            r"display\s*:\s*none|visibility\s*:\s*hidden", value or "", re.I
        ):
            return True

    if not prune_classes:
        return False
    for key, value in attrs:
        if not value or key not in ("class", "id", "role", "aria-label"):
            continue
        lowered = value.lower()
        if any(fragment in lowered for fragment in NOISE_FRAGMENTS):
            return True
        if any(token in NOISE_TOKENS for token in re.split(r"[\s_-]+", lowered)):
            return True
    return False


class _TextExtractor(HTMLParser):
    """Collect visible text, preserving paragraph and list boundaries.

    Tracks the open-tag stack so a skipped subtree is dropped as a whole.
    `_skip_depth` counts noise elements currently open; while it is non-zero
    no text is emitted.
    """

    def __init__(self, prune_classes: bool = True) -> None:
        super().__init__(convert_charrefs=True)
        self.parts: list[str] = []
        self._open: list[tuple[str, bool]] = []
        self._skip_depth = 0
        self._prune_classes = prune_classes

    def handle_starttag(
        self, tag: str, attrs: list[tuple[str, Optional[str]]]
    ) -> None:
        tag = tag.lower()
        noise = _is_noise(tag, attrs, self._prune_classes)
        if tag not in VOID_TAGS:
            self._open.append((tag, noise))
            if noise:
                self._skip_depth += 1
        if self._skip_depth:
            return
        if tag in BLOCK_TAGS:
            self.parts.append("\n")
        elif tag in {"td", "th"}:
            self.parts.append(" | ")

    def handle_startendtag(
        self, tag: str, attrs: list[tuple[str, Optional[str]]]
    ) -> None:
        # `<br/>`, `<img/>` and friends: a boundary, never content.
        if self._skip_depth:
            return
        if tag.lower() in BLOCK_TAGS:
            self.parts.append("\n")

    def handle_endtag(self, tag: str) -> None:
        tag = tag.lower()
        # Pop to the last matching open tag. Real pages leave tags unclosed;
        # refusing to recover would leave `_skip_depth` stuck above zero and
        # silently drop the rest of the document.
        for index in range(len(self._open) - 1, -1, -1):
            if self._open[index][0] == tag:
                for _, skipped in self._open[index:]:
                    if skipped:
                        self._skip_depth -= 1
                del self._open[index:]
                break
        if self._skip_depth:
            return
        if tag in BLOCK_TAGS or tag == "li":
            self.parts.append("\n")

    def handle_data(self, data: str) -> None:
        if self._skip_depth:
            return
        text = data.strip()
        if not text:
            return
        # Inline markup often has no whitespace between elements
        # (`<a>Rust</a><b>is</b>`). Stripping each chunk would glue the words
        # together, so restore the boundary the author intended.
        if self.parts:
            previous = self.parts[-1]
            ends_a_word = previous and (
                previous[-1].isalnum() or previous[-1] in ",;:"
            )
            if ends_a_word and text[0].isalnum():
                self.parts.append(" ")
        self.parts.append(text)

    def text(self) -> str:
        return collapse_whitespace("".join(self.parts))


def collapse_whitespace(text: str) -> str:
    """Normalise runs of spaces and blank lines without losing paragraphs."""
    text = re.sub(r"[ \t]+", " ", text)
    text = re.sub(r" *\n *", "\n", text)
    text = re.sub(r"\n{3,}", "\n\n", text)
    return text.strip()


def extract_title(html: str) -> Optional[str]:
    match = re.search(r"<title[^>]*>(.*?)</title>", html, re.I | re.S)
    if not match:
        return None
    title = re.sub(r"<[^>]+>", "", match.group(1))
    title = collapse_whitespace(unescape(title))
    return title or None


def _strip_noise(html: str) -> str:
    """Remove non-text elements whose contents can confuse the parser.

    Scripts frequently contain `<div>` strings that would otherwise be parsed
    as markup, so they go first, by regex. Structural chrome is handled by
    `_TextExtractor`'s skip stack, which understands nesting.
    """
    html = re.sub(r"<script[^>]*>.*?</script>", " ", html, flags=re.I | re.S)
    html = re.sub(r"<style[^>]*>.*?</style>", " ", html, flags=re.I | re.S)
    html = re.sub(r"<noscript[^>]*>.*?</noscript>", " ", html, flags=re.I | re.S)
    html = re.sub(r"<!--.*?-->", " ", html, flags=re.I | re.S)
    return html


def _main_region(html: str) -> str:
    """Trim everything before the main content container, when one is marked.

    Returns the *remainder* of the document rather than a slice up to the
    matching close tag. HTML cannot be matched with a non-greedy regex once
    containers nest: `<div role="main">` on docs.python.org contains dozens of
    nested divs, and slicing to the first `</div>` returned one paragraph.
    Everything after the container is site chrome, which the extractor's
    skip stack removes by tag and class.

    This is an optimisation, not a correctness requirement: if no container is
    found the whole document is used.
    """
    for pattern in (
        r"<main[^>]*>",
        r"<article[^>]*>",
        r'<div[^>]*role=["\']main["\'][^>]*>',
        r'<div[^>]*id=["\']content["\'][^>]*>',
    ):
        match = re.search(pattern, html, flags=re.I)
        if match:
            tail = html[match.end() :]
            if len(tail) > 400:
                return tail
    return html


def _extract(html: str, prune_classes: bool) -> str:
    extractor = _TextExtractor(prune_classes=prune_classes)
    try:
        extractor.feed(html)
        return extractor.text()
    except Exception:
        # Malformed markup must not lose the whole page.
        text = re.sub(r"<[^>]+>", " ", html)
        return collapse_whitespace(text)


def extract_text(html: str) -> str:
    """Visible article text of an HTML document.

    Class-based pruning is a heuristic: an unfamiliar site could use one of
    the same class names for its actual content. Rather than return an empty
    page in that case, retry with tag-only pruning.

    The retry only fires when pruning left *nothing*. Triggering it merely
    because the page is short would put the chrome back for every small
    document, because unpruned text is always at least as long.
    """
    cleaned = _main_region(_strip_noise(html))
    text = _extract(cleaned, prune_classes=True)
    if text.strip():
        return text
    return _extract(cleaned, prune_classes=False)


def looks_like_html(content_type: str, body: str) -> bool:
    if "html" in content_type.lower() or "xml" in content_type.lower():
        return True
    head = body[:512].lower()
    return "<html" in head or "<!doctype html" in head or "<body" in head


def clamp_length(value) -> int:
    """Coerce a model-supplied max_length into a usable bound."""
    try:
        length = int(value)
    except (TypeError, ValueError):
        return DEFAULT_MAX_LENGTH
    if length <= 0:
        return DEFAULT_MAX_LENGTH
    return max(MIN_ALLOWED_LENGTH, min(length, MAX_ALLOWED_LENGTH))


def format_result(url: str, title: Optional[str], body: str, max_length: int) -> str:
    parts = []
    if title:
        parts.append(f"Title: {title}")
    parts.append(f"URL: {url}")
    parts.append("")
    parts.append(body)
    out = "\n".join(parts)
    if len(out) > max_length:
        out = out[: max_length - 24].rstrip() + "\n\n[Content truncated...]"
    return out


def fetch_url(url: str, max_length: int = DEFAULT_MAX_LENGTH) -> str:
    """Fetch *url* and return clean text. Raises on network/HTTP errors."""
    if not url:
        raise ValueError("missing url")
    max_length = clamp_length(max_length)
    request = urllib.request.Request(
        url,
        headers={
            "User-Agent": USER_AGENT,
            "Accept": "text/html,application/xhtml+xml,application/xml;q=0.9,text/plain;q=0.8,*/*;q=0.5",
        },
    )
    with urllib.request.urlopen(request, timeout=15) as response:
        content_type = response.headers.get("Content-Type", "")
        raw = response.read()

    charset = "utf-8"
    match = re.search(r"charset=([^;\s]+)", content_type, re.I)
    if match:
        charset = match.group(1).strip("\"'")
    try:
        body = raw.decode(charset, errors="replace")
    except LookupError:
        body = raw.decode("utf-8", errors="replace")

    if looks_like_html(content_type, body):
        title = extract_title(body)
        text = extract_text(body)
    else:
        title = None
        text = body.strip()

    return format_result(url, title, text, max_length)


def send(message: dict) -> None:
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()


def _ok(request_id, result: dict) -> None:
    send({"jsonrpc": "2.0", "id": request_id, "result": result})


def _error(request_id, code: int, message: str) -> None:
    send({"jsonrpc": "2.0", "id": request_id, "error": {"code": code, "message": message}})


def handle_request(request: dict) -> None:
    """Handle one JSON-RPC message. Notifications produce no response."""
    method = request.get("method")
    request_id = request.get("id")

    if method == "initialize":
        _ok(
            request_id,
            {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "fetch", "version": "1.1.0"},
            },
        )
    elif method == "notifications/initialized":
        return
    elif method == "tools/list":
        _ok(
            request_id,
            {
                "tools": [
                    {
                        "name": "fetch",
                        "description": (
                            "Fetch a URL and return its readable text content "
                            "(HTML is stripped, scripts and styles removed)."
                        ),
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "url": {
                                    "type": "string",
                                    "description": "Absolute http(s) URL to fetch",
                                },
                                "max_length": {
                                    "type": "integer",
                                    "description": "Maximum characters to return (default 6000)",
                                },
                            },
                            "required": ["url"],
                        },
                    }
                ]
            },
        )
    elif method == "tools/call":
        params = request.get("params") or {}
        if params.get("name") != "fetch":
            _error(request_id, -32601, f"unknown tool: {params.get('name')}")
            return
        args = params.get("arguments") or {}
        url = args.get("url", "")
        max_length = clamp_length(args.get("max_length", DEFAULT_MAX_LENGTH))
        try:
            text = fetch_url(url, max_length)
            _ok(request_id, {"content": [{"type": "text", "text": text}]})
        except Exception as exc:  # noqa: BLE001 - reported to the caller
            _ok(
                request_id,
                {
                    "content": [{"type": "text", "text": f"Error fetching {url}: {exc}"}],
                    "isError": True,
                },
            )
    elif request_id is not None:
        _error(request_id, -32601, f"method not found: {method}")


def main() -> None:
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            request = json.loads(line)
        except json.JSONDecodeError:
            continue
        try:
            handle_request(request)
        except Exception:
            # A single bad message must not kill the server.
            continue


if __name__ == "__main__":
    main()
