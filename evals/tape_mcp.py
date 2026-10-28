#!/usr/bin/env python3
"""Record an MCP server's answers once, then replay them.

Live web results change daily. An eval score that moves because a search
engine reordered its results says nothing about Jarvis, so web cases run
against a tape: the exact `tools/list` and `tools/call` replies a real server
gave when the tape was recorded.

    tape_mcp.py --tape FILE                    replay FILE
    tape_mcp.py --tape FILE --pages PAGES      replay FILE; answer a search that
                                               is not on it from the pages
                                               recorded in PAGES (a fetch tape)
    tape_mcp.py --tape FILE --record -- CMD..  answer from FILE where it can;
                                               run CMD for the rest and add
                                               its replies to FILE

A call that is not on the tape is answered with a tool error naming the
arguments, never forwarded to the network. A case that drifts from its tape
then fails visibly instead of quietly becoming a live test.

Recording only adds. A call already on the tape is answered from it, so
recording a second model's path (a capable one searches again with better
queries) does not change the pages every other model is graded on. To
refresh an answer that went stale, delete it - or the tape - first: that
changes what a case means, and should be a decision rather than a side
effect of recording. Tool errors are not recorded (see `record_reply`).

# Frozen pages

Exact replay only grades a model that asks exactly what was recorded. A
capable one rewords its searches on every run (nemotron-3-ultra: 7 of 10
calls missed the tape on replay), so with ``--pages`` a search that is not
on the tape is answered from the case's recorded pages instead: the content
is frozen, the queries are free. Only recorded pages are offered, so every
result can be read, and a query that matches none gets no results rather
than an invented page. A fetch matches on its address alone: a page is the
same page however much of it was asked for.

Only the standard library is used, so this runs anywhere the sidecars do.
"""

import argparse
import json
import os
import re
import subprocess
import sys
import tempfile


def key(tool, args):
    """Calls match on tool name and arguments, independent of key order."""
    return json.dumps([tool, args or {}], sort_keys=True)


def load(path):
    try:
        with open(path, encoding="utf-8") as f:
            return json.load(f)
    except FileNotFoundError:
        return {"tools": None, "calls": []}


def save(path, tape):
    # Write then rename, so an interrupted recording never leaves half a tape.
    directory = os.path.dirname(os.path.abspath(path))
    os.makedirs(directory, exist_ok=True)
    fd, tmp = tempfile.mkstemp(dir=directory, suffix=".tmp")
    with os.fdopen(fd, "w", encoding="utf-8") as f:
        json.dump(tape, f, indent=2, sort_keys=True, ensure_ascii=False)
        f.write("\n")
    os.replace(tmp, path)


def send(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()


class Upstream:
    """The real server, spoken to over its own stdio."""

    def __init__(self, command):
        self.proc = subprocess.Popen(
            command,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
        )

    def forward(self, request):
        self.proc.stdin.write(json.dumps(request) + "\n")
        self.proc.stdin.flush()
        if request.get("id") is None:
            return None
        while True:
            line = self.proc.stdout.readline()
            if not line:
                raise RuntimeError("upstream MCP server exited")
            reply = json.loads(line)
            if reply.get("id") == request["id"]:
                return reply

    def close(self):
        self.proc.stdin.close()
        try:
            self.proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.proc.kill()


STOPWORDS = set(
    "a an and are as at be by did do does for from has have how i in is it its "
    "latest me most new news of on or recent the this to was what when where which "
    "who whom why will with".split()
)
RESULTS = 5
SNIPPET = 200


def terms(text):
    return [w for w in re.findall(r"[a-z0-9]+", text.lower()) if w not in STOPWORDS]


def recorded_pages(path):
    """Title, address and body of each page on a fetch tape, once per address."""
    pages, seen = [], set()
    for call in load(path)["calls"] if path else []:
        url = normalize_url((call.get("args") or {}).get("url", ""))
        text = "".join(c.get("text", "") for c in call["result"].get("content", []))
        if not url or url in seen:
            continue
        seen.add(url)
        title, _, body = text.partition("\n")
        body = body.split("\n", 1)[1] if body.startswith("URL:") else body
        title = title.removeprefix("Title:").strip().removesuffix(" - Wikipedia")
        pages.append({"url": url, "title": title or url, "body": " ".join(body.split())})
    return pages


def search_pages(pages, query):
    """Pages ranked by the query's words: a title match counts three times."""
    wanted = set(terms(query))
    ranked = []
    for order, page in enumerate(pages):
        title, body = set(terms(page["title"])), set(terms(page["body"]))
        score = sum(3 * (t in title) + (t in body) for t in wanted)
        if score:
            ranked.append((-score, order, page))
    ranked.sort(key=lambda r: (r[0], r[1]))
    return [page for _, _, page in ranked[:RESULTS]], wanted


def snippet(body, wanted):
    """The text around the first query word in the page."""
    lower = body.lower()
    hits = [m.start() for t in wanted for m in [re.search(r"\b" + re.escape(t) + r"\b", lower)] if m]
    start = max(0, min(hits) - 60) if hits else 0
    return body[start : start + SNIPPET].strip()


def frozen_search(pages, query):
    found, wanted = search_pages(pages, query)
    if not found:
        return {
            "content": [{"type": "text", "text": "Search failed: no results from recorded pages"}],
            "isError": True,
        }
    parts = ["Backend: recorded pages", f"Query: {query}", ""]
    for index, page in enumerate(found, start=1):
        parts += [f"{index}. {page['title']}", f"   {page['url']}", f"   {snippet(page['body'], wanted)}"]
    return {"content": [{"type": "text", "text": "\n".join(parts)}]}


def normalize_url(url):
    return url.strip().rstrip("/")


def replay_reply(tape, request, pages=None):
    method = request.get("method")
    request_id = request.get("id")
    if method == "initialize":
        return {
            "jsonrpc": "2.0",
            "id": request_id,
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "tape", "version": "1.0.0"},
            },
        }
    if method == "tools/list":
        if tape["tools"] is None:
            return {
                "jsonrpc": "2.0",
                "id": request_id,
                "error": {"code": -32000, "message": "tape has no tools/list; record it first"},
            }
        return {"jsonrpc": "2.0", "id": request_id, "result": tape["tools"]}
    if method == "tools/call":
        params = request.get("params") or {}
        wanted = key(params.get("name"), params.get("arguments"))
        for call in tape["calls"]:
            if key(call["tool"], call["args"]) == wanted:
                return {"jsonrpc": "2.0", "id": request_id, "result": call["result"]}
        arguments = params.get("arguments") or {}
        if arguments.get("url"):
            url = normalize_url(arguments["url"])
            for call in tape["calls"]:
                if normalize_url((call.get("args") or {}).get("url", "")) == url:
                    return {"jsonrpc": "2.0", "id": request_id, "result": call["result"]}
        if pages is not None and arguments.get("query"):
            return {
                "jsonrpc": "2.0",
                "id": request_id,
                "result": frozen_search(pages, arguments["query"]),
            }
        return {
            "jsonrpc": "2.0",
            "id": request_id,
            "result": {
                "content": [
                    {
                        "type": "text",
                        "text": "not on tape: "
                        + json.dumps(params.get("arguments") or {}, sort_keys=True),
                    }
                ],
                "isError": True,
            },
        }
    if request_id is None:
        return None
    return {
        "jsonrpc": "2.0",
        "id": request_id,
        "error": {"code": -32601, "message": "method not found"},
    }


def record_reply(tape, path, upstream, request):
    method = request.get("method")
    if method == "tools/list" and tape["tools"] is not None:
        return replay_reply(tape, request)
    if method == "tools/call":
        params = request.get("params") or {}
        wanted = key(params.get("name"), params.get("arguments"))
        if any(key(c["tool"], c["args"]) == wanted for c in tape["calls"]):
            return replay_reply(tape, request)
    reply = upstream.forward(request)
    if reply is None or "result" not in reply:
        return reply
    # A tool error is passed on but not kept. It may be the site being down
    # for a minute (Wikipedia answered 503 during one recording), and a tape
    # would replay that outage forever. On replay the call is simply not on
    # the tape, which also fails as a tool error.
    if reply["result"].get("isError"):
        return reply
    if method == "tools/list":
        tape["tools"] = reply["result"]
        save(path, tape)
    elif method == "tools/call":
        params = request.get("params") or {}
        tool, args = params.get("name"), params.get("arguments") or {}
        tape["calls"].append({"tool": tool, "args": args, "result": reply["result"]})
        save(path, tape)
    return reply


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--tape", required=True)
    parser.add_argument("--record", action="store_true")
    parser.add_argument("--pages", help="fetch tape whose pages answer searches not on --tape")
    parser.add_argument("command", nargs="*")
    options = parser.parse_args()
    if options.record and not options.command:
        parser.error("--record needs the real server command after --")

    tape = load(options.tape)
    pages = recorded_pages(options.pages) if options.pages else None
    upstream = Upstream(options.command) if options.record else None
    try:
        for line in sys.stdin:
            line = line.strip()
            if not line:
                continue
            request = json.loads(line)
            if upstream:
                reply = record_reply(tape, options.tape, upstream, request)
            else:
                reply = replay_reply(tape, request, pages)
            if reply is not None:
                send(reply)
    finally:
        if upstream:
            upstream.close()


if __name__ == "__main__":
    main()
