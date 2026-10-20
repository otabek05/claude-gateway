"""Ask the local claude-gateway a question. Plain Python, no extra packages.

    python3 examples/ask.py "What is the capital of Uzbekistan?"   # one question
    python3 examples/ask.py                                         # interactive chat

Sends POST {GATEWAY_URL}/v1/messages (default http://127.0.0.1:8443) with the key
from GATEWAY_API_KEY, or the first key in ../.env, and streams the reply.

In a terminal the prompts are rainbow-colored, an animated indicator shows "thinking"
while waiting and the live speed while the reply streams, and a speed summary follows
each reply. All of that is off when output is piped or NO_COLOR is set.
"""

import argparse
import colorsys
import itertools
import json
import os
import re
import shutil
import sys
import threading
import time
import urllib.error
import urllib.request
from pathlib import Path

GATEWAY_URL = os.environ.get("GATEWAY_URL", "http://127.0.0.1:8443")
COLOR = sys.stdout.isatty() and "NO_COLOR" not in os.environ

RESET, BOLD, DIM = "\033[0m", "\033[1m", "\033[2m"
_out_lock = threading.Lock()


def _rgb(r: float, g: float, b: float) -> str:
    return f"\033[38;2;{int(r * 255)};{int(g * 255)};{int(b * 255)}m"


def style(text: str, *codes: str) -> str:
    return "".join(codes) + text + RESET if COLOR else text


def rainbow(text: str) -> str:
    """Bold text with each letter in its own color; the palette shifts each call."""
    if not COLOR:
        return text
    base = time.time() * 0.1
    letters = [
        BOLD + _rgb(*colorsys.hsv_to_rgb((base + i / max(len(text), 1)) % 1.0, 0.65, 1.0)) + ch
        for i, ch in enumerate(text)
    ]
    return "".join(letters) + RESET


def you_prompt() -> str:
    return rainbow("you> ")


def claude_prompt() -> str:
    return rainbow("claude> ")


_ANSI = re.compile(r"\x1b(\[[0-9;?]*[A-Za-z]|[78])")
_col = 0  # visible cursor column, so the indicator knows whether it fits on the line


def write(text: str) -> None:
    global _col
    with _out_lock:
        sys.stdout.write(text)
        sys.stdout.flush()
        visible = _ANSI.sub("", text)
        _col = len(visible.rsplit("\n", 1)[-1]) if "\n" in visible else _col + len(visible)


class Indicator:
    """Animated spinner drawn just after the cursor for the whole reply:
    "thinking" with a timer while waiting, live speed while text streams in.
    It is erased before each new piece of text, so the reply itself stays clean."""

    FRAMES = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏"

    def __init__(self) -> None:
        self.started = time.monotonic()
        self.first_text: float | None = None
        self.chars = 0
        self._stop = threading.Event()
        self._thread = threading.Thread(target=self._run, daemon=True)
        if COLOR:
            self._thread.start()

    def text(self, chunk: str) -> None:
        if self.first_text is None:
            self.first_text = time.monotonic()
        self.chars += len(chunk)
        write("\033[K" + chunk)  # erase the indicator, then print the new text

    def stop(self) -> None:
        if self._thread.is_alive():
            self._stop.set()
            self._thread.join()
            write("\033[K")

    def live_speed(self) -> float:
        """Approximate tokens/s of visible text (about 4 characters per token)."""
        if self.first_text is None:
            return 0.0
        return self.chars / 4 / max(time.monotonic() - self.first_text, 0.1)

    def _run(self) -> None:
        for i in itertools.count():
            if self._stop.wait(0.08):
                return
            frame = _rgb(*colorsys.hsv_to_rgb((i * 0.03) % 1.0, 0.7, 1.0)) + self.FRAMES[i % len(self.FRAMES)] + RESET
            if self.first_text is None:
                label = f"{frame} {DIM}thinking{'.' * (i // 3 % 4):<3} {time.monotonic() - self.started:4.1f}s{RESET}"
                width = 20
            else:
                label = f"{frame} {DIM}~{self.live_speed():.0f} tok/s{RESET}"
                width = 14
            # Draw only if it fits on the current line; save cursor, draw, restore.
            with _out_lock:
                fits = _col + width < shutil.get_terminal_size().columns
            if fits:
                with _out_lock:
                    sys.stdout.write(f"\0337 {label}\033[K\0338")
                    sys.stdout.flush()

    def summary(self, output_tokens: int | None) -> str:
        total = time.monotonic() - self.started
        parts = []
        if output_tokens:
            parts.append(f"{output_tokens} tokens in {total:.1f}s ({output_tokens / total:.0f} tok/s)")
        else:
            parts.append(f"{total:.1f}s")
        if self.first_text is not None:
            parts.append(f"first text after {self.first_text - self.started:.1f}s")
        return "⚡ " + " · ".join(parts)


def key_from_dotenv() -> str | None:
    env = Path(__file__).resolve().parent.parent / ".env"
    if not env.exists():
        return None
    for line in env.read_text().splitlines():
        if line.startswith("GATEWAY_API_KEYS="):
            return line.split("=", 1)[1].split(",")[0].strip() or None
    return None


def ask(api_key: str, model: str, messages: list[dict], system: str | None = None) -> str:
    """Send the conversation to the gateway, print the streamed reply, return its text."""
    payload = {"model": model, "max_tokens": 16000, "stream": True, "messages": messages}
    if system:
        payload["system"] = system
    body = json.dumps(payload)
    req = urllib.request.Request(
        f"{GATEWAY_URL}/v1/messages",
        data=body.encode(),
        headers={"content-type": "application/json", "x-api-key": api_key},
        method="POST",
    )
    reply: list[str] = []
    output_tokens = None
    indicator = Indicator()
    try:
        with urllib.request.urlopen(req, timeout=900) as resp:
            # Server-sent events: we only need the "data: {...}" lines.
            for raw in resp:
                line = raw.decode().strip()
                if not line.startswith("data: "):
                    continue
                event = json.loads(line[len("data: "):])
                kind = event["type"]
                if kind == "content_block_delta" and event["delta"]["type"] == "text_delta":
                    indicator.text(event["delta"]["text"])
                    reply.append(event["delta"]["text"])
                elif kind == "message_delta":
                    output_tokens = event.get("usage", {}).get("output_tokens")
                elif kind == "error":
                    raise RuntimeError(event["error"]["message"])
    finally:
        indicator.stop()
    write("\n")
    if COLOR:
        write(style(indicator.summary(output_tokens), DIM) + "\n")
    return "".join(reply)


def main() -> None:
    parser = argparse.ArgumentParser(description="Ask the local claude-gateway a question.")
    parser.add_argument("question", nargs="*", help="question to ask; omit for interactive chat")
    parser.add_argument("--model", default="claude-opus-5-5")
    args = parser.parse_args()

    api_key = os.environ.get("GATEWAY_API_KEY") or key_from_dotenv()
    if not api_key:
        sys.exit("No API key: set GATEWAY_API_KEY or GATEWAY_API_KEYS in .env")

    try:
        if args.question:
            ask(api_key, args.model, [{"role": "user", "content": " ".join(args.question)}])
            return

        print(style(f"Chatting with {args.model} via {GATEWAY_URL} (Ctrl-D to quit)", DIM))
        history: list[dict] = []
        while True:
            try:
                question = input("\n" + you_prompt()).strip()
            except (EOFError, KeyboardInterrupt):
                print()
                return
            if not question:
                continue
            write(claude_prompt())
            try:
                answer = ask(api_key, args.model, history + [{"role": "user", "content": question}])
            except KeyboardInterrupt:
                write(style("\n(cancelled)\n", DIM))
                continue
            history += [{"role": "user", "content": question}, {"role": "assistant", "content": answer}]
    except urllib.error.HTTPError as e:
        try:
            message = json.load(e)["error"]["message"]
        except Exception:
            message = e.reason
        sys.exit(f"Gateway error {e.code}: {message}")
    except urllib.error.URLError as e:
        sys.exit(f"Cannot reach the gateway at {GATEWAY_URL} ({e.reason}). Is it running? (make status)")
    except RuntimeError as e:
        sys.exit(f"Gateway error: {e}")


if __name__ == "__main__":
    main()
