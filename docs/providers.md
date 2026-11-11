# Where the figures come from

Each page on a display shows one subscription's allowances, labelled with where the figures came from (`docs/protocol-v1.md`, "Data quality"). This page records, for each subscription, what is documented, what Display Hub uses, and how each adapter was checked.

**The rules:**

- No browser cookies.
- No session tokens copied from another app.
- No undocumented or private endpoints.
- No scraping.
- A share that isn't known is shown as unknown, never as zero.
- Nothing provider-related runs on the Pi. The PC helper (`hubctl`) only forwards what a provider's own tool already reports to its signed-in user on that PC.

Documentation checked on 11/11/2026. Providers change these interfaces; re-check them before relying on a new release.

## Summary

| Subscription | Source in Display Hub | Shows as | Evidence |
|---|---|---|---|
| Claude Max | Claude Code's status line, through `hubctl claude-statusline`. Or typed in. | REPORTED (or MANUAL) | [Claude Code status line docs](https://code.claude.com/docs/en/statusline) |
| ChatGPT Plus | Codex's app server (`account/rateLimits/read`), through `hubctl codex-sync`. Or typed in. | REPORTED (or MANUAL) | [Codex app server docs](https://learn.chatgpt.com/docs/app-server); `codex-rs/app-server-protocol` in [openai/codex](https://github.com/openai/codex) |
| OpenCode Go Plus | Typed in only | MANUAL | [OpenCode Go docs](https://opencode.ai/docs/go/): limits documented, no usage API |

Neither Anthropic nor OpenAI documents an API that returns a consumer subscription's usage. Anthropic's Usage and Cost API and Admin API are for Console organisations with an admin key, not claude.ai Pro or Max accounts. So the PC-side tools above are the only documented route.

## Claude Max: Claude Code's status line

**What is documented:**

- Claude Code runs the command set as `statusLine` in its settings, and passes JSON on stdin.
- For claude.ai Pro, Max and Ultra subscribers, after the first API response in a session, that JSON has:
  - `rate_limits.five_hour.used_percentage` and `rate_limits.seven_day.used_percentage`, each "from 0 to 100";
  - `resets_at` for each, in Unix epoch seconds.
- Each window may be missing. Claude Code drops a window once its `resets_at` passes.
- API-key users get no `rate_limits`.
- Updates are debounced at 300 ms. "If a new update triggers while your script is still running, Claude Code cancels the in-flight script." Output shows only after exit code 0.

**What `hubctl claude-statusline` does:**

1. Reads at most 256 KiB of the JSON.
2. Prints a short line at once, such as `Opus 4.7 · 5h 42% · 7d 18%`, and exits 0. If the JSON is unreadable, it prints a blank line rather than failing.
3. Maps `five_hour` to the five-hour window and `seven_day` to the weekly window.
   - A share outside 0–100 becomes unknown.
   - A missing window is left out.
   - A missing `rate_limits` sends nothing.
4. At most once a minute, it queues the figures in an owner-only file and starts `hubctl send-pending` as a separate, detached process: its own process group on Unix, no window on Windows. Claude Code therefore never waits on the network, and cancelling the status line doesn't cancel the send.
   - It sends when the figures change, or every five minutes while Claude Code is in use, so the page doesn't go stale for no reason.
5. The hub stores the figures as `provider_reported`, with the time Claude Code reported them, on every Claude Max page whose figures come from "PC helper". They are stale after 30 minutes, so a display shows plainly when the PC has been off.

**Set-up**, on the PC where you use Claude Code:

1. In the manager, open **Settings**, then **PC helper**. Tick **Claude Max (Claude Code)**, choose **Create helper string**, then **Copy helper string**.
2. Run `hubctl login` and paste the string when asked. It works once, within 10 minutes.
3. In Claude Code's settings (`~/.claude/settings.json`, which is `%USERPROFILE%\.claude\settings.json` on Windows), add:

   ```json
   { "statusLine": { "type": "command", "command": "hubctl claude-statusline" } }
   ```

   Use the full path to `hubctl` if it isn't on PATH. On Windows, write it with forward slashes, such as `C:/Tools/hubctl.exe claude-statusline`, because Claude Code runs it through Git Bash. If you already have a status line script, keep it, and pipe the same input into `hubctl claude-statusline --quiet`, which forwards without printing.
4. In the manager, set a Claude Max page's figures to **PC helper**.

`hubctl status` shows the hub, what this PC reports and the last few sends.

**Verification report (11/11/2026):**

- **Unit tests:** the documented example JSON is parsed; missing windows, an impossible share and non-JSON input are handled; and the throttle works (`apps/hubctl/src/claude.rs`).
- **End-to-end test:** a real hub over pinned HTTPS, with the helper invited, logged in, queuing and sending. The figures reach the page as REPORTED. A second update within a minute sends nothing. After the helper is removed, its sends are refused and logged (`apps/hubctl/tests/end_to_end.rs`).
- **Real binaries on Linux:**
  - `hubd pair-helper`, then `hubctl login` from stdin with GNOME Keyring;
  - `echo <JSON> | hubctl claude-statusline` printed in 4 ms;
  - the detached sender delivered, and `hubctl status` showed it.
- **Not yet tried:** with a real Claude Code session, or on Windows. The JSON shape comes from the documentation, not from a live session.

## ChatGPT Plus: Codex's app server

**What is documented:**

- `codex app-server` speaks JSON-RPC 2.0 over stdio, one JSON object per line, without the `"jsonrpc"` member.
- A client sends `initialize` (with `clientInfo`), then the `initialized` notification.
- `account/rateLimits/read` returns `rateLimits` with optional `primary` and `secondary` windows, each holding:
  - `usedPercent`: an integer, 0–100;
  - `windowDurationMins`;
  - `resetsAt`: Unix seconds.

  The response may also carry `rateLimitsByLimitId`, `planType` and other fields.
- It needs Codex signed in with ChatGPT. Signed out, or with an API key, it answers with an error such as "chatgpt authentication required to read rate limits". That wording comes from the source code, not the documentation.

**What `hubctl codex-sync` does:**

1. Starts `codex app-server`. Pass `--codex <path>` if it isn't on PATH. On Windows, the npm install's `codex.cmd` shim is started through `cmd.exe`, with fixed arguments.
2. Sends the handshake and `account/rateLimits/read`, skipping notifications, with a 30-second limit.
3. Stops Codex.
4. Matches windows by length only:
   - 300 minutes is the five-hour window;
   - 10,080 minutes is the weekly window.

   A window of any other length, or with no length, is left out rather than guessed.
5. Sends the figures as ChatGPT Plus, and logs the result. `--dry-run` shows the figures without sending them.

**Set-up**, on the PC where you use Codex:

1. Do the same as steps 1–2 for Claude Code, ticking **ChatGPT Plus (Codex)**. One helper can report both.
2. Check it works: `hubctl codex-sync --dry-run`.
3. Run it regularly with **Task Scheduler**:
   1. Choose **Create Task**.
   2. Leave **Run only when user is logged on** selected, so it can reach your Credential Manager.
   3. Add a trigger **At log on**, repeating every 15 minutes indefinitely.
   4. For the action, choose **Start a program**: `hubctl-sync.exe`. It's the same as `hubctl codex-sync`, without a console window. Add `--codex <path>` if needed.
4. In the manager, set a ChatGPT Plus page's figures to **PC helper**. Collector figures are stale after 30 minutes, so a 15-minute schedule keeps the page fresh while the PC is on.

**Verification report (11/11/2026):**

- **Unit tests:** the documented exchange against a stand-in server on a pipe. They cover the handshake order, a notification being skipped, extra fields being ignored, window matching, the "not signed in" error and a timeout (`apps/hubctl/src/codex.rs`).
- **End-to-end test** (Linux): a stand-in `codex app-server` script answering as documented. `codex-sync --dry-run`, then a real send, reached the ChatGPT Plus page as REPORTED (`apps/hubctl/tests/end_to_end.rs`).
- **Not yet tried:** with the real Codex CLI, or with Task Scheduler on Windows. Codex's protocol is changing quickly. If a release renames these fields, `codex-sync` reports that it doesn't understand the answer, and sends nothing.

## OpenCode Go Plus: typed in

OpenCode documents the Go plan's limits as dollar amounts per model:

- the 5-hour window is 20% of the monthly limit;
- the weekly window is 50%;
- the monthly window is 100%.

You track usage in its web console. Each API response carries a per-request `opencode.allowance` figure. But nothing documented reports the running totals, and no local file has them. So the page takes figures typed into the manager (**Pages**, **Edit**), shown as MANUAL, and stale after a day. The manager doesn't offer "PC helper" for it.

## Typed in, for any subscription

Any page can take figures typed in from the provider's own usage page. Leave a share empty if you don't know it, and the display shows "unknown", never 0 %. Reset times are entered as "3 h 20 min" and measured on the hub's clock.
