# claude-gateway

A plain-HTTP server (meant to sit behind nginx for TLS) that speaks the Anthropic Messages API and answers each request by
running your local `claude` CLI. Because it is wire-compatible, the official Anthropic
SDKs work against it if you change `base_url`.

```
client (SDK / curl) ──HTTPS──▶ nginx ──HTTP──▶ claude-gateway (Rust) ──spawns──▶ claude -p --output-format stream-json
```

**New here?** API users: [API.md](API.md) (short guide). Admins: [INSTRUCTIONS.md](INSTRUCTIONS.md) (server, admin panel, tokens).

## Endpoints

| Method | Path | Notes |
|---|---|---|
| POST | `/v1/messages` | `stream: true` → SSE (`message_start`, `content_block_*`, `message_delta`, `message_stop`), otherwise a full `Message` JSON |
| GET | `/v1/models` | List from `GATEWAY_MODELS` |
| GET | `/health` | No auth |
| GET | `/admin` | Admin panel page (sign in with `GATEWAY_ADMIN_KEY`) |

Auth: `x-api-key: <key>` or `Authorization: Bearer <key>`. Valid keys are the ones created
through the admin API below, plus any static keys in `GATEWAY_API_KEYS`.
Errors use the API's shape: `{"type":"error","error":{"type":"invalid_request_error","message":"..."}}`.

## Run

```bash
cp .env.example .env    # set GATEWAY_API_KEYS
make run                # foreground, Ctrl-C to stop
make install            # or: install and start as a systemd user service
```

`make help` lists everything: `start`, `stop`, `restart` (rebuilds first), `status`, `logs`, `uninstall`.
To keep the service running after you log out, run `loginctl enable-linger $USER` once.
Your user must already be logged in to `claude`, because the gateway uses that login.

## API keys

Client keys are managed over HTTP with the admin key (`GATEWAY_ADMIN_KEY` in `.env`). Changes
take effect immediately, with no restart. Keys are stored as SHA-256 hashes in
`~/.local/share/claude-gateway/keys.json` (mode 600), so a key is shown only once, when it's created.

| Method | Path | Body |
|---|---|---|
| POST | `/admin/keys` | `{"name": "my-app"}` → returns `key` once |
| GET | `/admin/keys` | list with `status`, `request_count`, `last_used_at` |
| GET | `/admin/keys/{id}` | |
| PATCH | `/admin/keys/{id}` | `{"name": "...", "status": "active" \| "disabled"}` |
| DELETE | `/admin/keys/{id}` | |

Shortcuts:

```bash
make key-create NAME=my-app     # prints the new key; save it
make key-list
make key-disable ID=key_...     # or key-enable / key-delete
```

```bash
curl -X POST http://127.0.0.1:8443/admin/keys -H "x-api-key: $ADMIN_KEY" \
  -H "content-type: application/json" -d '{"name":"my-app"}'
```

If `GATEWAY_ADMIN_KEY` is unset, `/admin` returns 404. Client keys can't use the admin API.
Usage counters are saved every 10 seconds.

## nginx

nginx handles TLS. Streaming needs buffering off and long timeouts (the gateway also
sends `X-Accel-Buffering: no`):

```nginx
server {
    listen 443 ssl;
    server_name api.example.com;
    # ssl_certificate / ssl_certificate_key ...

    location / {
        proxy_pass http://192.168.0.131:8443;   # the gateway host
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        proxy_set_header Connection "";
        proxy_buffering off;
        proxy_cache off;
        proxy_read_timeout 900s;
        proxy_send_timeout 900s;
        client_max_body_size 20m;
    }
}
```

If nginx runs on the same machine as the gateway, set `GATEWAY_BIND=127.0.0.1:8443` and
proxy to `http://127.0.0.1:8443`.

## Use it

```python
import anthropic
client = anthropic.Anthropic(api_key="<your gateway key>", base_url="https://api.example.com")
msg = client.messages.create(model="claude-opus-5-5", max_tokens=1024,
                             messages=[{"role": "user", "content": "Hello"}])
```

```bash
curl https://api.example.com/v1/messages -H "x-api-key: $KEY" -H "content-type: application/json" \
  -d '{"model":"claude-opus-5-5","max_tokens":1024,"stream":true,"messages":[{"role":"user","content":"Hi"}]}'
```

## Modes

- **Chat** (`GATEWAY_TOOLS` empty, the default): no tools. The request's `system` replaces
  Claude Code's system prompt.
- **Agent** (`GATEWAY_TOOLS=Read,Grep,WebSearch,...`): the CLI runs its own agent loop in
  `workspace/` with those tools pre-approved. The request's `system` is appended to
  Claude Code's prompt. All the model turns are merged into one response, and the
  CLI's internal tool calls are hidden, so the client sees only text and thinking.
  Anyone with a key can drive these tools on this machine, so grant only what you need.

### Host protection: `workspace/CLAUDE.md`

The CLI runs inside `workspace/` and loads `workspace/CLAUDE.md`, which tells it not to read,
change or reveal anything outside that folder. In testing it refused requests to read
`/etc/hostname` or list the home directory, even with `Read` and `Bash` enabled. These are
instructions, not a sandbox: a clever prompt could still get past them. The hard guarantee
is chat mode, which has no tools. In agent mode, prefer read-only tools and leave out `Bash`,
`Write` and `Edit`.

## Supported request fields

`model`, `messages` (text content only), `system`, `stream`, `max_tokens` (passed as
`CLAUDE_CODE_MAX_OUTPUT_TOKENS`), `output_config.effort`. Earlier turns are replayed to
the CLI as a transcript inside the prompt. Other fields are ignored.

Not supported: images and documents, client-defined `tools` / `tool_use` round trips,
assistant prefill, `temperature`, and `stop_sequences`. Usage numbers come from the CLI,
and in agent mode `output_tokens` is summed across turns.

## Limits

- One CLI process per request. `GATEWAY_MAX_CONCURRENT` caps how many run at once
  (beyond that the gateway returns 429), and `GATEWAY_TIMEOUT_SECS` kills a slow run.
- If the client disconnects, its CLI process is killed.
- Requests count against the logged-in Claude account's usage limits. Keep the gateway
  for your own apps. Serving other people needs an Anthropic API key or a commercial plan.
