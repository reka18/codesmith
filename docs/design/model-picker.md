# Design: direct providers, brokers, and one model picker

Status: **approved by Ray on 2026-11-26.** Not built yet. The Bifrost
provider comes first, in its own PR.

## Why

Some services reach models through another company's API. OpenRouter,
Bifrost, Vercel AI Gateway and LiteLLM's proxy all work this way. One key
reaches many upstream providers, the model ids name the upstream, and what
you may use depends on your key. Billing, logging and who sees your traffic
differ from calling OpenAI, Anthropic or Moonshot directly. The picker should
make that difference visible, while keeping the common case one step.

## Words

- A **direct provider** serves models itself, even ones it didn't train.
  Examples: OpenAI, Anthropic, Moonshot, Fireworks, Together, Groq, Bedrock,
  Vertex, Azure, Copilot. You sign in with a subscription (Claude Code,
  ChatGPT, Copilot) or an API key.
- A **broker** forwards your request to another company's API.
- A **route** is the chain a request travels, from the provider you pick to
  the one that serves the model.

## Routes

Being a broker is a property of a route, not of a company. The same company
can be direct on one route and an upstream, or a broker, on another.

| You pick | Route | Kind |
|---|---|---|
| `fireworks/<model>` | Fireworks | direct |
| `bifrost/fireworks/<model>` | Bifrost → Fireworks | brokered |
| `openrouter/anthropic/<model>` | OpenRouter → Anthropic | brokered |
| `bifrost/fireworks/anthropic/<model>` | Bifrost → Fireworks → Anthropic | double-brokered |

- **Known, never guessed.** A route comes only from stated facts:
  - a broker's own model list, which names the next hop;
  - catalog metadata (models.dev, or the provider's API);
  - config.

  Each hop reads only its own prefix. CodeSmith sends `fireworks/anthropic/x`
  to Bifrost, and Bifrost sends `anthropic/x` to Fireworks. So the hop after
  Fireworks is known only if Fireworks's catalog says so. Otherwise the route
  ends at "Fireworks, model `anthropic/x`".
- **Config.** `"kind": "broker"` on a provider marks a custom gateway, such as
  your own LiteLLM or Bifrost, as a broker. A model's `"route"` corrects a
  route the catalog gets wrong. The built-in brokers (OpenRouter, Bifrost,
  Vercel AI Gateway) mark themselves.
- **Ids don't change.** `-m`, saved config and scripts keep working as typed.

What follows from a route:

| | Rule |
|---|---|
| Effort levels | The levels of the model at the end of the route, sent in the form the next hop accepts. Reaching a model directly or through a broker offers the same levels, unless a broker documents that it limits them. |
| Billed cost (`usd`) | Whatever the first hop reports it billed. |
| Estimated cost (`usd_estimated`) | Priced at the hop that bills you, not the model maker's list price. For example, Fireworks's price for a model reached through Bifrost → Fireworks. |
| Who sees your traffic | Every company on the route. |
| A broker's model list | Fetched from the gateway with the user's own key each time a session starts. A saved list, filed under a hash of the key, is used only when that fetch fails, with a notice. |

## The picker

One picker that you can browse route-first or search model-first. Effort is
chosen on the same screen. Keys are arrows, Enter, Esc and typing; there's no
mouse. Favourites (ctrl+f) stay at the top with recent models.

### Opening it

Recent picks first, then direct providers, then brokers, each with how you're
signed in. Providers you aren't signed in to are listed last, one key from
signing in.

```
 Choose a model                                   type to search
 Recent
 ▸ Opus           Anthropic · subscription              effort high
   GLM            Bifrost › Fireworks · key             effort medium
 Direct
   Anthropic      subscription                  12 models  →
   OpenAI         API key                       30 models  →
   Moonshot       API key                        8 models  →
 Brokers
   Bifrost        key · estimated cost          41 models  →
   OpenRouter     key · billed cost            300 models  →
 Not signed in
   Google, Groq, Fireworks …                       sign in  →
```

A provider with both a subscription and an API key shows as two rows, one
for each way of paying.

### Inside a broker

The broker's upstream providers, then their models. The rest of the route is
shown as a breadcrumb, so the picker is never more than two levels deep.

```
 Bifrost › Fireworks                               5 models
 ▸ GLM                    via Fireworks
   Kimi                   via Fireworks
   Opus                   via Fireworks › Anthropic
```

### Searching

Typing searches every route. Each model is listed once, with every way you
can reach it under it, and each route shows how it's paid.

```
 opus
 Opus
 ▸ Anthropic                        subscription     direct
   Anthropic                        API key          direct
   Bifrost › Anthropic              key              estimated cost
   Bifrost › Fireworks › Anthropic  key              estimated cost
```

### Effort

The highlighted row shows only the levels its model supports. ←/→ changes the
level, and Enter picks the model and level together. A model with no levels
shows no effort row. `/effort` keeps working on its own, for changing the
level without changing the model.

```
 Effort   low   medium  [high]  max                  ←/→ to change
```

### Text form

`codesmith models` prints the same groups as text: direct providers, then
brokers with their upstreams, with each route and how it's paid.

## Order of work

1. **The Bifrost PR (in progress).**
   - It chooses Bifrost's API by the first upstream: `anthropic/…` goes to
     the Anthropic-compatible API, everything else to the OpenAI-compatible
     one.
   - It fetches the model list for each user's key at session start.
2. **Routes in the data model.**
   - Routes, with `kind: "broker"` and the model `route` setting.
   - OpenRouter, Bifrost and Vercel AI Gateway marked as brokers.
   - Effort levels and cost taken by route.
3. **The picker and `codesmith models` as above.**

## Tests

- **Routes:** each kind in the table resolves to the right route. A route is
  never inferred from a name alone, and the `route` setting overrides the
  catalog.
- **Effort:** the same model reached directly and through a broker offers
  the same levels, for a Claude model and for a non-Claude reasoning model.
  In a double-brokered route, effort is sent in the next hop's form.
- **Picker:**
  - the opening order;
  - a provider with a subscription and an API key showing as two rows;
  - the breadcrumbs;
  - search listing each model once with all its routes;
  - ←/→ changing effort, and Enter applying both;
  - a model with no levels showing no effort row;
  - providers not signed in listed last.
- **Broker lists:**
  - refreshed when a session starts;
  - switching keys switches the list;
  - the saved list used, with a notice, when the gateway is down.
- **Ids:** `-m` with two, three and four segments round-trips unchanged.
