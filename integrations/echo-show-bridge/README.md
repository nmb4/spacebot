# Spacebot Echo Show Bridge

Alexa Custom Skill bridge for Echo Show 5 that connects to Spacebot through the existing webhook messaging adapter.

## What This Does

- Treats Echo Show as a Spacebot channel (`echo_show:<hash>` conversation IDs).
- Sends user speech to Spacebot via `POST /send`.
- Polls Spacebot replies via `GET /poll/{conversation_id}`.
- Speaks the response and optionally renders an APL screen if Spacebot includes a structured `echo_show` JSON directive.

## Directory Layout

- `src/` bridge runtime code (Lambda handler + parser/client modules)
- `test/` unit/integration tests (`node:test`)
- `alexa/skill-package/` Alexa manifest + interaction model artifacts
- `scripts/` local invoke and Lambda zip build scripts

## Prerequisites

- Node.js 20+
- A running Spacebot instance with webhook adapter enabled and reachable from AWS over HTTPS
- Amazon Developer account + Echo Show 5 on the same account

## Spacebot Config

Enable webhook and bind it to your target agent:

```toml
[messaging.webhook]
enabled = true
port = 18789
bind = "0.0.0.0"

[[bindings]]
agent_id = "main"
channel = "webhook"
```

## Bridge Environment

Copy `.env.example` values into your deployment environment:

- `SPACEBOT_WEBHOOK_BASE`: base URL of Spacebot webhook adapter (no trailing slash)
- `SPACEBOT_AGENT_ID`: optional explicit target agent
- `SPACEBOT_CONVERSATION_PREFIX`: defaults to `echo_show`
- `SPACEBOT_POLL_INTERVAL_MS`: poll interval for `/poll`
- `SPACEBOT_MAX_WAIT_MS`: total wait time per request
- `SPACEBOT_USER_HASH_SALT`: optional salt for conversation hashing

## Local Testing

```bash
npm install
npm test
SPACEBOT_WEBHOOK_BASE=http://127.0.0.1:18789 npm run invoke:local
```

## Deploy to Lambda

```bash
npm install
npm run build:lambda-zip
```

Upload `lambda.zip` to Lambda (Node.js runtime), set env vars, then point the Alexa skill endpoint to this Lambda.

## Alexa Skill Package

Use files in `alexa/skill-package/`:

- `skill.json`
- `interactionModels/custom/en-US.json`

Invocation name is currently `space bot`.  
Primary intent is `ChatIntent` with an `AMAZON.SearchQuery` slot for free-form utterances.

## Visual Directive Contract

Spacebot may include this in a fenced JSON block:

```json
{
  "echo_show": {
    "template": "content_list_v1",
    "title": "Now",
    "body": "Summary text",
    "items": ["item 1", "item 2"],
    "image_url": "https://example.com/image.png"
  }
}
```

Invalid JSON or schema is ignored and the bridge falls back to speech-only responses.
