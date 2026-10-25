#!/usr/bin/env python3
"""Jarvis search MCP server (stdio, JSON-RPC line-delimited).

One tool: ``search(query)`` -> ranked results (title, URL, snippet).

# Why the backend is pluggable

General web search without a key is not reliably available: the HTML
endpoints of DuckDuckGo, Mojeek and Qwant answer this class of client with a
CAPTCHA or ``403``. Pretending otherwise would produce a tool that fails at
random. Instead every backend is explicit:

* ``wikipedia`` - the MediaWiki search API. Keyless and dependable; the right
  default for "what is X" and for version/fact questions, because the article
  it returns carries an infobox that :mod:`fetch` can read.
* ``instant``   - DuckDuckGo Instant Answers. Keyless; good for entity
  summaries, silent for most other queries.
* ``searx``     - any SearXNG instance, configured with ``JARVIS_SEARX_URL``.
  This is general web search.
* ``brave``     - the Brave Search API, configured with ``JARVIS_BRAVE_KEY``.

``auto`` prefers a configured general backend, because that is a deliberate
choice by the operator, and otherwise tries Wikipedia then Instant Answers.

The response always names the backend that produced it, so neither the model
nor the user has to guess how much of the web was actually covered.

Search result handling (URL unwrapping, entity decoding, dedupe) lives here
rather than in the orchestrator: it is specific to this tool.
"""

from __future__ import annotations

import json
import os
import re
import sys
import urllib.parse
import urllib.request
from dataclasses import dataclass
from html import unescape
from typing import Iterable, Optional

PROTOCOL_VERSION = "2024-11-05"
DEFAULT_MAX_RESULTS = 5
MAX_ALLOWED_RESULTS = 10
DEFAULT_MAX_LENGTH = 3000
MAX_ALLOWED_LENGTH = 15000
USER_AGENT = "Mozilla/5.0 (compatible; Jarvis/1.0)"
WIKIPEDIA_API = "https://en.wikipedia.org/w/api.php"
INSTANT_API = "https://api.duckduckgo.com/"
BRAVE_API = "https://api.search.brave.com/res/v1/web/search"

BACKENDS = ("auto", "wikipedia", "instant", "searx", "brave")


class SearchError(RuntimeError):
    """A search could not be completed, with a reason fit for the model."""


@dataclass
class Result:
    title: str
    url: str
    snippet: str = ""


@dataclass
class SearchResponse:
    backend: str
    results: list[Result]


# --------------------------------------------------------------------- helpers


def clamp_results(value) -> int:
    """Coerce a model-supplied result count into a usable bound."""
    try:
        count = int(value)
    except (TypeError, ValueError):
        return DEFAULT_MAX_RESULTS
    if count <= 0:
        return DEFAULT_MAX_RESULTS
    return min(count, MAX_ALLOWED_RESULTS)


def clamp_length(value) -> int:
    try:
        length = int(value)
    except (TypeError, ValueError):
        return DEFAULT_MAX_LENGTH
    if length <= 0:
        return DEFAULT_MAX_LENGTH
    return min(length, MAX_ALLOWED_LENGTH)


def strip_markup(text: str) -> str:
    """Visible text of a snippet: tags out, entities decoded, space collapsed."""
    if not text:
        return ""
    text = re.sub(r"<[^>]+>", "", text)
    text = unescape(text)
    return re.sub(r"\s+", " ", text).strip()


def unwrap_redirect(url: str) -> str:
    """Resolve a search engine's redirect wrapper to the real destination.

    Engines wrap outbound links (`//duckduckgo.com/l/?uddg=<encoded>`). Feeding
    the wrapper to the fetch tool retrieves the engine's interstitial page
    instead of the result, so the destination is extracted here.
    """
    if not url:
        return url
    if url.startswith("//"):
        url = "https:" + url
    parsed = urllib.parse.urlparse(url)
    if "duckduckgo.com" in parsed.netloc and parsed.path.startswith("/l/"):
        target = urllib.parse.parse_qs(parsed.query).get("uddg")
        if target:
            return urllib.parse.unquote(target[0])
    return url


def dedupe(results: Iterable[Result]) -> list[Result]:
    """Drop repeated destinations, keeping the first (best-ranked) mention."""
    seen: set[str] = set()
    out: list[Result] = []
    for result in results:
        url = unwrap_redirect(result.url).strip()
        if not url or url in seen:
            continue
        seen.add(url)
        out.append(Result(result.title.strip(), url, result.snippet.strip()))
    return out


def _get_json(url: str, headers: Optional[dict] = None, timeout: int = 15) -> dict:
    request = urllib.request.Request(
        url, headers={"User-Agent": USER_AGENT, "Accept": "application/json", **(headers or {})}
    )
    with urllib.request.urlopen(request, timeout=timeout) as response:
        body = response.read().decode("utf-8", errors="replace")
    return json.loads(body)


# -------------------------------------------------------------------- backends


def parse_wikipedia(payload: dict) -> list[Result]:
    """Results from a MediaWiki ``list=search`` response."""
    hits = ((payload.get("query") or {}).get("search")) or []
    results = []
    for hit in hits:
        title = strip_markup(hit.get("title", ""))
        if not title:
            continue
        path = urllib.parse.quote(title.replace(" ", "_"), safe="()/:,!'")
        results.append(
            Result(
                title=title,
                url="https://en.wikipedia.org/wiki/" + path,
                snippet=strip_markup(hit.get("snippet", "")),
            )
        )
    return results


def search_wikipedia(query: str, limit: int) -> list[Result]:
    params = urllib.parse.urlencode(
        {
            "action": "query",
            "list": "search",
            "srsearch": keywords(query),
            "srlimit": limit,
            "srprop": "snippet",
            "format": "json",
        }
    )
    return parse_wikipedia(_get_json(f"{WIKIPEDIA_API}?{params}"))


# Leading phrases that frame a question rather than describe its subject.
# MediaWiki ranks keyword matches, and measured here the framing actively
# hurts: "who won the 2026 nobel prize in physics" returns "Nobel Prize in
# Physics controversies" first, while "2026 nobel prize in physics" returns
# "2026 Nobel Prizes" first. Only the opening is stripped, so a title such as
# "The Who" is not mangled.
QUESTION_PREFIXES = (
    "who won",
    "who is",
    "who was",
    "who are",
    "what is",
    "what are",
    "what was",
    "what were",
    "when did",
    "when was",
    "when is",
    "where is",
    "where was",
    "which",
    "how many",
    "how much",
    "has",
    "have",
    "is",
    "are",
    "was",
    "were",
    "did",
    "does",
    "do",
    "can you find",
    "tell me about",
    "do you know",
)


def keywords(query: str) -> str:
    """Strip question scaffolding so a keyword index can match the subject."""
    text = query.strip()
    lower = text.lower()
    for prefix in sorted(QUESTION_PREFIXES, key=len, reverse=True):
        if lower.startswith(prefix + " "):
            text = text[len(prefix) :].lstrip()
            break
    text = re.sub(r"^(the|a|an)\s+", "", text, flags=re.I)
    return text.strip() or query.strip()


def parse_instant(payload: dict) -> list[Result]:
    """Results from a DuckDuckGo Instant Answer response."""
    results: list[Result] = []

    abstract = strip_markup(payload.get("AbstractText", ""))
    abstract_url = payload.get("AbstractURL", "")
    if abstract and abstract_url:
        results.append(
            Result(strip_markup(payload.get("Heading") or query_title(payload)), abstract_url, abstract)
        )

    for item in payload.get("Results") or []:
        if isinstance(item, dict) and item.get("FirstURL"):
            results.append(
                Result(strip_markup(item.get("Text", "")), item["FirstURL"], "")
            )

    for item in payload.get("RelatedTopics") or []:
        # Related topics nest one level: `{"Topics": [...]}`.
        children = item.get("Topics") if isinstance(item, dict) else None
        for child in children or ([item] if isinstance(item, dict) else []):
            if isinstance(child, dict) and child.get("FirstURL"):
                text = strip_markup(child.get("Text", ""))
                results.append(Result(text, child["FirstURL"], ""))
    return results


def query_title(payload: dict) -> str:
    return strip_markup(payload.get("Heading", "")) or "Result"


def search_instant(query: str, limit: int) -> list[Result]:
    params = urllib.parse.urlencode(
        {"q": query, "format": "json", "no_html": 1, "skip_disambig": 1}
    )
    return parse_instant(_get_json(f"{INSTANT_API}?{params}"))


def parse_searx(payload: dict) -> list[Result]:
    results = []
    for item in payload.get("results") or []:
        url = unwrap_redirect(item.get("url", ""))
        if not url:
            continue
        results.append(
            Result(
                title=strip_markup(item.get("title", "")),
                url=url,
                snippet=strip_markup(item.get("content", "")),
            )
        )
    return results


def search_searx(base_url: str, query: str, limit: int) -> list[Result]:
    base = base_url.rstrip("/")
    params = urllib.parse.urlencode({"q": query, "format": "json", "safesearch": 1})
    return parse_searx(_get_json(f"{base}/search?{params}"))[:limit]


def parse_brave(payload: dict) -> list[Result]:
    results = []
    for item in ((payload.get("web") or {}).get("results")) or []:
        url = item.get("url", "")
        if not url:
            continue
        results.append(
            Result(
                title=strip_markup(item.get("title", "")),
                url=url,
                snippet=strip_markup(item.get("description", "")),
            )
        )
    return results


def search_brave(key: str, query: str, limit: int) -> list[Result]:
    params = urllib.parse.urlencode({"q": query, "count": limit})
    payload = _get_json(
        f"{BRAVE_API}?{params}",
        headers={"X-Subscription-Token": key, "Accept-Encoding": "gzip"},
    )
    return parse_brave(payload)[:limit]


# ------------------------------------------------------------------ dispatch


def search(
    query: str,
    max_results: int = DEFAULT_MAX_RESULTS,
    backend: str = "auto",
    max_length: int = DEFAULT_MAX_LENGTH,
    env: Optional[dict] = None,
) -> str:
    """Run a search and render it for the model. Raises SearchError."""
    query = (query or "").strip()
    if not query:
        raise ValueError("missing query")
    limit = clamp_results(max_results)
    max_length = clamp_length(max_length)
    env = os.environ if env is None else env
    backend = (backend or "auto").lower()

    if backend not in BACKENDS:
        raise SearchError(
            f"unknown backend {backend!r}; choose one of {', '.join(BACKENDS)}"
        )

    searx_url = env.get("JARVIS_SEARX_URL", "").strip()
    brave_key = env.get("JARVIS_BRAVE_KEY", "").strip()

    if backend == "auto":
        # A configured general backend is an explicit operator choice, so it
        # wins over the keyless defaults. Otherwise the keyless chain is tried
        # in order until one produces results.
        if searx_url:
            chain = ["searx"]
        elif brave_key:
            chain = ["brave"]
        else:
            chain = ["wikipedia", "instant"]
    else:
        # An explicit backend means exactly that. Falling through to a
        # different index would answer a question the caller did not ask.
        chain = [backend]

    if backend == "searx" and not searx_url:
        raise SearchError("JARVIS_SEARX_URL is not set, so the searx backend is unavailable")
    if backend == "brave" and not brave_key:
        raise SearchError("JARVIS_BRAVE_KEY is not set, so the brave backend is unavailable")

    tried: list[str] = []
    errors: list[str] = []

    def attempt(name: str) -> list[Result]:
        tried.append(name)
        try:
            if name == "wikipedia":
                return search_wikipedia(query, limit)
            if name == "instant":
                return search_instant(query, limit)
            if name == "searx":
                return search_searx(searx_url, query, limit)
            if name == "brave":
                return search_brave(brave_key, query, limit)
            return []
        except Exception as exc:  # noqa: BLE001 - reported, not raised
            errors.append(f"{name}: {exc}")
            return []

    # `auto` falls through the keyless backends; an explicit backend does not,
    # because silently answering from a different source than the one the
    # caller named would be misleading.
    results: list[Result] = []
    used = chain[0]
    for name in chain:
        found = dedupe(attempt(name))
        if found:
            results = found
            used = name
            break

    if not results:
        detail = f" ({'; '.join(errors)})" if errors else ""
        raise SearchError(f"no results from {', '.join(tried)}{detail}")

    return render(used, query, results[:limit], max_length)


def render(backend: str, query: str, results: list[Result], max_length: int) -> str:
    """Numbered result block, bounded so one search cannot flood the prompt."""
    parts = [f"Backend: {backend}", f"Query: {query}", ""]
    for index, result in enumerate(results, start=1):
        parts.append(f"{index}. {result.title or result.url}")
        parts.append(f"   {result.url}")
        if result.snippet:
            parts.append(f"   {result.snippet}")
    out = "\n".join(parts)
    if len(out) > max_length:
        out = out[: max_length - 24].rstrip() + "\n\n[Results truncated...]"
    return out


# ------------------------------------------------------------------- protocol


def send(message: dict) -> None:
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()


def _ok(request_id, result: dict) -> None:
    send({"jsonrpc": "2.0", "id": request_id, "result": result})


def _error(request_id, code: int, message: str) -> None:
    send({"jsonrpc": "2.0", "id": request_id, "error": {"code": code, "message": message}})


def tool_definition() -> dict:
    return {
        "name": "search",
        "description": (
            "Search for web pages about a topic and return titles, URLs and "
            "snippets. Use it to find a page, then fetch that URL for details."
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "What to search for"},
                "max_results": {
                    "type": "integer",
                    "description": "Maximum results to return (default 5)",
                },
                "backend": {
                    "type": "string",
                    "enum": list(BACKENDS),
                    "description": "Search backend; leave unset to choose automatically",
                },
            },
            "required": ["query"],
        },
    }


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
                "serverInfo": {"name": "search", "version": "1.0.0"},
            },
        )
    elif method == "notifications/initialized":
        return
    elif method == "tools/list":
        _ok(request_id, {"tools": [tool_definition()]})
    elif method == "tools/call":
        params = request.get("params") or {}
        if params.get("name") != "search":
            _error(request_id, -32601, f"unknown tool: {params.get('name')}")
            return
        args = params.get("arguments") or {}
        try:
            text = search(
                args.get("query", ""),
                args.get("max_results", DEFAULT_MAX_RESULTS),
                args.get("backend", "auto"),
            )
            _ok(request_id, {"content": [{"type": "text", "text": text}]})
        except Exception as exc:  # noqa: BLE001 - reported to the caller
            _ok(
                request_id,
                {
                    "content": [{"type": "text", "text": f"Search failed: {exc}"}],
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
