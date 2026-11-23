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
- the schema is `agent-command-catalog-v1`;
- the scope matches the file's endpoint and the source is that endpoint's published URL;
- the version is well formed;
- there are at most 2,000 rows;
- the SHA-256 of the canonical row serialization matches;
- each row names a supported provider, model and effort, has a coding index in 0–100, and has a positive speed in its endpoint's unit;
- no row repeats a provider, model and effort.

If a cache is missing, invalid or does not match, Agent Command falls back to
the bundled snapshot for that endpoint only. The other endpoint is unaffected.
`agent-command catalog show` prints each endpoint's origin, the reason for
any fallback, the version, the row count and the provenance.

**Bundled snapshot provenance.** The bundled snapshots are dated 2026-11-23
and contain no rows. The coding-agent leaderboard
(`https://artificialanalysis.ai/agents/coding-agents`) and the Data API
reference could not be reached from the authoring environment. The network
proxy refused the connection. So neither the values nor permission to
redistribute them could be verified, and none were invented. Until a verified
refresh, every index and speed shows as missing (`index n/a`, `speed n/a`).
Test rows are labeled `synthetic test fixture` or use `fixture-` identifiers.

**Refresh.** Agent Command needs no key by default and makes no network
request unless the user runs `agent-command catalog refresh` with
`ARTIFICIAL_ANALYSIS_API_KEY` set. Keys may contain only letters, digits,
`-`, `_` and `.`. The key goes to `curl` on standard input as the `x-api-key`
header, never as a command-line argument. Agent Command never writes the key
to state and redacts it from errors. A refresh:

- starts `curl` with `-q`, so no `.curlrc` or other implicit configuration is read;
- removes `ARTIFICIAL_ANALYSIS_API_KEY` and `CURL_HOME` from the `curl` environment;
- requests only the fixed HTTPS URLs and their `?page=N` forms;
- disables redirects, so requests stay on the same origin;
- limits responses to 4 MiB and 20 pages;
- reads the HTTP status itself and does not depend on a `status` field in the body;
- treats HTTP 401 or 403 (key or plan tier refused), 429 and any other non-200 status as a failure for that endpoint and keeps its previous data;
- checks the `data` array, the page number, a constant page total, and a constant version across pages.

`models/free` rows are read from `slug`, `model_creator.slug`,
`evaluations.artificial_analysis_coding_index` and
`median_output_tokens_per_second`. These names follow the earlier public
free-tier shape. They were not checked against the current reference because
it could not be reached.

`agents/coding` is never requested. Its response fields could not be checked
against the published reference, so refresh reports
`agents/coding response fields are not verified against the published reference; previous data kept`
and keeps the previous cache or bundled snapshot.

Rows are normalized through the supported-configuration table. Unknown models
are skipped. A row with a malformed metric rejects the whole endpoint. Each
endpoint is published atomically after it is fully validated. A failed endpoint
keeps its previous data.

## Eligibility (Stage 2 gate)

Each profile is checked with the Stage 2 allowance gate across all reported
windows. A profile is excluded with a visible reason if it is:

- `latched`;
- `usage state unreadable` (unreadable or corrupt state);
- `usage window exhausted` (at or above 100% with a reset still ahead);
- `sign-in refused` (the native subscription sign-in check fails: Claude `claude.ai`, Codex `Logged in using ChatGPT`).

Missing or stale partial readings do not exclude a profile. API-key
environment variables are removed from every provider command, as in Stage 2.

## Supported configurations

| Provider | Model | Effort | Native arguments |
|---|---|---|---|
| claude | provider default | default | none |
| claude | `opus`, `sonnet` | default | `--model <model>` |
| codex | provider default | default | none |
| codex | `gpt-5-codex` | default, low, medium, high | `-c model="gpt-5-codex"` and, for an explicit effort, `-c model_reasoning_effort="<effort>"` |

Claude has no effort option here. Agent Command does not pass a Claude effort
flag that it has not verified.

## Difficulty floors

Leading directives in the task select the policy and the floor: `/any`
(default, no floor), `/standard` (index ≥ 40), `/hard` (index ≥ 55). These
floors are local policy values, not published benchmark facts.

The published coding index must meet the floor. A configuration with no index
never meets a floor. If nothing qualifies, nothing starts, and the reason
gives the best published index or says none is available, followed by any
skipped profiles.

## Ranking and precedence

Comparisons use only matching units:

- **Coding index:** the exact provider/model/effort row from `agents/coding` is used first, then the one from `models/free`. Indexes are compared only within the same endpoint; `agents/coding` ranks ahead of `models/free`, and a missing index ranks last.
- **Speed:** seconds per task (lower is better) ranks ahead of tokens per second (higher is better), and a missing speed ranks last. The two units are never compared numerically.
- **Headroom rate:** the minimum, across open windows, of remaining percentage divided by seconds until reset. Higher ranks first. A profile with no open reported window has unknown headroom and ranks after the known rates.

| Policy | Directive | Order |
|---|---|---|
| Default | none | index → speed → headroom → identity |
| Ordinary | `/ordinary` | headroom → index → speed → identity |
| Urgent | `/urgent` | speed → headroom → index → identity |

The identity tie-breaker is the profile name (`provider/name`), then the
configuration's row order in the table above. Ranking does not depend on
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

Reverting the Stage 3 commit restores Stage 2 behavior. Stage 2 ignores
`agent-command-routing-v1`.
