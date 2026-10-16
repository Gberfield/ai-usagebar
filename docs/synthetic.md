# Synthetic

[Synthetic](https://synthetic.new) is a privacy-focused AI platform offering
an OpenAI- and Anthropic-compatible API across the models it supports, on a
subscription of packs ($1/day, $30/mo each) or pay-as-you-go credits.

## Setup

1. Create an API key: [dev.synthetic.new](https://dev.synthetic.new) →
   Getting Started.
2. Enable the provider:

   ```toml
   [synthetic]
   enabled = true
   api_key_env = "SYNTHETIC_API_KEY"
   # api_key = "..."   # inline fallback; prefer the environment
   ```

   or enter the key in the Settings overlay (`ai-usagebar-tui`, press `s`).

## What is reported

ai-usagebar reads the documented
[`GET /v2/quotas`](https://dev.synthetic.new/docs/synthetic/quotas) endpoint.
Only the `subscription` block is documented; the quota lanes
(`rollingFiveHourLimit`, `weeklyTokenLimit`, `search.hourly`) are
reverse-engineered and parsed tolerantly — a lane may report a
consumed-percent, a remaining-percent, or a used/limit pair under any
spelling observed on the wire.

Synthetic subscriptions have two regenerating allowances rather than
windows that empty and reset on a timer (per their rate-limits page):

- **5h requests** — regains 5% of the maximum every 15 minutes
- **weekly credits** — regains ~2% of the maximum every 202 minutes

Because of that:

- the 5h and weekly countdowns read **"next top-up"**, not "resets" — the
  actionable figure is when capacity comes back, and the worst-case wait is
  15 minutes (5h) or ~3.4 hours (weekly), not a locked-out window
- no pacing is applied to these two lanes (there is no cliff to pace against)
- the weekly lane also shows the API's own credit figures
  (`$46.45 of $48.00 left`) with the wire's formatting preserved
- the **hourly web-search window** is a real window and keeps normal reset
  semantics

`subscription` (duplicates the rolling limit's maximum behind an ambiguous
`renewsAt`) and `freeToolCalls` (zero on every plan observed) are not
modelled. A lane that is present but unreadable is a schema error rather
than a fabricated 0%.

## Format placeholders

`{syn_plan}`, `{syn_session_pct}`, `{syn_session_reset}`,
`{syn_weekly_pct}`, `{syn_weekly_reset}`, `{syn_search_pct}`,
`{syn_search_reset}`, `{syn_credits_left}`, `{syn_credits_max}`, plus the
generic `{icon}`, `{plan}`, `{vendor_short}`, `{session_pct}`,
`{session_reset}`, `{weekly_pct}`, `{weekly_reset}`.

## CLI

```bash
ai-usagebar --vendor synthetic
ai-usagebar --vendor synthetic --format '{syn_session_pct}% · {syn_credits_left}'
ai-usagebar usage --json   # includes a "synthetic" entry when enabled
```
