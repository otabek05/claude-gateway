# Claude API: quick guide

**Base URL:** `https://proxyx.online/claude/api`
**Auth:** send your token in the `x-api-key` header (or `Authorization: Bearer <token>`).
Ask the admin for a token. Keep it secret and use it only from server-side code.

It works like the official Claude Messages API, so the Anthropic SDKs work if you set `base_url`.

## Send a message

`POST /v1/messages`

```bash
curl https://proxyx.online/claude/api/v1/messages \
  -H "x-api-key: $TOKEN" \
  -H "content-type: application/json" \
  -d '{
    "model": "claude-opus-5-5",
    "max_tokens": 1024,
    "messages": [{"role": "user", "content": "Hello!"}]
  }'
```

| Field | Required | Notes |
|---|---|---|
| `model` | no | `claude-opus-5-5` (default), `claude-sonnet-5-5`, `claude-haiku-5-5` (fastest), `claude-fable-5-1` |
| `messages` | yes | The whole conversation, alternating `user` / `assistant` and ending with `user`. Nothing is remembered between requests |
| `max_tokens` | no | Maximum length of the reply |
| `system` | no | Instructions for the assistant |
| `stream` | no | `true` streams the reply as it's generated (see below) |

Message content can be a string or a list of blocks: `{"type": "text", "text": "..."}` and
`{"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "<base64>"}}`
(PNG, JPEG, GIF or WebP, up to 5 MB).

## Send a file with a message

Put the file and your question in the **same** user message, as a list of blocks: the file
first, then the text.

### Images (PNG, JPEG, GIF, WebP)

Send the image base64-encoded in an `image` block:

```bash
IMG=$(base64 -w0 photo.jpg)
curl https://proxyx.online/claude/api/v1/messages \
  -H "x-api-key: $TOKEN" -H "content-type: application/json" \
  -d '{
    "model": "claude-opus-5-5",
    "max_tokens": 1024,
    "messages": [{"role": "user", "content": [
      {"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": "'"$IMG"'"}},
      {"type": "text", "text": "What is in this photo?"}
    ]}]
  }'
```

```python
import base64

with open("photo.jpg", "rb") as f:
    data = base64.b64encode(f.read()).decode()

msg = client.messages.create(
    model="claude-opus-5-5",
    max_tokens=1024,
    messages=[{"role": "user", "content": [
        {"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": data}},
        {"type": "text", "text": "What is in this photo?"},
    ]}],
)
print(msg.content[0].text)
```

```js
import { readFile } from "node:fs/promises";

const data = (await readFile("photo.jpg")).toString("base64");
const resp = await fetch("https://proxyx.online/claude/api/v1/messages", {
  method: "POST",
  headers: { "content-type": "application/json", "x-api-key": process.env.CLAUDE_TOKEN },
  body: JSON.stringify({
    model: "claude-opus-5-5", max_tokens: 1024,
    messages: [{ role: "user", content: [
      { type: "image", source: { type: "base64", media_type: "image/jpeg", data } },
      { type: "text", text: "What is in this photo?" },
    ] }],
  }),
});
console.log((await resp.json()).content[0].text);
```

- Set `media_type` to match the file: `image/png`, `image/jpeg`, `image/gif` or `image/webp`.
- You can attach several images to one message (up to 20). Add one `image` block per image.
- An image URL also works: `{"type": "image", "source": {"type": "url", "url": "https://..."}}`.

### Documents (PDF, Word, text, code, CSV …)

Only `text` and `image` blocks are supported. For a document, read its text in your app and
send it as a `text` block before your question:

```python
from pypdf import PdfReader   # pip install pypdf

doc = "\n".join(page.extract_text() or "" for page in PdfReader("report.pdf").pages)

msg = client.messages.create(
    model="claude-opus-5-5",
    max_tokens=2048,
    messages=[{"role": "user", "content": [
        {"type": "text", "text": f"<document name=\"report.pdf\">\n{doc}\n</document>"},
        {"type": "text", "text": "Summarize this report in 5 bullet points."},
    ]}],
)
```

For plain text files (`.txt`, `.md`, `.csv`, `.json`, code), read the file and send its
contents the same way. For a scanned PDF with no text layer, convert its pages to images and
send them as `image` blocks.

### Follow-up questions

To ask more about the file later, send the earlier messages again, including the message with
the file. Nothing is remembered between requests.

## Response

```json
{
  "id": "msg_011Cfx4wZCgHpntpT4yzLB1V",
  "type": "message",
  "role": "assistant",
  "model": "claude-haiku-5-5",
  "content": [{"type": "text", "text": "Hello! How can I help?"}],
  "stop_reason": "end_turn",
  "usage": {
    "input_tokens": 2,
    "cache_creation_input_tokens": 1429,
    "cache_read_input_tokens": 4215,
    "output_tokens": 70
  }
}
```

- **The reply text:** join every `content` block whose `type` is `"text"` (usually there's one: `content[0].text`).
- **`stop_reason`:** `end_turn` means finished; `max_tokens` means it was cut off, so raise `max_tokens`.

### Token counts

| Field | Meaning |
|---|---|
| `usage.output_tokens` | Tokens in the reply |
| `usage.input_tokens` + `usage.cache_creation_input_tokens` + `usage.cache_read_input_tokens` | **Total input tokens** |

The input is split three ways because repeated prompt parts are cached. Add all three to get
the input size. In the example above, the total input is 2 + 1429 + 4215 = 5646 tokens.

## Streaming (`"stream": true`)

The response is server-sent events:

```
event: message_start        data: {"type":"message_start","message":{..., "usage":{input token counts}}}
event: content_block_start  data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}
event: content_block_delta  data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}}
event: content_block_delta  data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lo!"}}
event: content_block_stop   data: {"type":"content_block_stop","index":0}
event: message_delta        data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":75}}
event: message_stop         data: {"type":"message_stop"}
```

Append every `text_delta` to build the reply. Input token counts are in `message_start`
(`message.usage`), and the output count is in the final `message_delta` (`usage.output_tokens`).
An `event: error` can arrive mid-stream if something fails.

## Python

```python
import anthropic

client = anthropic.Anthropic(api_key=TOKEN, base_url="https://proxyx.online/claude/api")

msg = client.messages.create(
    model="claude-opus-5-5",
    max_tokens=1024,
    messages=[{"role": "user", "content": "Hello!"}],
)
print(msg.content[0].text)
u = msg.usage
print("input:", u.input_tokens + u.cache_creation_input_tokens + u.cache_read_input_tokens,
      "output:", u.output_tokens)

# Streaming
with client.messages.stream(model="claude-opus-5-5", max_tokens=1024,
                            messages=[{"role": "user", "content": "Tell me a joke"}]) as s:
    for text in s.text_stream:
        print(text, end="", flush=True)
```

## JavaScript (Node 18+)

```js
const resp = await fetch("https://proxyx.online/claude/api/v1/messages", {
  method: "POST",
  headers: { "content-type": "application/json", "x-api-key": process.env.CLAUDE_TOKEN },
  body: JSON.stringify({
    model: "claude-opus-5-5", max_tokens: 1024,
    messages: [{ role: "user", content: "Hello!" }],
  }),
});
const data = await resp.json();
if (!resp.ok) throw new Error(data.error.message);
console.log(data.content[0].text, data.usage.output_tokens);
```

## Errors

Errors return JSON: `{"type": "error", "error": {"type": "...", "message": "..."}}`

| Status | Meaning |
|---|---|
| 400 | The request is invalid; `message` names the field |
| 401 | The token is missing, wrong or disabled |
| 429 | Busy or the usage limit was reached; wait and retry |
| 502 / 504 | Server error or timeout; retry later |

## Other endpoints

- `GET /v1/models`: available models (needs a token)
- `GET /health`: returns `ok` (no token needed)
