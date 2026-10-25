# Python Sidecars

MCP servers and AI utilities that run as separate processes/services.

Implemented:

- `fetch`: read a URL and return clean text (HTML chrome removed)
- `search`: find pages by topic and return titles, URLs and snippets

Planned:

- embeddings: text embeddings via Ollama/local models
- router: model routing logic
- stt: speech-to-text (Whisper)
- tts: text-to-speech (Piper)

Each sidecar is a stdio JSON-RPC server started by the daemon, with its own
tests (`test_server.py`) that run offline.
