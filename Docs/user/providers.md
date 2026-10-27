# Model Providers

A provider instance connects Giles to a model API. Each one is a `[provider.<instance>]` table in
`giles.toml`; agents name models as `<instance>/<model id>`, e.g. `anthropic/claude-opus-5-5`. Only
the `config` table is passed to the provider; secrets in it are written as references
(`{ secret = "name" }` from `secrets.toml`, or `{ env = "VAR" }`).

## Anthropic

The built-in `anthropic` component talks to the Anthropic Messages API. It's compiled in by default
(Cargo feature `provider-anthropic`).

```toml
[provider.anthropic]
config = { api_key = { secret = "anthropic" } }
```

| Key                 | Default                     | Meaning                                                            |
| ------------------- | --------------------------- | ------------------------------------------------------------------ |
| `api_key`           | required                    | The API key, sent as `x-api-key`                                   |
| `base_url`          | `https://api.anthropic.com` | The API root without `/v1`, e.g. a gateway; `http` is allowed      |
| `anthropic_version` | `2023-06-01`                | The `anthropic-version` header                                     |
| `beta`              | `[]`                        | Beta feature names, sent in the `anthropic-beta` header            |
| `max_tokens`        | the model's maximum         | Output limit for requests that set none (16384 for unknown models) |

An unknown key is ignored with a warning at startup (a misspelled key does nothing, so the setting
keeps its default); a known key with an invalid value stops startup.

Notes:

- Prompt caching is always on. The system prompt and the conversation are cached for 1 hour in chats
  and 5 minutes in scheduled jobs. Cache reads and writes are recorded in usage.
- Images (JPEG, PNG, GIF, WebP) and PDFs are sent to the model. When the model id is one the Models
  API lists (not an alias such as `claude-sonnet-4-5`), Giles also leaves out the types it says the
  model can't take. Other attachments reach it only as a text reference.
- A `temperature` is sent only to Claude 4.6 and 4.5 models, and only with an `anthropic_version`
  before `2026-10-21`: newer models and API versions reject it, so Giles leaves it out there and the
  model uses its default.
- An agent without a reasoning budget gets thinking turned off on models that allow it. Claude Opus
  5.5, Sonnet 5.5, Fable, and Mythos always think.

## OpenAI-compatible

The built-in `openai` component talks to any server with an OpenAI-compatible Chat Completions API:
OpenAI, OpenRouter, Ollama, vLLM, llama.cpp, and most gateways. It's compiled in by default (Cargo
feature `provider-openai`). Use one instance per server, with `component = "openai"` when the
instance has another name:

```toml
[provider.openai]
config = { api_key = { secret = "openai" }, reasoning = true }

[provider.openrouter]
component = "openai"

[provider.openrouter.config]
base_url = "https://openrouter.ai/api/v1"
api_key = { env = "OPENROUTER_API_KEY" }
headers = { "HTTP-Referer" = "https://giles.example", "X-Title" = "Giles" }

[provider.ollama]
component = "openai"

[provider.ollama.config]
base_url = "http://localhost:11434/v1"
models."qwen2.5vl:7b" = { media_types = ["image/png", "image/jpeg"] }
models."qwen3:8b" = { reasoning = true, context_window = 40960 }
```

| Key              | Default                     | Meaning                                                                                      |
| ---------------- | --------------------------- | -------------------------------------------------------------------------------------------- |
| `api_key`        | none                        | Sent as `Authorization: Bearer <key>`; local servers such as Ollama need none                |
| `base_url`       | `https://api.openai.com/v1` | The API root including `/v1` (OpenRouter: `https://openrouter.ai/api/v1`); `http` is allowed |
| `dialect`        | from the `base_url` host    | `openai`, `openrouter`, or `generic`: which server-specific request fields to use            |
| `headers`        | none                        | Extra headers for every request, e.g. OpenRouter's `HTTP-Referer` and `X-Title`              |
| `max_tokens`     | the server's default        | Output limit for requests that set none                                                      |
| `media_types`    | see below                   | Attachment types the models accept                                                           |
| `context_window` | reported by the server      | The models' context window in tokens, for compaction                                         |
| `reasoning`      | `false`                     | Whether the models reason; reasoning settings are sent only to models that do                |
| `models`         | none                        | Per-model settings: `max_tokens`, `media_types`, `context_window`, and `reasoning`           |

Settings under `models."<model id>"` override the instance-level ones for that model. Unknown keys
(in `models` too) are ignored with a warning at startup; a known key with an invalid value stops
startup.

Notes:

- The dialect is `openai` for `api.openai.com`, `openrouter` for `openrouter.ai`, and `generic`
  otherwise. Set it when a proxy sits in front of OpenAI or OpenRouter.
- Media: `media_types` may list image types (`image/png`, ...), `audio/wav`, `audio/mpeg`, and
  `application/pdf`. Without it, OpenAI and OpenRouter models get JPEG, PNG, GIF, and WebP images,
  and other servers get no attachments, since vision support varies by local model and a model that
  can't take an image fails every later request in the conversation. Attachments a model doesn't
  take reach it only as a text reference. OpenRouter's model list also says which models take
  images, PDFs, or audio, which narrows the instance's list.
- Images returned by tools are shown to the model in a message right after the tool results, since
  the API can't carry images in a tool result.
- Reasoning: on OpenAI and other servers, a reasoning budget is sent as `reasoning_effort` (`low` up
  to 2048 tokens, `medium` up to 8192, `high` above) only to models with `reasoning = true`; servers
  reject it for other models. OpenRouter gets the budget itself (`reasoning.max_tokens`) for every
  model and ignores it where it doesn't apply. Without a budget nothing is sent and the model
  reasons as it does by default. On OpenAI, models with `reasoning = true` get no `temperature`,
  which they reject.
- Reasoning text that a server streams (OpenRouter, Ollama, DeepSeek) is kept and sent back to it in
  later requests, which DeepSeek requires during tool use.
- Prompt caching: OpenAI, DeepSeek, Gemini, and most servers cache on their own; cached tokens are
  recorded in usage where the server reports them. On OpenRouter, Giles also asks for Anthropic's
  automatic caching (1 hour in chats, 5 minutes in scheduled jobs).
- `list_models` asks the server for its model ids. OpenAI and Ollama report nothing else, so context
  windows and capabilities come from this config; OpenRouter reports context windows, output limits,
  and capabilities.
