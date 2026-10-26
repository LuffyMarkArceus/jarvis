#!/usr/bin/env python3
"""Record an MCP server's answers once, then replay them.

Live web results change daily. An eval score that moves because a search
engine reordered its results says nothing about Jarvis, so web cases run
against a tape: the exact `tools/list` and `tools/call` replies a real server
gave when the tape was recorded.

    tape_mcp.py --tape FILE                    replay FILE
    tape_mcp.py --tape FILE --record -- CMD..  run CMD, answer from it, and
                                               add every reply to FILE

A call that is not on the tape is answered with a tool error naming the
arguments, never forwarded to the network. A case that drifts from its tape
then fails visibly instead of quietly becoming a live test.

Only the standard library is used, so this runs anywhere the sidecars do.
"""

import argparse
import json
import os
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


def replay_reply(tape, request):
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
    reply = upstream.forward(request)
    if reply is None or "result" not in reply:
        return reply
    method = request.get("method")
    if method == "tools/list":
        tape["tools"] = reply["result"]
        save(path, tape)
    elif method == "tools/call":
        params = request.get("params") or {}
        tool, args = params.get("name"), params.get("arguments") or {}
        wanted = key(tool, args)
        # Re-recording replaces a stale answer rather than keeping both.
        tape["calls"] = [c for c in tape["calls"] if key(c["tool"], c["args"]) != wanted]
        tape["calls"].append({"tool": tool, "args": args, "result": reply["result"]})
        save(path, tape)
    return reply


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--tape", required=True)
    parser.add_argument("--record", action="store_true")
    parser.add_argument("command", nargs="*")
    options = parser.parse_args()
    if options.record and not options.command:
        parser.error("--record needs the real server command after --")

    tape = load(options.tape)
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
                reply = replay_reply(tape, request)
            if reply is not None:
                send(reply)
    finally:
        if upstream:
            upstream.close()


if __name__ == "__main__":
    main()
