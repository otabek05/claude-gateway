# claude-gateway: how to use it

claude-gateway is an API server compatible with the Claude API. Apps send requests with a
**token**, and the gateway answers them with the Claude CLI that's logged in on this PC.
Requests without a valid token are rejected with `401 Unauthorized`.

```
your app ──(token)──▶ nginx (HTTPS) ──▶ claude-gateway :8443 ──▶ claude CLI
```

---

## 1. Start and stop the server

The gateway runs as a background service. It starts at boot and keeps running after you log out.
Run these in `~/personal_projects/claude-gateway`:

| Command | What it does |
|---|---|
| `make status` | Is it running? |
| `make logs` | Live log (Ctrl-C to exit) |
| `make restart` | Rebuild and restart, after code or `.env` changes |
| `make stop` / `make start` | Stop or start it |

Quick check: `curl http://127.0.0.1:8443/health` should print `ok`.

---

## 2. Generate a token in the admin panel

1. Open **http://127.0.0.1:8443/admin** in a browser on this PC.
2. Sign in with the **admin key**. To print it:
   ```bash
   grep GATEWAY_ADMIN_KEY ~/personal_projects/claude-gateway/.env
   ```
3. Under **Create a token**, enter a name for the app that will use it (for example
   `telegram-bot`) and click **Generate token**.
4. Click **Copy token** and store it in your app's settings. **It's shown only once.** The
   gateway keeps only a hash, so a lost token can't be recovered; create a new one instead.

The table lists every token with its status, request count and when it was last used:

- **Disable**: the token stops working immediately (401). **Enable** turns it back on.
- **Delete**: removes the token for good.

Changes take effect at once, with no restart. You stay signed in until you close the tab
or click **Log out**.

From the terminal instead:

```bash
make key-create NAME=telegram-bot
make key-list
make key-disable ID=key_...      # key-enable / key-delete work the same way
```

---

## 3. Use the token in your app

Send the token in **one** of these headers:

```
x-api-key: sk-gw-...
Authorization: Bearer sk-gw-...
```

The base URL is `http://127.0.0.1:8443` on this PC, `http://192.168.0.131:8443` on your
local network, or your nginx HTTPS address.

### curl

```bash
curl http://127.0.0.1:8443/v1/messages \
  -H "x-api-key: $TOKEN" -H "content-type: application/json" \
  -d '{"model":"claude-opus-5-5","max_tokens":1024,
       "messages":[{"role":"user","content":"Hello!"}]}'
```

Add `"stream": true` to get the reply as server-sent events while it's generated.

### Python (official Anthropic SDK)

```python
import anthropic

client = anthropic.Anthropic(api_key="sk-gw-...", base_url="http://127.0.0.1:8443")
msg = client.messages.create(
    model="claude-opus-5-5",
    max_tokens=1024,
    messages=[{"role": "user", "content": "Hello!"}],
)
print(msg.content[0].text)
```

### Python (no packages)

```python
import json, urllib.request

req = urllib.request.Request(
    "http://127.0.0.1:8443/v1/messages",
    data=json.dumps({"model": "claude-opus-5-5", "max_tokens": 1024,
                     "messages": [{"role": "user", "content": "Hello!"}]}).encode(),
    headers={"content-type": "application/json", "x-api-key": "sk-gw-..."},
)
print(json.load(urllib.request.urlopen(req))["content"][0]["text"])
```

### JavaScript (Node 18+ or a backend; never put a token in browser code)

```js
const resp = await fetch("http://127.0.0.1:8443/v1/messages", {
  method: "POST",
  headers: { "content-type": "application/json", "x-api-key": process.env.GATEWAY_TOKEN },
  body: JSON.stringify({
    model: "claude-opus-5-5", max_tokens: 1024,
    messages: [{ role: "user", content: "Hello!" }],
  }),
});
console.log((await resp.json()).content[0].text);
```

### Sending images

Put image blocks before the text in a user message: PNG, JPEG, GIF or WebP, base64-encoded,
up to about 5 MB each. Images from earlier turns stay available, so follow-up questions about
them work.

```python
import base64

with open("photo.jpg", "rb") as f:
    data = base64.b64encode(f.read()).decode()

msg = client.messages.create(
    model="claude-opus-5-5",
    max_tokens=1024,
    messages=[{"role": "user", "content": [
        {"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": data}},
        {"type": "text", "text": "What's in this photo?"},
    ]}],
)
```

For documents (PDF, Word and others), extract the text yourself and send it as text.
The browser chat in `examples/web.py` does this for you.

### Conversations

The API doesn't remember anything between requests: send the whole conversation each time,
alternating `user` and `assistant` messages and ending with `user`:

```python
messages = [
    {"role": "user", "content": "My name is Otabek."},
    {"role": "assistant", "content": "Nice to meet you, Otabek!"},
    {"role": "user", "content": "What's my name?"},
]
```

Models: `claude-opus-5-5` (default), `claude-sonnet-5-5`, `claude-haiku-5-5` (fastest),
`claude-fable-5-1`. The full list is at `GET /v1/models`, which also needs a token.

---

## 4. Responses when something is wrong

| Status | `error.type` | Meaning | Fix |
|---|---|---|---|
| 401 | `authentication_error` | `missing API key` | Send the token in `x-api-key` or `Authorization: Bearer` |
| 401 | `authentication_error` | `invalid or disabled API key` | Check that it's copied correctly and enabled in the admin panel |
| 400 | `invalid_request_error` | Bad request body | The message says which field |
| 429 | `rate_limit_error` | Too many requests at once, or the Claude account limit was reached | Retry later. `GATEWAY_MAX_CONCURRENT` in `.env` sets how many run at once |
| 502 / 504 | `api_error` / `timeout_error` | The Claude CLI failed or took too long | `make logs` shows the reason |

Errors have the same shape as the Claude API:
`{"type":"error","error":{"type":"authentication_error","message":"..."}}`

---

## 5. Public URL behind nginx

The public base URL is `https://proxyx.online/claude/api`. `GATEWAY_BASE_PATH=/claude/api` in `.env`
makes the gateway answer under that prefix as well as at `/`, so nginx can forward with or without
stripping it.

The nginx blocks are in [config/nginx.conf](config/nginx.conf). Paste them inside the HTTPS
`server { }` of proxyx.online (`/etc/nginx/conf.d/proxyx.conf` on the nginx host), above
`location / { ... }`, then run `sudo nginx -t && sudo systemctl reload nginx`. Everything starting
with `/claude` goes to the gateway, and the admin panel is limited to `192.168.0.x`.

Share [API.md](API.md) with API users.

## 6. Security checklist

- **Keep the admin panel private.** If nginx exposes the gateway to the internet, block the
  admin pages there so only this PC or your LAN can open them:
  ```nginx
  location /admin { allow 192.168.0.0/24; deny all; }   # or just: deny all;
  ```
  `make key-*` and `http://127.0.0.1:8443/admin` keep working, because they don't go through nginx.
- **Use one token per app.** If one leaks, disable just that token and the others keep working.
- **Never put a token in browser or mobile app code.** Call the gateway from a backend.
- **To change the admin key,** edit `GATEWAY_ADMIN_KEY` in `.env` (24+ characters), then
  run `make restart`.
- Tokens are stored as SHA-256 hashes in `~/.local/share/claude-gateway/keys.json`, which
  only your user can read.
- Every request uses your Claude account's usage limits, so give tokens only to your own apps.

---

## 7. Troubleshooting

| Problem | Check |
|---|---|
| `Connection refused` | `make status`; start it with `make start` |
| The admin panel says "admin API is disabled" | Set `GATEWAY_ADMIN_KEY` in `.env`, then run `make restart` |
| The admin key is rejected | Copy it again from `.env`, without spaces or quotes |
| A token suddenly gets 401 | It was disabled or deleted; check the admin panel |
| Answers fail with an authentication error from Claude | Run `claude` once in a terminal to check the CLI login, and keep `CLAUDE_CODE_OAUTH_TOKEN` empty in `.env` |
