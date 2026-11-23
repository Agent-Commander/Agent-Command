# Model catalog and routing (Stage 3)

Stage 3 adds a model catalog and a router to the existing entry points:
the terminal UI and `agent-command catalog {show|refresh}`. Its state lives
in a private `agent-command-routing-v1` directory under the profile root.
Stage 1 and Stage 2 ignore this directory. The Stage 2
`agent-command-usage-v1` and `agent-command-checkpoints-v1` directories keep
their format and behavior.

## Catalog

| Endpoint scope | Source | Speed unit |
|---|---|---|
| `agents/coding` | `https://artificialanalysis.ai/api/v2/language/agents/coding` | seconds per task |
| `models/free` | `https://artificialanalysis.ai/api/v2/language/models/free` | output tokens per second |

The retired `/api/v2/data/...` paths are refused, and a cache that names them falls back to the bundled snapshot.

Each endpoint has its own cache file in `agent-command-routing-v1/catalog/`.
A cache is used only if all of these checks pass:

- the file is at most 1 MiB;
- the schema is `agent-command-catalog-v2` (a v1 cache from the previous Stage 3 revision falls back to the bundled snapshot until the next refresh);
- the scope matches the file's endpoint and the source is that endpoint's published URL;
- the version is well formed;
- there are at most 2,000 rows;
- the SHA-256 of the canonical row serialization matches;
- each row names a known harness (`claude` or `codex`), a well-formed published slug and, for `agents/coding` only, a published effort or none;
- coding and intelligence indexes are null or in 0–100, `agents/coding` rows carry no intelligence index, and a speed is positive and in its endpoint's unit;
- no row repeats a harness, slug and effort.

If a cache is missing, invalid or does not match, Agent Command falls back to
the bundled snapshot for that endpoint only. The other endpoint is unaffected.
`agent-command catalog show` prints each endpoint's origin, the reason for
any fallback, the version, the row count and the provenance.

**Bundled snapshot provenance.** The bundled snapshots are dated 2026-11-23
and contain no rows, by licensing decision. The Data API documentation asks for attribution and
directs users to Artificial Analysis for redistribution rights. No
redistribution permission is documented for this project, so no published
values are bundled and none were invented. On a cold start, before a
verified refresh, every configuration is `unscored` and every speed is
`speed n/a`. `/standard` and `/hard` start nothing and say that no published
coding or intelligence index is available and that a refresh with your own
key is needed. `/any`, `/ordinary` and `/urgent` rank by headroom and then
identity. A licensed snapshot is still
needed for meaningful cold-start ranking. Test rows are labeled
`synthetic test fixture` or use `fixture-` identifiers.

**Refresh with your own key.** Agent Command needs no key by default and makes no network
request unless the user runs `agent-command catalog refresh` with their own
`ARTIFICIAL_ANALYSIS_API_KEY` set. The refreshed values stay in the user's
private cache; Agent Command does not redistribute them. Keys may contain only letters, digits,
`-`, `_` and `.`. The key goes to `curl` on standard input as the `x-api-key`
header, never as a command-line argument. Agent Command never writes the key
to state and redacts it from errors. A refresh:

- starts `curl` with `-q`, so no `.curlrc` or other implicit configuration is read;
- removes `ARTIFICIAL_ANALYSIS_API_KEY` and `CURL_HOME` from the `curl` environment;
- requests only the fixed HTTPS URLs and their 1-indexed `?page=N` forms;
- disables redirects, so requests stay on the same origin;
- limits responses to 4 MiB and 20 pages;
- reads the HTTP status itself and does not depend on a `status` field in the body;
- treats HTTP 401 (key refused), 403 (plan tier or approval refused), 429 (daily limit) and any other non-200 status as a failure for that endpoint only and keeps its previous data;
- treats `agents/coding` as optional: it returns HTTP 403 on the free plan, which keeps its previous data and never suppresses `models/free`. `catalog refresh` exits non-zero only when `models/free` fails;
- requires `tier`, `data` (objects only) and `pagination` with `page` equal to the requested page, `page_size` at least the page's row count, `total_pages` at most 20, and `has_more` true exactly when more pages follow;
- requires the same `tier`, `total_pages` and index version on every page.

Schema source: the public Data API reference
(`https://artificialanalysis.ai/data-api/docs/reference`) and the Tester's
free-tier observations, supplied to this revision as behavior: `models/free`
returned the intact `tier`, `data`, `pagination` and
`intelligence_index_version` envelope over four pages, and `agents/coding`
returned HTTP 403 on the free plan. No live request was made while authoring
this revision; the parser is checked against synthetic fixtures in
`src/catalog/fixtures/` that use actual identity strings with synthetic
values.

| Endpoint | Version | Row identity | Indexes | Speed |
|---|---|---|---|---|
| `models/free` | numeric `intelligence_index_version` | `slug` + `model_creator.name` | `evaluations.artificial_analysis_coding_index`, `evaluations.artificial_analysis_intelligence_index` | `performance_v3.mean_output_tokens_per_second` |
| `agents/coding` (Pro/Commercial, optional) | string `coding_agent_index_version` | `agent.name`, `agent_creator.name` when present, `model.slug`, `model.release_slug`, `model.effort`, `provider`, `is_router` | `coding_agent_index` | `time_per_task_seconds` |

All metrics are nullable. A null or missing metric stays missing (`n/a`) and
is never treated as a measurement. On the tested free tier,
`artificial_analysis_coding_index` and `artificial_analysis_agentic_index`
were always null and `artificial_analysis_intelligence_index` was present but
nullable, so rows with a null coding index and a present intelligence index
are published. The agentic index is not read. `performance_v3` is nullable.
The deprecated `performance.median_output_tokens_per_second` (null from
2026-12-10) is not read.

Cached rows keep the published identity, not a dispatch table:

| Endpoint | Published identity | Cached as |
|---|---|---|
| `models/free` | creator `OpenAI` | harness `codex`, the published slug |
| `models/free` | creator `Anthropic` | harness `claude`, the published slug |
| `agents/coding` | agent `Codex`, creator `OpenAI` or absent, provider `OpenAI`, `is_router` false, no different `release_slug` | harness `codex`, `model.slug`, `model.effort` (null stays null) |
| `agents/coding` | agent `Claude Code`, creator `Anthropic` or absent, provider `Anthropic`, `is_router` false, no different `release_slug` | harness `claude`, `model.slug`, `model.effort` |

The model author (`model_creator`) and the agent author (`agent_creator`) are
checked separately; a crossed pair is skipped. Routers, rows with a missing or
non-boolean `is_router`, other agents, creators or providers (including API
gateways), dated releases that differ from the published slug and unknown rows
are skipped and counted. Rows that repeat an identity are ambiguous and all of
them are skipped. A matched row with a malformed metric rejects the whole
endpoint. A refresh that yields no supported rows keeps the previous data.
Each endpoint is published atomically after it is fully validated.

**Digest round trip.** Agent Command builds `serde_json` with
`float_roundtrip`, so a decimal speed or index parsed from a response
serializes, reopens and verifies against the same SHA-256 digest. Without it,
38 of the 999 synthetic decimal samples in
`decimal_speeds_survive_seal_serialize_reopen_and_verify` failed verification
after reopening.

## Native options and exact identities

A configuration is scored only through an exact identity derived at run time
from the profile's approved native options; there is no model table.

| Native option | Benchmark identity |
|---|---|
| Codex model ID, e.g. `gpt-6.2-sol` | slug with dots turned into hyphens: `gpt-6-2-sol` |
| Full Claude model name, e.g. `claude-opus-5-5` | the name verbatim |
| Explicit effort `low`, `medium`, `high` or `xhigh` | `models/free` slug `<slug>-<effort>`; `agents/coding` row with `model.slug` = `<slug>` and `model.effort` = `<effort>` |
| No effort | `models/free` base slug; `agents/coding` row with a null effort; labeled `AA default setting` |

These stay unscored and never borrow another version's or effort's row:
efforts with no published suffix (`max`, `minimal`, and anything else such as
`ultra`), a missing slug (provider default), rolling aliases (`opus`,
`sonnet`, `haiku`, `opusplan`, `best`, `default`, `claude-…-latest`, a family
name without a version), and options with no matching row (for example
`gpt-reserve` or `codex-auto-review`).

### Model pins

```sh
agent-command model pin claude work claude-opus-5-5 high
agent-command model pin codex work gpt-6.2-sol xhigh
agent-command model show
agent-command model unpin claude work
```

Each profile can hold one pinned model and an optional effort. Pins live in
`agent-command-routing-v1/native-pins-v1.json` with schema
`agent-command-native-pins-v1`. The file is read when the app opens; an
unreadable or invalid file shows a notice and no pin is used. Claude pins must
be full model names. Claude accepts effort `low`, `medium`, `high`, `xhigh` and
`max`; Codex accepts `minimal`, `low`, `medium`, `high` and `xhigh`. `max` and
`minimal` are dispatchable but unscored. The profile panel shows the pin for
the displayed profile (`pin claude-opus-5-5 high` or `no model pin`).

## Eligibility (Stage 2 gate)

Each profile is checked with the Stage 2 allowance gate across all reported
windows. A profile is excluded with a visible reason if it is:

- `latched`;
- `usage state unreadable` (unreadable or corrupt state);
- `usage window exhausted` (at or above 100% with a reset still ahead);
- `sign-in refused` (the native subscription sign-in check fails: Claude `claude.ai`, Codex `Logged in using ChatGPT`).

Missing or stale partial readings do not exclude a profile. API-key
environment variables are removed from every provider command, as in Stage 2.

## Dispatchable configurations

| Provider | Configurations | Native arguments |
|---|---|---|
| claude | provider default; `opus` and `sonnet` aliases (unscored); the profile's pin | `--model <model>` for an alias or pin, `--effort <effort>` when the pin has one |
| codex | provider default; the profile's pin | `-c model="<model>"` and, when the pin has one, `-c model_reasoning_effort="<effort>"` |

Arguments are added after the Stage 2 isolated sign-in arguments, so API-key
variables stay removed and only subscription sign-ins (Claude `claude.ai`,
Codex ChatGPT) are admitted. A usage-limit continuation runs on the alternate
with the exact model and effort of the interrupted configuration.

## Ranking metric and difficulty floors

Every ranking uses one metric for all of its candidates, chosen from the
eligible candidates in this order: AA coding agent index (`agents/coding`),
then AA coding index (`models/free`), then AA intelligence index
(`models/free`). The first metric that has a value for at least one candidate
is used; candidates without that metric are null for the whole ranking and
rank last on it. When no candidate has any index, every configuration is
`unscored`. The metric name appears in every routing reason, ranked
description and floor refusal, for example `Auto → codex/work gpt-6.2-sol high
· AA intelligence index 61.5 · 88 tok/s · 40% left`.

Leading directives select the policy and the floor: `/any` (default, no
floor), `/standard` (≥ 40), `/hard` (≥ 55). Both 0–100 indexes use the same
thresholds. A candidate without the chosen metric never meets a floor. If
nothing qualifies, nothing starts, and the reason names the metric and the
best value or says that no index is available and a refresh with your own key
is needed, followed by any skipped profiles.

**Threshold provenance.** The reference `routing.py` takes its thresholds from
configuration; the private reference example configuration on a normalized
0–1 scale uses routine 0.35, complex 0.50 and critical 0.55. On the 0–100
indexes:

| Reference tier | Reference example | Agent Command | Applied threshold (coding or intelligence index) |
|---|---|---|---|
| routine | 0.35 → 35 | `/any` | none: the existing product routine permits unscored cold-start routes |
| complex | 0.50 → 50 | `/standard` | 40 |
| critical | 0.55 → 55 | `/hard` | 55 |

The Stage 3 task named Standard 35, unless the pinned code differed. The
pinned Stage 3 code (`f9c08be`) uses 40, so 40 is kept. These are local policy
values, not published benchmark facts.

## Ranking and precedence

Comparisons use only matching units:

- **Index:** the single ranking metric described above. Higher ranks first and a null value ranks last.
- **Speed:** seconds per task (lower is better) ranks ahead of tokens per second (higher is better), and a missing speed ranks last. The two units are never compared numerically.
- **Headroom rate:** the minimum, across open windows, of remaining percentage divided by seconds until reset. Higher ranks first. A profile with no open reported window has unknown headroom and ranks after the known rates.

| Policy | Directive | Order |
|---|---|---|
| Default | none | index → speed → headroom → identity |
| Ordinary | `/ordinary` | headroom → index → speed → identity |
| Urgent | `/urgent` | speed → headroom → index → identity |

The identity tie-breaker is the profile name (`provider/name`), then provider
default, aliases and pin, then model and effort names. Ranking does not depend on
input order.

**Divergence from the reference policy (Stage 2 missing-window semantics).**
Stage 2 allows profiles with stale or unreported windows. Stage 3 keeps them
eligible but gives them unknown headroom. In the ordinary policy, an unknown
profile ranks after profiles whose remaining allowance is known, instead of
being treated as having infinite or zero headroom. Stale readings below 100%
use their recorded percentage, as Stage 2 does.

## Selection and override

With no override, the highest-ranked eligible configuration runs (`Auto`). F2
or a click on the profile row cycles through Auto, then each profile, then
back to Auto. The choice is saved to `agent-command-routing-v1/override.json`
and restored when the app reopens. When an override is set:

- its best qualifying configuration runs (`Override → …`);
- if it is excluded or below the floor, the Stage 2 rotation order applies first: same provider, first eligible name (`Override X skipped: <reason> · rotated → …`); otherwise Auto runs (`… · Auto → …`).

The reason appears in the status row and the ship's log. An unreadable or
unknown saved override shows a notice and uses Auto.

## Splittable work

Splitting happens only when the user asks for it explicitly:
`/split unit one ;; unit two ;; unit three` (2–16 non-empty units). Agent
Command never splits free text on its own.

Each profile counts once, with its best-ranked configuration. Units are
assigned in order by the highest-quotient method: a profile's weight is its
lowest remaining percentage across its windows, divided by one plus the units
it already has. Ties go to the higher-ranked profile. Profiles with unknown
headroom get units only if no profile has known positive headroom; then all
weights are equal and assignment is round-robin in rank order.

Each unit's text is sent once, to one profile. Units run one after another in
the same workspace, so no two units write to it at the same time. Before each
unit starts, its profile is checked against the allowance gate again. If it is
no longer eligible, the remaining units stop with the reason.

Each unit uses the Stage 2 path:

- a usage limit checkpoints the workspace and allows at most one fenced continuation, on an eligible same-provider alternate with the same model and effort;
- a pending claim resumed by the user runs with the provider default configuration.

## Rollback

Reverting this revision restores the previous Stage 3 behavior. It ignores
`native-pins-v1.json`, and its v1 catalog check rejects a v2 cache, which then
falls back to the bundled snapshot until a refresh. Reverting the Stage 3
commits restores Stage 2 behavior. Stage 2 ignores `agent-command-routing-v1`.
