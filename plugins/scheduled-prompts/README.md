# scheduled-prompts

Let a node do useful work on a schedule instead of only when someone is
watching.

The operator writes jobs in a file — a cron expression, a prompt, a model, and
where the answer goes. This plugin runs them, never overlaps a job with itself,
caps how much of the machine the whole schedule can take, keeps out of the hours
the owner reserved, and writes a bounded record of what happened.

Six MCP tools, projected by the host:

| Tool | On the MCP endpoint | What it does |
| --- | --- | --- |
| `list` | `scheduled-prompts.list` | Every declared job: schedule, model, sink, next due, last run. |
| `status` | `scheduled-prompts.status` | Where the jobs file is, whether it loaded, the endpoint, the caps. No network. |
| `history` | `scheduled-prompts.history` | Recent runs with outcome, duration, and tokens. |
| `run_now` | `scheduled-prompts.run_now` | Run one declared job immediately, subject to the same guards. |
| `pause` | `scheduled-prompts.pause` | Stop a job from its next occurrence. |
| `resume` | `scheduled-prompts.resume` | Let a paused or quarantined job run again. |

The same six are mounted over HTTP by the host — `GET .../http/jobs`,
`.../http/status`, `.../http/history`, and `POST .../http/run`,
`.../http/pause`, `.../http/resume`, all under
`/api/plugins/scheduled-prompts/`. Reads are `GET`; the three that change what
the machine does are `POST`.

**There is no tool that creates, edits, or deletes a job.** That is the central
design decision, and the next section is why.

---

## Why a model cannot create a job

A tool that lets a model schedule its own future execution is a much larger
trust decision than it looks like on the tool list. `create_job(schedule,
prompt)` reads like a convenience. What it actually grants is four things at
once:

- **Persistence beyond the conversation.** Whatever wrote the job is gone by the
  time it runs. There is no session to end, no context to close, and no way to
  ask the thing that scheduled it what it meant.
- **Self-invocation with a self-authored prompt.** The model chooses both *when
  it runs again* and *what it says to itself* when it does. Every review step
  that a human turn would have provided is gone.
- **A standing claim on somebody else's hardware.** This is donated compute.
  "Every fifteen minutes, forever" is a decision about a stranger's electricity
  bill and their GPU's thermal budget, and it should be made by the person who
  owns them.
- **An outbound channel that outlives the session.** Output goes to a file or a
  webhook. A job created in one conversation keeps writing to a destination
  chosen in that conversation, long after it ended.

Each of those is defensible when a person chose it deliberately. None of them is
defensible as a side effect of a tool call. So the schedule lives in a file the
operator owns, edits, diffs, and reverts — and there is no code path in this
plugin from a tool call to a new job. It is not a permission check that could be
misconfigured; the function that builds a job takes a file's text and nothing
else.

### What a model *can* do here, precisely

| Action | Allowed? | Bounded by |
| --- | --- | --- |
| See the jobs, their schedules, their history | Yes | Nothing; these are reads |
| Run an already-declared job now (`run_now`) | Yes | One named job. Refused while it is running, while it is backing off, when the concurrency cap is full, and outside its window unless the **file** opted that job in |
| Pause a job (`pause`) | Yes | Ephemeral: not written to disk, cleared by a restart |
| Resume a paused job (`resume`) | Yes | Cannot start a job the file set `enabled = false` |
| Create, edit, or delete a job | **No** | There is no such tool |
| Change a prompt, a model, a schedule, or a sink | **No** | There is no such tool |

Two of those are worth stating outright.

**`run_now` spends GPU time, and a model can call it.** It is deliberately
narrow: it names exactly one job, has no "run everything" form, and cannot
choose what that job says. It also honours every guard that protects the
machine, including the operator's hours — a *tool argument* cannot widen the
window, only `manual_ignores_window` in the jobs file can. What a model can do
with it is make an operator-approved job happen sooner. That is a real cost, and
it is the smallest one that makes "trigger one now" useful at all.

**`pause` is a denial of service on the operator's own automation.** Nothing
stops a model pausing every job. Three things bound it: the pause is visible in
`list` with a timestamp and a note, it cannot outlive a restart, and it cannot
be used to *start* anything. The jobs file remains the only durable statement of
what this machine has agreed to run.

---

## The jobs file

Default location `~/.tdcc/scheduled-prompts.toml` (`%USERPROFILE%\.tdcc\…` on
Windows); `--jobs <path>` or `TDCC_SCHEDULED_PROMPTS_JOBS_FILE` moves it.

```toml
version = 1                     # required; this build understands version 1

timezone = "local"              # "local" (default) | "utc"
max_concurrent_runs = 1         # 1-8. How much of the machine the schedule may take at once
window = "22:00-06:00"          # optional default hours for every job
misfire = "run_once"            # "run_once" (default) | "skip"
catch_up_grace_secs = 3600      # how stale a missed occurrence may be and still run
history_per_job = 20            # detailed runs kept per job, 1-200

[[job]]
id = "nightly-digest"                       # required, unique, [A-Za-z0-9._-]
description = "What the node did overnight" # shown by `list`
schedule = "0 3 * * *"                      # required
model = "qwen3:8b"                          # required, as /v1/models names it
# The prompt is required, and is sent verbatim every time this job fires.
prompt = """
Summarise, in five bullet points, what a small home GPU node
should check after a week of unattended operation.
"""
system = "You are terse and concrete."
enabled = true                  # the file's own switch; no tool can flip this to true
timeout_secs = 300              # 5-3600
max_output_tokens = 800         # optional; omitted means the server's own default
temperature = 0.2               # optional, 0.0-2.0
sink = { kind = "file", path = "digests/nightly.md", format = "text" }

[[job]]
id = "hourly-alert"
schedule = "0 * * * *"
window = "22:00-06:00"          # narrower than the file default, or its own
misfire = "skip"                # an hourly alert is worthless an hour late
model = "qwen3:8b"
prompt = "One sentence: anything unusual in the last hour?"
quarantine_after_failures = 5   # park it after five failures in a row; 0 disables
sink = { kind = "webhook", url_env = "TDCC_SCHEDULED_PROMPTS_WEBHOOK_ALERT" }
```

**Unknown keys are errors.** `scheduel = "0 3 * * *"` does not silently do
nothing; it stops the file loading, with the key and the line. A typo that
quietly disabled a job would be indistinguishable from a node that was never
busy.

### Every setting

File level, all optional except `version`:

| Key | Range | Default | Meaning |
| --- | --- | --- | --- |
| `version` | `1` | — | Required. A different value is refused, not guessed at. |
| `timezone` | `local`, `utc` | `local` | Which clock schedules and windows are read on. |
| `window` | `HH:MM-HH:MM` | none | Default hours for every job. |
| `max_concurrent_runs` | 1-8 | `1` | Runs in flight across all jobs. |
| `misfire` | `run_once`, `skip` | `run_once` | Default catch-up policy. |
| `catch_up_grace_secs` | 0-86400 | `3600` | Default staleness limit for a catch-up run. |
| `history_per_job` | 1-200 | `20` | Detailed runs kept per job. |

Job level:

| Key | Range | Default | Meaning |
| --- | --- | --- | --- |
| `id` | ≤64 chars, `[A-Za-z0-9._-]` | — | Required, unique. Names the job in every tool. |
| `schedule` | cron or `@shorthand` | — | Required. See [Schedules](#schedules). |
| `model` | non-empty | — | Required, as the endpoint's `/v1/models` names it. |
| `prompt` | ≤32,768 chars | — | Required. |
| `sink` | see below | — | Required. |
| `description` | ≤240 chars | none | Shown by `list`. |
| `system` | ≤8,192 chars | none | System message, sent before the prompt. |
| `enabled` | bool | `true` | The file's own switch. `resume` cannot set it. |
| `window` | `HH:MM-HH:MM` | file default | Hours this job may run in. |
| `misfire` | `run_once`, `skip` | file default | Catch-up policy. |
| `catch_up_grace_secs` | 0-86400 | file default | Staleness limit for this job. |
| `timeout_secs` | 5-3600 | `300` | How long one completion may take. |
| `max_output_tokens` | 1-131072 | unset | Omitted from the request when unset. |
| `temperature` | 0.0-2.0 | unset | Omitted from the request when unset. |
| `manual_ignores_window` | bool | `false` | Whether `run_now` may run this job outside its window. |
| `quarantine_after_failures` | 0-10000 | `10` | Consecutive failures before the job parks itself. `0` disables. |

At most 64 jobs. Every job is a standing claim on the machine, and a file with
hundreds of them is a mistake worth catching at load.

### Sinks

```toml
sink = { kind = "file", path = "digests/nightly.md", format = "text" }
sink = { kind = "file", path = "runs.jsonl", format = "jsonl" }
sink = { kind = "webhook", url_env = "TDCC_SCHEDULED_PROMPTS_WEBHOOK_ALERT" }
```

**File.** `path` is relative to the output directory and `/`-separated on every
platform. It is built from plain names — letters, digits, `-`, `_`, `.` — at
most 8 levels deep. There is no input that reaches an absolute path, a `..`, a
drive letter, a UNC share, or a Windows device name, and the sink re-checks
containment against the canonicalized root before writing, which is what catches
a symlink pointing out of the tree. `format = "text"` appends a dated header and
the answer; `format = "jsonl"` appends one JSON object per line.

**Webhook.** `url_env` names an **environment variable**, never the URL. A Slack
or Discord webhook URL is a bearer credential: anyone holding it can post as the
integration, and a jobs file is the kind of thing people paste into an issue.
Writing a URL there is refused by name. The variable must be set in the
environment of the `tdcc` process when the plugin starts, or the file does not
load — a delivery target that cannot be resolved should fail loudly, not drop
every run silently. The webhook receives one JSON object per run, the same shape
`jsonl` writes.

```bash
export TDCC_SCHEDULED_PROMPTS_WEBHOOK_ALERT='https://hooks.slack.com/services/…'
```

---

## Schedules

Five fields, minute resolution: `minute hour day-of-month month day-of-week`.
`*`, `a`, `a,b`, `a-b`, `*/n`, `a/n`, and `a-b/n` work in every field. Months
accept `jan`/`january`, days accept `sun`/`sunday`, and `7` is a second spelling
of Sunday.

```text
0 3 * * *        03:00 every day
*/15 * * * *     every quarter hour
0 9-17 * * 1-5   on the hour, working hours, weekdays
0 0 1 * *        the first of the month
@hourly @daily @midnight @weekly @monthly @yearly @annually
```

**When both day fields are restricted, either one matching is enough.**
`0 0 1 * mon` is "the 1st, *and* every Monday", not "Mondays that fall on the
1st". That is Vixie cron's rule and what `crontab(5)` documents. When only one of
the two is restricted, that one decides alone.

**Quartz syntax is refused, not misread.** `?`, `L`, `W`, `#`, and six-field
(seconds) expressions produce an error naming the character. A schedule copied
from a Quartz example that quietly meant something else is the worst possible
outcome for something that spends GPU time. (`jul` and `wed` are fine — the
check looks for Quartz *shapes*, not for the letters.)

**`@reboot` is deliberately absent.** "Fire because the process started" is
exactly the wake-up stampede the misfire policy exists to control. Write a real
schedule and choose `misfire = "run_once"` if you want one catch-up run on wake.

**Editing a schedule takes effect on the next tick.** The scheduler remembers
when each job is next due, and that cursor is written to disk so a restart does
not lose it. A cursor the *current* expression would never produce — because you
changed `0 3 * * *` to `*/15 * * * *` and restarted — is discarded and rebuilt
from the file, and the old schedule's occurrences are not counted as missed. The
file always wins over a cursor it did not write.

**A schedule that can never fire is a load error.** `0 0 30 2 *` (30 February)
and any schedule whose occurrences never fall inside its window — say
`0 12 * * *` with `window = "22:00-06:00"` — are refused when the file is read,
naming the job. Finding that contradiction at startup beats discovering it after
a week of nothing happening.

### Timezone and DST

`timezone = "local"` reads the machine's own zone, because an operator who
writes `0 3 * * *` means 3am where the machine is. Two decisions follow, and
both are made rather than discovered:

- **Spring forward.** A job whose wall-clock time falls inside the skipped hour
  has no instant to run at, so **it does not run that day**. It is not moved to
  03:00 and it is not run twice the next day.
- **Fall back.** A job whose wall-clock time falls inside the repeated hour runs
  at the **first** of the two, once. Running twice would double that job's cost
  for one night a year on hardware somebody lent you.

`timezone = "utc"` has no transitions and neither surprise. Named zones like
`Europe/Berlin` are refused: they would need a bundled timezone database this
plugin does not ship, and silently treating one as UTC would be worse.

---

## Misfire policy

The question nobody asks until it bites: **the node was asleep, off, or busy
when a job was due. What now?**

Three answers are possible. This plugin picks the middle one and does not offer
the third:

| Policy | Behaviour | Available? |
| --- | --- | --- |
| `skip` | Wait for the next scheduled occurrence. | Yes |
| `run_once` | **One** catch-up run, if the missed occurrence is still fresh. Default. | Yes |
| run every missed occurrence | Replay the backlog. | **No, and there is no setting for it** |

**Why `run_once` is the default.** A laptop that wakes after a week owes an
`@hourly` job 168 runs. Delivering them means 168 completions queued at once on
a machine that just came out of sleep — that is a way to melt hardware somebody
lent you, and it is a self-inflicted denial of service on the node's own users.
It is also pointless: by the time it wakes, 167 of those answers describe hours
nobody is going to read about. One run, now, covering the gap, is what the
operator actually wanted.

**Why "run every missed occurrence" does not exist.** Not as a discouraged
setting, not behind a flag. Any bound that made it safe (a cap on the backlog, a
delay between replays) is a worse version of `run_once`, and any operator who
genuinely wants N runs can write a schedule that produces N occurrences.
`Misfire::parse` rejects `run_all` with a message pointing here.

**Freshness.** A catch-up run happens only if the missed occurrence is younger
than `catch_up_grace_secs` (one hour by default). A 03:00 digest discovered at
09:00 because the laptop was shut is not run: a stale answer delivered six hours
late is usually worse than no answer, and the skip is recorded so the gap is
visible rather than silent. `catch_up_grace_secs = 86400` for a job that is
worth running whenever the machine comes back; `misfire = "skip"` for one that
is worthless late.

**What counts as late.** The scheduler wakes every `--tick-secs` (20 by
default), so a perfectly healthy job is always found a moment after it came due.
Anything more than 90 seconds late, or with another occurrence already behind
it, is treated as a misfire. `list` and `history` report
`totals.missed_occurrences` so the size of a gap is a number rather than an
impression.

---

## Hours, concurrency, and overlap

Three separate limits, because they fail differently.

**A job never overlaps itself.** If the previous run is still going when the
next occurrence arrives, that occurrence is **skipped** — not queued. A job that
consistently takes longer than its interval therefore runs back to back at most,
never in an ever-growing pile.

**`max_concurrent_runs` caps every job together**, at 1 by default. An
occurrence that cannot get a slot is shed and recorded as `skipped_busy`.
Nothing waits for a permit, because a queue is how a slow evening turns into a
burst of work at midnight.

**A `window` confines runs to hours the operator chose.** `22:00-06:00` wraps
past midnight, start inclusive and end exclusive. A job due outside its window is
skipped, not deferred: the operator wrote both the schedule and the window, and
deferring a noon job to 22:00 delivers an answer they did not ask for. If the
two genuinely never coincide, the file does not load at all.

The window is read on the node's clock, and `run_now` honours it. The only way
to run a job outside its hours is `manual_ignores_window = true` on that job, in
the file — which is the point: a tool argument cannot widen it, because a model
can call the tool.

Skips are counted by reason rather than listed one by one. A half-hourly job
with an overnight window is skipped 32 times on an ordinary day; writing each of
those into the run history would push every real run out of it within a day. So
`list` and `history` carry `skips_by_reason` and the most recent `last_skip`,
and the detailed history is for attempts that actually spent time:

```jsonc
"skips_by_reason": { "skipped_window": 32, "skipped_overlap": 1 },
"last_skip": {
  "at_ms": 1786897500000,
  "code": "skipped_window",
  "detail": "this job may only run inside 22:00-06:00, and it is not that time on this machine…"
}
```

Being switched off in the file, or paused, does not count as a skip. Those are
standing states that `list` already reports; counting one every twenty seconds
would turn the skip counters into a measure of uptime.

---

## Failure, backoff, and quarantine

A run fails if the endpoint refuses it, times out, returns something that is not
a completion, returns an **empty** completion, or if the sink rejects the answer.
A completion that could not be delivered is a failed run, not a partial success:
from the operator's side nothing arrived where they asked for it.

Failures back off; they do not retry hot. Delay is exponential with full jitter,
starting at one minute and capped at one hour, and every occurrence that comes
due inside the delay is skipped as `backing_off`. `run_now` is refused during a
backoff too — otherwise the one gate that protects a broken endpoint would be
bypassable by anything that can call a tool.

A success clears the streak and the delay together.

After `quarantine_after_failures` consecutive failures (10 by default) the job
**parks itself**: `list` shows it paused with reason `quarantined` and a note.
Fix the cause and call `resume`, which clears the quarantine *and* the backoff so
the next occurrence runs. A restart clears it too — the quarantine is a runtime
state, and the file is the intent.

Delivery is not retried inside a run. One attempt, one outcome; the job's own
backoff is the retry, and it does not hold a concurrency slot while it waits.

---

## What is recorded, and what is not

Three layers, so the file is bounded however often a job fires:

- **The last `history_per_job` runs**, in full: when, how long, the outcome, a
  stable code, the trigger, token counts, and the first line of any error.
- **Lifetime totals per job** that never age out — attempts, successes,
  failures, skips, missed occurrences, total duration, completion tokens, last
  success, last failure. So "has this ever worked?" survives long after the run
  that answers it has aged out.
- **Skips as counters** keyed by reason, plus the most recent one in full.

Everything lives in one JSON file in the state directory, written atomically
through a temp file and a rename, so an interrupted write leaves the previous
state intact.

**The model's output is not stored here.** Not the completion, not a preview of
it. A run record carries how many characters were produced and where they were
delivered. Output is the part most likely to be sensitive and the part most
likely to be large, and the sink is where the operator already decided it should
live.

**A pause is not stored either.** `pause` is `#[serde(skip)]` in the state file
on purpose: restarting the node restores the jobs file's intent. Failure backoff
*is* stored, because that is a measurement rather than an intent — a plugin that
forgot it on every restart would retry a broken job hot every time the host came
back.

A file sink is capped at 8 MiB. When the next record would cross the cap the
file is rotated to `<name>.1`, replacing any previous `.1`, so disk use per sink
is bounded at roughly twice the cap forever, with no cron job and no operator
action.

---

## Using the tools

`list`:

```jsonc
{
  "count": 2,
  "timezone": "local",
  "max_concurrent_runs": 1,
  "jobs": [
    {
      "id": "nightly-digest",
      "schedule": "0 3 * * *",
      "window": null,
      "model": "qwen3:8b",
      "sink": "file:digests/nightly.md (text)",
      "enabled": true,
      "paused": null,
      "running": false,
      "misfire": "run_once",
      "next_due_utc": "2026-03-02T02:00:00Z",
      "next_due_local": "2026-03-02T03:00:00+01:00",
      "due_in_secs": 41820,
      "consecutive_failures": 0,
      "last_run": { "outcome": "success", "duration_ms": 4210, "completion_tokens": 412, … },
      "totals": { "attempts": 31, "succeeded": 30, "failed": 1, "skipped": 4, … }
    }
  ],
  "note": "Jobs are declared only in the jobs file. No tool in this plugin can create, edit, or delete one…"
}
```

`run_now` names one job and waits up to 45 seconds:

```jsonc
{ "job_id": "nightly-digest" }
```

```jsonc
{
  "id": "nightly-digest",
  "status": "finished",
  "outcome": "success",
  "code": "ok",
  "duration_ms": 4210,
  "output_chars": 1180,
  "completion_tokens": 412,
  "sink": "file:digests/nightly.md"
}
```

A longer run keeps going in the background and answers `"status": "running"`
with a pointer to `history` — a tool call that held the control connection open
for an hour would be a broken tool.

Refusals name the guard and what to do about it, rather than failing quietly:

```text
nightly-digest cannot run now: this job may only run inside 22:00-06:00, and it
is not that time on this machine. The window is the operator's statement about
when this hardware works.
```

Stable outcome codes, which callers may key on: `ok`, `endpoint_error`,
`delivery_error` for runs; `skipped_window`, `skipped_overlap`, `skipped_busy`,
`skipped_misfire`, `skipped_stale`, `backing_off` for skips; `disabled` and
`paused` for standing states.

---

## Configuration

There is no `[plugin.settings]` block for this plugin, and that is deliberate.

`[plugin.settings]` values are stored by the host and rendered by the console,
but they are **never delivered to the plugin process** — there is no settings
field in the launch contract or the initialize handshake, and only a web UI
bundle can read them back. This plugin ships no web UI, so a config schema would
draw console controls that could not move a single job. Everything therefore
comes from the two channels a plugin process actually has, plus the jobs file.

| Setting | `[[plugin]].args` | Environment | Default |
| --- | --- | --- | --- |
| Jobs file | `--jobs <path>` | `TDCC_SCHEDULED_PROMPTS_JOBS_FILE` | `$HOME/.tdcc/scheduled-prompts.toml` |
| State directory | `--state-dir <path>` | `TDCC_SCHEDULED_PROMPTS_STATE_DIR` | `<plugin store>/scheduled-prompts/state` |
| Output root | `--output-dir <path>` | `TDCC_SCHEDULED_PROMPTS_OUTPUT_DIR` | `<state-dir>/out` |
| Endpoint | `--endpoint <url>` | `TDCC_SCHEDULED_PROMPTS_ENDPOINT`, then `[[plugin]].url` | `http://127.0.0.1:9337/v1` |
| Allow a remote endpoint | `--allow-remote-endpoint` | `TDCC_SCHEDULED_PROMPTS_ALLOW_REMOTE_ENDPOINT=true` | refused |
| Scheduler tick | `--tick-secs <5-300>` | `TDCC_SCHEDULED_PROMPTS_TICK_SECS` | `20` |
| Endpoint API key | — *(environment only)* | `TDCC_SCHEDULED_PROMPTS_API_KEY` | none |

`args` wins over the environment, which wins over `[[plugin]].url`, which wins
over the built-in default. An unrecognised flag or an out-of-range number is a
**hard startup error**, not a warning: a typo in `--allow-remote-endpoint` that
was quietly ignored would leave you believing a guard was off when it was on.

**The API key is environment-only, on purpose.** `args` is written into
`~/.tdcc/config.toml`, echoed back by `tdcc plugins info`, and visible in a
process listing on most systems. The `ApiKey` type also hand-writes its `Debug`
implementation so an accidental `{:?}` cannot print it.

```toml
version = 1

[[plugin]]
name = "scheduled-prompts"
enabled = true
args = ["--jobs", "/home/you/.tdcc/scheduled-prompts.toml", "--output-dir", "/srv/digests"]
```

### Starting up

Three outcomes, each with a different posture, all reported on stderr and by
`status`:

| Situation | Behaviour |
| --- | --- |
| No jobs file | Starts with zero jobs. Installing an unconfigured plugin must never change what a node does. |
| A file that loads | Its jobs are scheduled, and the summary line says how many. |
| A file that does not load | **Nothing runs.** The scheduler does not start and every tool reports the error. Running the half of a schedule that happened to parse is the worst of the three. |

---

## Blast radius

This runs on hardware somebody else paid for. Every widening below is opt-in.

**Network — outbound, two shapes, no listener.** One `POST` per run to the
configured OpenAI-compatible endpoint, and, for a webhook sink, one `POST` per
run to the URL in the environment variable that job named. Nothing else. The
host owns HTTP and MCP; this plugin opens no socket.

**The endpoint must be on loopback** unless the operator passes
`--allow-remote-endpoint`. The default is the node's own API on
`127.0.0.1:9337`, so out of the box no prompt leaves the machine. A URL carrying
credentials, a query, or a fragment is refused at startup, and redirects are
never followed — an endpoint that answers `302` is a misconfiguration, and
following it would send the operator's prompts somewhere they never named.

**Webhook URLs are treated as credentials.** They come from the environment, are
never rendered in full anywhere — `list`, `status`, `history`, log lines, and
error messages all show `https://hooks.slack.com/[redacted] via
TDCC_…_WEBHOOK_ALERT` — and a failure body quoted back from the endpoint is
scrubbed of the URL, its path, its query, and any path segment long enough to be
a token before it is stored or returned. Slack answers `invalid_token for
/services/T0/B0/…`, quoting your token straight back at you; that is a real leak
this plugin has a test for.

**Filesystem — two directories, both operator-named.** Run history in the state
directory, and file sinks strictly beneath the output directory. Sink paths are
built from plain names by construction, and the write path independently refuses
any segment that could escape and then verifies the resolved parent is inside the
canonicalized root, which is what catches a symlink out of the tree. Nothing else
is read, written, or created anywhere.

**Subprocesses:** none. This plugin spawns nothing.

**Mesh:** nothing declared, so by the host's allowlist nothing can arrive. A
schedule is this machine's own business; it is not gossiped and no peer can
influence it.

**Bounded everywhere a caller could grow something.** 64 jobs; 32,768-character
prompts; 8 MiB read from the endpoint per completion; 8 MiB per sink file before
rotation; `history_per_job` detailed runs and a fixed set of skip counters per
job; 500 counted missed occurrences past which the number is reported as a cap
rather than scanned for.

**What it cannot protect you from:** the prompts themselves. A job runs the
prompt the operator wrote, against the model they named, and writes the answer
where they said. If that prompt asks for something expensive, it is expensive
every time it fires. `list` shows the schedule and `history` shows the duration
and token count of every run, which is the honest way to find out.

---

## Building against the SDK

`tdcc-plugin` is **not published on crates.io under that name** — it was renamed
from `mesh-llm-plugin` and its repository is private — so a version requirement
like `tdcc-plugin = "0.72.1"` will not resolve. `Cargo.toml` here uses a path
dependency into a local `tdcc-mesh` checkout instead:

```toml
tdcc-plugin = { path = "../../../tdcc-mesh/crates/tdcc-plugin" }
```

That path assumes `tdcc-plugins` and `tdcc-mesh` are siblings:

```text
token/
  tdcc-mesh/          # the private host repository, with crates/tdcc-plugin
  tdcc-plugins/       # this repository
    plugins/scheduled-prompts/
```

If your checkout is laid out differently, change that one line, or declare the
dependency as a version requirement and redirect it:

```toml
[patch.crates-io]
tdcc-plugin = { path = "/absolute/path/to/tdcc-mesh/crates/tdcc-plugin" }
```

**Once the SDK is published**, a public consumer replaces the path dependency
with a version pin matching the `tdcc` release they target:

```toml
tdcc-plugin = "0.72.1"
```

Nothing else changes — no code here depends on the dependency being local. Pin
an exact version: the initialize handshake requires an exact protocol-version
match, so a host and a plugin built against mismatched protocol versions refuse
to connect at startup rather than misbehaving later.

```bash
cargo build --release
```

The first build downloads a vendored `protoc` for `tdcc-plugin`'s `prost-build`
step; no system protobuf compiler is needed. TLS is rustls with bundled roots, so
no OpenSSL headers are needed either.

---

## Tests

```bash
cargo test
```

141 tests, no network beyond `127.0.0.1` and no model. Roughly:

| Area | Tests | What they pin |
| --- | --- | --- |
| `decide` | 21 | The whole decision table: due, not due, overlap, backoff, window, both misfire policies, manual triggers, and the codes and messages callers key on |
| `cron` | 18 | Parsing and next-occurrence search, including the Vixie day rule, leap day, a schedule that can never fire, and every refusal message |
| `jobs` | 18 | File validation: unknown keys, ranges, duplicate ids, path confinement, the webhook-URL-in-the-file refusal — and that the example jobs file in this README still loads |
| `scheduler` | 14 | Tick behaviour, pause/resume semantics, quarantine, and the answers the tools give |
| `sink` | 13 | Text and JSONL rendering, appending, rotation, the second confinement layer, and URL scrubbing |
| `history` | 12 | Bounded history, rollups, atomic persistence, and that a pause never reaches the disk |
| `openai` | 11 | Request shape and every malformed-response message, four of them against a real socket |
| `config` | 12 | Precedence, the loopback guard, that `--help` survives a broken endpoint, and that the key never appears in `Debug` |
| `clock` | 9 | Zones, windows, and the naive-to-instant mapping |
| `manifest` | 5 | The declared surface, including that **no tool name contains `create`, `add`, `edit`, `delete`, …** |
| `end_to_end` | 5 | A due job → a real HTTP request → a real file; no overlap; the concurrency cap; a failure that backs off |
| `main` | 3 | Missing, valid, and unloadable jobs files |

The end-to-end tests do not race the clock. A stub endpoint on loopback counts
the requests it serves and **holds** its response until the test releases it, so
"while a run is in flight" is a state the test controls rather than a sleep it
hopes is long enough, and "did it start a second copy?" is answered by a counter
rather than by timing.

What the suite does **not** cover: anything that needs a live host — the
initialize handshake, the MCP and HTTP projections, and health behaviour under
load. Those rest on the checklist in the catalog's CONTRIBUTING.md. The cron
search is checked against an independent matcher minute by minute, but DST
transitions are only exercised as a property (occurrences always move strictly
forward and always land on a matching wall-clock minute) rather than against a
bundled timezone database.

---

## Package and install locally

The archive needs one top-level directory named after the plugin, containing
`plugin.toml` and an executable named exactly `scheduled-prompts`
(`scheduled-prompts.exe` on Windows). This plugin declares neither a config
schema nor a web UI, so its `plugin-manifest.json` is `{}` and may be left out;
`--print-package-manifest` prints it if you want to include it anyway.

macOS and Linux:

```bash
rm -rf target/package
mkdir -p target/package/scheduled-prompts
cp target/release/scheduled-prompts target/package/scheduled-prompts/scheduled-prompts
cp plugin.toml README.md target/package/scheduled-prompts/
tar -C target/package -czf target/scheduled-prompts-0.1.0-local.tar.gz scheduled-prompts

tdcc plugins install --archive ./target/scheduled-prompts-0.1.0-local.tar.gz \
  --name scheduled-prompts --version 0.1.0
tdcc plugins info scheduled-prompts
```

Windows:

```powershell
Remove-Item -Recurse -Force target\package -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force target\package\scheduled-prompts | Out-Null
Copy-Item target\release\scheduled-prompts.exe target\package\scheduled-prompts\scheduled-prompts.exe
Copy-Item plugin.toml, README.md target\package\scheduled-prompts\
Compress-Archive -Path target\package\scheduled-prompts `
  -DestinationPath target\scheduled-prompts-0.1.0-local.zip -Force

tdcc plugins install --archive .\target\scheduled-prompts-0.1.0-local.zip `
  --name scheduled-prompts --version 0.1.0
```

Set `TDCC_PLUGIN_DIR` to an empty directory first if you do not want an
in-development build landing in your real plugin store.

Then write a jobs file, enable the plugin, and start the node:

```bash
tdcc client --port 9337 --console 3131 --config ./config.toml
curl --fail http://127.0.0.1:3131/api/plugins/scheduled-prompts/http/status
```

Running the binary directly, outside a host, fails immediately with
`TDCC_PLUGIN_ENDPOINT is not set for plugin process`. That is correct — the host
owns the control endpoint and passes it in through the launch contract.

---

## Limitations, stated plainly

- **No sub-minute schedules.** Cron resolution is one minute and the scheduler
  wakes every 20 seconds by default, so a job fires within about a minute of its
  nominal time, not on the second.
- **No named timezones.** `local` and `utc` only. A machine that moves between
  zones changes when `local` jobs fire.
- **No dependencies between jobs.** Each job is independent. There is no "run B
  after A", and building one out of two schedules and a shared file is a
  workaround, not a feature.
- **One attempt per run.** A transient webhook `503` loses that run's output —
  the answer is not stored anywhere else, by design. The failure is recorded and
  the job backs off; the next scheduled run is the retry.
- **The prompt is static.** It is exactly what the file says, every time. There
  is no templating, no substitution, and nothing from the previous run is
  carried forward.
- **A run cannot be cancelled.** `pause` stops the *next* occurrence; a run
  already in flight finishes or hits its `timeout_secs`.
- **`run_now` answers within 45 seconds or not at all.** A longer run keeps going
  and its outcome appears in `history`; the tool does not hold the connection.
- **The node's own API is the default endpoint, and it may not be serving.** A
  job naming a model the node cannot route to fails with the endpoint's own
  error, recorded per run. `status` reports the endpoint but does not probe it —
  it is the tool you call when everything else is failing, so it touches no
  network.

---

## Compatibility

These identifiers are a public API. Changing one is a breaking change, because
they are names other people wrote down:

- the capability id `scheduled-prompts.v1`;
- the MCP tool names `list`, `status`, `history`, `run_now`, `pause`, `resume`;
- the HTTP paths `/jobs`, `/status`, `/history`, `/run`, `/pause`, `/resume`;
- the jobs file keys, and `version = 1`;
- the outcome and skip codes listed under [Using the tools](#using-the-tools);
- the environment variable names in [Configuration](#configuration).

The run-history file carries its own version and is refused rather than misread
if it comes from a newer build.

---

## License

Apache-2.0.
