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

Unknown keys are an error, so a misspelled key doesn't silently do nothing.

Notes:

- Prompt caching is always on. The system prompt and the conversation are cached for 1 hour in
  chats and 5 minutes in scheduled jobs. Cache reads and writes are recorded in usage.
- Images (JPEG, PNG, GIF, WebP) and PDFs are sent to the model. Other attachments reach it only as
  a text reference.
- A `temperature` is sent only to Claude 4.6 and 4.5 models, and only with an `anthropic_version`
  before `2026-10-21`: newer models and API versions reject it, so Giles leaves it out there and the
  model uses its default.
- An agent without a reasoning budget gets thinking turned off on models that allow it. Claude Opus
  5.5, Sonnet 5.5, Fable, and Mythos always think.
