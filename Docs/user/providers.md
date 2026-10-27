# Model Providers

A provider instance connects Giles to a model API.

Each instance is a `[provider.<instance>]` table in `giles.toml`. Agents name a model as
`<instance>/<model id>`, for example `anthropic/claude-opus-5-5`.

The provider's settings go in the instance's `config` table. Write secrets there as
[references](configuration.md#secrets): `{ secret = "name" }` for a secret in `secrets.toml`, or
`{ env = "VAR" }` for an environment variable.

## Anthropic

The built-in `anthropic` component talks to the Anthropic Messages API.

```toml
[provider.anthropic]
config = { api_key = { secret = "anthropic" } }
```

| Key                 | Default                     | Meaning                                                        |
| ------------------- | --------------------------- | -------------------------------------------------------------- |
| `api_key`           | required                    | The API key                                                    |
| `base_url`          | `https://api.anthropic.com` | The API root without `/v1`, such as a gateway's; may be `http` |
| `anthropic_version` | `2023-06-01`                | The `anthropic-version` header                                 |
| `beta`              | `[]`                        | Beta feature names, sent in the `anthropic-beta` header        |
| `max_tokens`        | the model's maximum         | Default output limit (16384 for unknown models)                |

Giles sends images (JPEG, PNG, GIF, WebP) and PDFs to the model. Other attachments reach it only as
a text reference.

Without a reasoning budget, thinking is turned off on models that allow it. Claude Opus 5.5, Sonnet
5.5, Fable, and Mythos always think.

## OpenAI-compatible

The built-in `openai` component talks to any server with an OpenAI-compatible Chat Completions API.
That includes most gateways and these servers:

- OpenAI
- OpenRouter
- Ollama
- vLLM
- llama.cpp

Add one instance per server. Set `component = "openai"` when the instance has another name.

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

| Key              | Default                         | Meaning                                                                            |
| ---------------- | ------------------------------- | ---------------------------------------------------------------------------------- |
| `api_key`        | none                            | Sent as a bearer token; local servers such as Ollama need none                     |
| `base_url`       | `https://api.openai.com/v1`     | The API root including `/v1`; may be `http`                                        |
| `dialect`        | from the `base_url` host        | `openai`, `openrouter`, or `generic`                                               |
| `headers`        | none                            | Extra headers for every request, such as OpenRouter's `HTTP-Referer` and `X-Title` |
| `max_tokens`     | the server's default            | Default output limit                                                               |
| `media_types`    | see [Attachments](#attachments) | Attachment types the models accept                                                 |
| `context_window` | reported by the server          | The models' context window in tokens, for compaction                               |
| `reasoning`      | `false`                         | Whether the models reason; reasoning settings are sent only to models that do      |
| `models`         | none                            | Per-model settings: `max_tokens`, `media_types`, `context_window`, and `reasoning` |

Settings under `models."<model id>"` override the instance's settings for that model.

The dialect is `openai` for `api.openai.com`, `openrouter` for `openrouter.ai`, and `generic`
otherwise. Set `dialect` when a proxy sits in front of OpenAI or OpenRouter.

Without a reasoning budget, Giles sends no reasoning settings, and each model reasons as it does by
default.

### Describing your models

OpenAI and Ollama report only their model ids. Set `context_window`, `media_types`, and `reasoning`
for their models yourself.

OpenRouter reports context windows, output limits, and which models take images, PDFs, or audio.

### Attachments

Without `media_types`, OpenAI and OpenRouter models get JPEG, PNG, GIF, and WebP images. Other
servers get no attachments, since vision support varies by local model.

`media_types` may list:

- image types, such as `image/png`
- `audio/wav` and `audio/mpeg`
- `application/pdf`

**List only types the model accepts.** A model that can't take an image fails every later request in
the conversation.

Attachments a model doesn't take reach it only as a text reference.
