# /// script
# requires-python = ">=3.10"
# dependencies = ["chromadb", "pypdf", "python-docx"]
# ///
"""ChatGPT-style browser chat with your personal AI assistant (claude-gateway).

    uv run examples/web.py               # then open http://127.0.0.1:8765
    uv run examples/web.py --port 9000 --host 0.0.0.0   # reachable from other machines

- Chats are saved in examples/.chats/ and listed in the sidebar.
- Messages can carry images, documents (PDF, Word, text...) and pasted text.
- Your knowledge base (the Docs panel, stored in examples/docs/) is searched for every
  question, and related passages are sent along with it.

The page talks only to this server, which forwards the chat to the gateway, so the
gateway API key never reaches the browser. Anyone who can open the page can use your
gateway and read your chats and documents, so keep the default 127.0.0.1 unless you
trust your network.
"""

import argparse
import base64
import binascii
import json
import os
import re
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
import uuid
from http import HTTPStatus
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, unquote, urlsplit

from ask import GATEWAY_URL, key_from_dotenv
from rag import (
    ASSISTANT_SYSTEM, DOCS_DIR, EXTENSIONS, build_question, collection, extract_text, index_file, retrieve, sync_docs,
)

HERE = Path(__file__).resolve().parent
PAGE = HERE / "web" / "index.html"
CHATS_DIR = HERE / ".chats"
API_KEY = os.environ.get("GATEWAY_API_KEY") or key_from_dotenv()
MODELS = ["claude-opus-5-5", "claude-sonnet-5-5", "claude-haiku-5-5"]
MAX_UPLOAD = 25 * 1024 * 1024           # one knowledge-base document
MAX_REQUEST = 60 * 1024 * 1024          # one chat message with its attachments
MAX_IMAGE = 5 * 1024 * 1024             # Claude's per-image limit
MAX_FILE_CHARS = 150_000                # text kept from one attached document
IMAGE_TYPES = {"image/png", "image/jpeg", "image/gif", "image/webp"}
index_lock = threading.Lock()           # one ChromaDB writer at a time
chats_lock = threading.Lock()


class BadRequest(Exception):
    pass


# ---------------------------------------------------------------- knowledge base

def list_docs() -> list[dict]:
    with index_lock:
        col = collection()
        sync_docs(col)
        counts: dict[str, int] = {}
        for m in col.get(include=["metadatas"])["metadatas"]:
            counts[Path(m["source"]).name] = counts.get(Path(m["source"]).name, 0) + 1
    if not DOCS_DIR.is_dir():
        return []
    return [
        {"name": f.name, "size": f.stat().st_size, "chunks": counts.get(f.name, 0), "modified": f.stat().st_mtime}
        for f in sorted(DOCS_DIR.iterdir(), key=lambda f: f.name.lower())
        if f.is_file() and f.suffix.lower() in EXTENSIONS
    ]


def safe_doc_path(raw_name: str) -> Path | None:
    """A path inside DOCS_DIR for an uploaded file name, or None if the name isn't acceptable."""
    name = Path(unquote(raw_name).replace("\\", "/")).name.strip()
    if not name or name.startswith(".") or Path(name).suffix.lower() not in EXTENSIONS:
        return None
    return DOCS_DIR / name


# ---------------------------------------------------------------- chats

def chat_path(chat_id: str) -> Path:
    if not re.fullmatch(r"[0-9a-f]{32}", chat_id):
        raise BadRequest("bad chat id")
    return CHATS_DIR / f"{chat_id}.json"


def load_chat(chat_id: str) -> dict | None:
    p = chat_path(chat_id)
    return json.loads(p.read_text()) if p.exists() else None


def save_chat(chat: dict) -> None:
    CHATS_DIR.mkdir(exist_ok=True)
    chat["updated"] = time.time()
    p = chat_path(chat["id"])
    tmp = p.with_suffix(".tmp")
    tmp.write_text(json.dumps(chat))
    tmp.replace(p)


def list_chats() -> list[dict]:
    if not CHATS_DIR.is_dir():
        return []
    chats = []
    for p in CHATS_DIR.glob("*.json"):
        try:
            c = json.loads(p.read_text())
            chats.append({"id": c["id"], "title": c["title"], "updated": c["updated"]})
        except (json.JSONDecodeError, KeyError):
            continue
    return sorted(chats, key=lambda c: c["updated"], reverse=True)


def make_title(text: str, attachments: list[dict]) -> str:
    line = next((l.strip() for l in text.splitlines() if l.strip()), "")
    if not line and attachments:
        line = attachments[0]["name"]
    line = line or "New chat"
    return line if len(line) <= 48 else line[:47].rstrip() + "…"


def process_attachments(raw: list) -> list[dict]:
    """Validate what the browser sent; documents are converted to text here, once."""
    out = []
    for a in raw or []:
        kind, name = a.get("kind"), str(a.get("name") or "attachment")[:200]
        if kind == "image":
            if a.get("media_type") not in IMAGE_TYPES:
                raise BadRequest(f"{name}: images must be PNG, JPEG, GIF or WebP")
            try:
                size = len(base64.b64decode(a["data"], validate=True))
            except (KeyError, binascii.Error):
                raise BadRequest(f"{name}: image data is not valid base64")
            if size > MAX_IMAGE:
                raise BadRequest(f"{name}: image is larger than 5 MB")
            out.append({"kind": "image", "name": name, "media_type": a["media_type"], "data": a["data"]})
        elif kind == "file":
            suffix = Path(name).suffix.lower()
            if suffix not in EXTENSIONS:
                raise BadRequest(f"{name}: unsupported file type; allowed: {', '.join(sorted(EXTENSIONS))}")
            try:
                data = base64.b64decode(a["data"], validate=True)
            except (KeyError, binascii.Error):
                raise BadRequest(f"{name}: file data is not valid base64")
            with tempfile.TemporaryDirectory() as tmp:
                path = Path(tmp) / f"upload{suffix}"
                path.write_bytes(data)
                try:
                    text = extract_text(path)
                except Exception as e:
                    raise BadRequest(f"{name}: could not read the file ({e})")
            if not text.strip():
                raise BadRequest(f"{name}: no readable text found (a scanned PDF needs OCR first)")
            truncated = len(text) > MAX_FILE_CHARS
            out.append({"kind": "file", "name": name, "size": len(data), "text": text[:MAX_FILE_CHARS],
                        "truncated": truncated})
        elif kind == "paste":
            text = str(a.get("text") or "")
            if text.strip():
                out.append({"kind": "paste", "name": name or "Pasted text", "text": text[:MAX_FILE_CHARS]})
        else:
            raise BadRequest(f"unknown attachment kind {kind!r}")
    return out


def user_content(msg: dict, hits: list | None = None) -> list[dict]:
    """Messages API content for a stored user message: its images, then its text with
    attached documents and pasted text inlined (and RAG passages for the newest message)."""
    blocks = [
        {"type": "image", "source": {"type": "base64", "media_type": a["media_type"], "data": a["data"]}}
        for a in msg.get("attachments", []) if a["kind"] == "image"
    ]
    text = msg.get("text", "")
    for a in msg.get("attachments", []):
        if a["kind"] == "file":
            note = " (truncated)" if a.get("truncated") else ""
            text += f'\n\n<attached_file name="{a["name"]}"{note}>\n{a["text"]}\n</attached_file>'
        elif a["kind"] == "paste":
            text += f"\n\n<pasted_text>\n{a['text']}\n</pasted_text>"
    if not text.strip():
        text = "Please look at the attached image." if blocks else "(empty message)"
    if hits:
        text = build_question(text, hits)
    return blocks + [{"type": "text", "text": text}]


def public_message(m: dict) -> dict:
    """A stored message as the page needs it (document text is not sent back)."""
    out = dict(m)
    out["attachments"] = [
        {k: v for k, v in a.items() if k != "text"} | ({"chars": len(a["text"])} if "text" in a else {})
        for a in m.get("attachments", [])
    ]
    return out


# ---------------------------------------------------------------- HTTP

class Handler(BaseHTTPRequestHandler):
    server_version = "claude-gateway-web"

    def log_message(self, fmt: str, *args) -> None:  # quieter logs
        sys.stderr.write(f"{self.address_string()} {fmt % args}\n")

    def send_json(self, status: int, body) -> None:
        data = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def read_body(self, limit: int) -> bytes:
        size = int(self.headers.get("content-length", 0))
        if size > limit:
            raise BadRequest(f"request too large (max {limit // 1024 // 1024} MB)")
        return self.rfile.read(size)

    def route(self, method: str) -> None:
        url = urlsplit(self.path)
        path = url.path
        try:
            if method == "GET" and path in ("/", "/index.html"):
                return self.send_page()
            if method == "GET" and path == "/api/info":
                return self.send_json(200, {"models": MODELS, "extensions": sorted(EXTENSIONS)})
            if path == "/api/docs":
                if method == "GET":
                    return self.send_json(200, list_docs())
                if method == "POST":
                    return self.upload_doc(parse_qs(url.query).get("name", [""])[0])
            if path.startswith("/api/docs/") and method == "DELETE":
                return self.delete_doc(path[len("/api/docs/"):])
            if path == "/api/chats" and method == "GET":
                return self.send_json(200, list_chats())
            if path.startswith("/api/chats/"):
                return self.chat_item(method, path[len("/api/chats/"):])
            if path == "/api/chat" and method == "POST":
                return self.chat()
            self.send_json(404, {"error": "not found"})
        except BadRequest as e:
            self.send_json(400, {"error": str(e)})

    def do_GET(self) -> None:
        self.route("GET")

    def do_POST(self) -> None:
        self.route("POST")

    def do_PATCH(self) -> None:
        self.route("PATCH")

    def do_DELETE(self) -> None:
        self.route("DELETE")

    def send_page(self) -> None:
        data = PAGE.read_bytes()
        self.send_response(HTTPStatus.OK)
        self.send_header("content-type", "text/html; charset=utf-8")
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    # ---- knowledge base documents

    def upload_doc(self, raw_name: str) -> None:
        target = safe_doc_path(raw_name)
        if not target:
            raise BadRequest(f"unsupported file; allowed: {', '.join(sorted(EXTENSIONS))}")
        data = self.read_body(MAX_UPLOAD)
        DOCS_DIR.mkdir(parents=True, exist_ok=True)
        target.write_bytes(data)
        with index_lock:
            chunks = index_file(collection(), target)
        warning = None if chunks else "no readable text found (a scanned PDF needs OCR first)"
        self.send_json(200, {"name": target.name, "chunks": chunks, "warning": warning, "docs": list_docs()})

    def delete_doc(self, raw_name: str) -> None:
        target = safe_doc_path(raw_name)
        if not target or not target.is_file():
            return self.send_json(404, {"error": "no such document"})
        target.unlink()
        self.send_json(200, {"deleted": target.name, "docs": list_docs()})

    # ---- saved chats

    def chat_item(self, method: str, chat_id: str) -> None:
        with chats_lock:
            chat = load_chat(chat_id)
            if not chat:
                return self.send_json(404, {"error": "no such chat"})
            if method == "GET":
                return self.send_json(200, chat | {"messages": [public_message(m) for m in chat["messages"]]})
            if method == "PATCH":
                title = str(json.loads(self.read_body(10_000) or b"{}").get("title", "")).strip()
                if not title:
                    raise BadRequest("title must not be empty")
                chat["title"] = title[:100]
                save_chat(chat)
                return self.send_json(200, {"id": chat_id, "title": chat["title"]})
            if method == "DELETE":
                chat_path(chat_id).unlink()
                return self.send_json(200, {"deleted": chat_id})
        self.send_json(405, {"error": "method not allowed"})

    def chat(self) -> None:
        try:
            body = json.loads(self.read_body(MAX_REQUEST))
        except json.JSONDecodeError:
            raise BadRequest("body must be JSON")
        model = body.get("model") if body.get("model") in MODELS else MODELS[0]

        with chats_lock:
            chat = load_chat(body["chat_id"]) if body.get("chat_id") else None
            if body.get("regenerate"):
                if not chat or not chat["messages"]:
                    raise BadRequest("nothing to regenerate")
                if chat["messages"][-1]["role"] == "assistant":
                    chat["messages"].pop()
            else:
                text = str(body.get("text") or "")
                attachments = process_attachments(body.get("attachments"))
                if not text.strip() and not attachments:
                    raise BadRequest("empty message")
                if not chat:
                    chat = {"id": uuid.uuid4().hex, "title": make_title(text, attachments),
                            "created": time.time(), "messages": []}
                chat["messages"].append({"role": "user", "text": text, "attachments": attachments, "ts": time.time()})
            save_chat(chat)

        # Knowledge-base passages for the newest message only; earlier turns stay as they were.
        last = chat["messages"][-1]
        hits = []
        if body.get("use_docs", True) and last.get("text", "").strip():
            with index_lock:
                hits = retrieve(last["text"], int(body.get("k", 4)))
        api_messages = []
        for m in chat["messages"]:
            if m["role"] == "user":
                api_messages.append({"role": "user", "content": user_content(m, hits if m is last else None)})
            elif m.get("text"):
                api_messages.append({"role": "assistant", "content": m["text"]})
        sources = [{"name": Path(src).name, "distance": round(dist, 3), "text": doc[:400]} for doc, src, dist in hits]

        payload = {"model": model, "max_tokens": 16000, "stream": True, "system": ASSISTANT_SYSTEM,
                   "messages": api_messages}
        req = urllib.request.Request(
            f"{GATEWAY_URL}/v1/messages",
            data=json.dumps(payload).encode(),
            headers={"content-type": "application/json", "x-api-key": API_KEY},
            method="POST",
        )
        try:
            upstream = urllib.request.urlopen(req, timeout=900)
        except urllib.error.HTTPError as e:
            try:
                message = json.load(e)["error"]["message"]
            except Exception:
                message = str(e.reason)
            return self.send_json(502, {"error": f"gateway error {e.code}: {message}", "chat_id": chat["id"]})
        except urllib.error.URLError as e:
            return self.send_json(502, {"error": f"cannot reach the gateway at {GATEWAY_URL}: {e.reason}",
                                        "chat_id": chat["id"]})

        # Relay the gateway's events, preceded by the chat id and the passages used,
        # and keep the reply so it can be saved with the chat.
        self.send_response(HTTPStatus.OK)
        self.send_header("content-type", "text/event-stream")
        self.send_header("cache-control", "no-cache")
        self.end_headers()
        reply, stopped, usage = [], False, {}
        started = time.time()
        try:
            head = f"event: chat\ndata: {json.dumps({'id': chat['id'], 'title': chat['title']})}\n\n"
            head += f"event: sources\ndata: {json.dumps(sources)}\n\n"
            self.wfile.write(head.encode())
            self.wfile.flush()
            with upstream:
                for line in upstream:
                    if line.startswith(b"data: "):
                        ev = json.loads(line[6:])
                        if ev.get("type") == "content_block_delta" and ev["delta"].get("type") == "text_delta":
                            reply.append(ev["delta"]["text"])
                        elif ev.get("type") == "message_delta":
                            usage = ev.get("usage", {})
                    self.wfile.write(line)
                    if line == b"\n":
                        self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            stopped = True  # browser pressed Stop; closing upstream stops the gateway's CLI run
        finally:
            text = "".join(reply)
            if text or stopped:
                with chats_lock:
                    current = load_chat(chat["id"]) or chat
                    current["messages"].append({
                        "role": "assistant", "text": text, "model": model, "sources": sources, "ts": time.time(),
                        "stopped": stopped, "output_tokens": usage.get("output_tokens"),
                        "seconds": round(time.time() - started, 1),
                    })
                    save_chat(current)


def main() -> None:
    parser = argparse.ArgumentParser(description="Browser chat for claude-gateway")
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=8765)
    args = parser.parse_args()
    if not API_KEY:
        sys.exit("No API key: set GATEWAY_API_KEY or GATEWAY_API_KEYS in .env")
    try:
        server = ThreadingHTTPServer((args.host, args.port), Handler)
    except OSError as e:
        if e.errno == 98:
            sys.exit(
                f"Port {args.port} is already in use. The chat may already be running: open "
                f"http://{args.host}:{args.port}, or use another port: uv run examples/web.py --port {args.port + 1}"
            )
        raise
    print(f"Chat UI on http://{args.host}:{args.port}  →  gateway {GATEWAY_URL}  (Ctrl-C to stop)")
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print()


if __name__ == "__main__":
    main()
