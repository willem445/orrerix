# The webview is not throttled while the window is hidden (#1141)

## The symptom, and the wrong first reading of it

During an unattended run with the monitors off, deliveries landed in a pane's
input box and were never submitted. A report to the orchestrator sat for about
3.5 hours, and the whole backlog went through the moment someone woke the
display. The first reading was that the delivery loop runs on a webview clock
(`setInterval` or `requestAnimationFrame`), and Chromium throttles a hidden
page's clocks.

**That reading is wrong for this tree, and the fix does not act on it.**
Delivery already runs on the backend:

| Step | Where it runs |
| --- | --- |
| Queue drain and its cadence | `run_queue_drainer` (`src-tauri/src/orchestration/mod.rs`), a Rust thread per pane that sleeps `queue::QUEUE_DRAIN_POLL` (2 s) between polls |
| Paste, echo verification, Enter, submit confirmation | `deliver_now`, on that same thread, writing through the pane's own writer thread (`pty.rs`, `spawn_pane_writer`) |
| Hold re-checks (the question gate, box occupancy) | the drainer's polls, reading the backend's own terminal grid (`loomux_engine::termgrid`) built from the output ring |
| The output ring every one of those reads | teed on the PTY reader thread in `pty.rs`, *before* the coalescing pump emits anything to the webview |
| CI-watch notices, the idle tick | `start_gh_poller` and `start_idle_tick`, backend `spawn_tick_loop` threads |

Nothing on that path waits on a frontend tick. `docs/design/pane-render-throttle.md`
already says so in its #813 section: "the backend never reads xterm". So moving
the delivery clock to the backend would have moved nothing. It is already
there.

## What does depend on the webview: the terminal's replies

xterm.js is the terminal the agent CLI is talking to. A CLI asks its terminal
questions: Device Attributes, a cursor-position report (DSR/CPR), OSC 10/11
colours, XTVERSION, and DEC-1004 focus reports. **Only xterm answers them, and
only once it has parsed the query out of the output stream.** That parse is
scheduled on `setTimeout` chains: loomux's own flush timer, and xterm's
`WriteBuffer`, which yields every 12 ms and reschedules. The reply then goes
back down the PTY to the child.

A hidden page's timers are clamped. With the display off or the window fully
occluded, Windows reports the window hidden, and Chromium applies two kinds of
throttling:

- background timer throttling, which aligns timers to 1-second wake-ups;
- Intensive Wake-Up Throttling, which after about 5 minutes hidden allows
  chained timers **one wake-up per minute**.

A parse that needs dozens of timer slices then takes an hour. A CLI waiting on a
query reply stops taking input in the meantime. The backend's paste lands in the
input box and its Enter goes unanswered. When the display wakes, the timers
unclamp, xterm parses the backlog, the replies go out and the CLI catches up,
all at once. That matches the symptom exactly. The xterm half of this chain is
quoted from `node_modules` in the render-throttle note. The Chromium half is
documented platform behaviour, which cannot be cited from this tree.

## The fix: WebView2 browser arguments

`src-tauri/tauri.conf.json` gives the one window `additionalBrowserArgs`:

```
--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection,IntensiveWakeUpThrottling
--disable-background-timer-throttling
--disable-backgrounding-occluded-windows
--disable-renderer-backgrounding
```

(one line in the file). Two traps shape that string, and
`test/webviewthrottle.test.ts` pins both:

1. **Setting it replaces wry's defaults rather than adding to them.**
   wry 0.55's `create_environment` does `additional_browser_args.unwrap_or_else(||
   default_args …)`, so its three disabled features come back on unless they
   are restated. The test lists them.
2. **Chromium keeps only the last occurrence of a repeated switch.** A second
   `--disable-features=` would silently drop the first list. The test requires
   exactly one.

This is also product-generic (constraint 8). It is a statement about how the app's
own renderer behaves, and it holds whichever CLI runs in the panes.

### Why not answer the queries from the backend

A backend responder could answer DA or CPR from `termgrid` while the page is
hidden. It is rejected because the queries are not consumed there. xterm still
parses the same bytes when the page wakes, and answers each one again. The CLI
then receives a second, late reply that it did not ask for, and it reads that
reply as input. That is the class of bug #179 was: a terminal reply misread as
keystrokes. The only way to avoid the duplicate would be to suppress xterm's
replies, which puts two terminals in charge of one reply channel. Keeping the
renderer awake leaves one terminal answering, as it always has.

### Why not a watchdog that re-presses Enter

#871's submit-retry idea does not fix this. The Enter is already written. The
CLI is not reading it because it is blocked on the reply. A retry adds
keystrokes behind the same blockage.

## What it costs

The window no longer idles when hidden. Chained timers keep their normal
cadence, and an occluded window is no longer treated as backgrounded. So xterm
keeps parsing at its visible cadence. The page may also go on reporting
itself visible while occluded, in which case the polled views gated by
`src/pollgate.ts` keep polling too. Which of the two happens under a display-off
is part of the live check. The cost is some CPU and power for as long as the app is
open with the display off. It scales with pane output, and a quiet group costs
little. The user page (`docs/autonomous-mode.md`, *Leaving it running with the
display off*) states this. No figure is claimed, because none was measured
here: this repo cannot turn a monitor off in CI.

The #813 sync-parse hint in `pane.ts` (`hintXtermSyncParse`) stays. It still
covers a minimized window, which is hidden for reasons these switches do not
touch.

## Open items

- **Live validation is the human's.** Leave a delivery queued with the monitors
  off for an hour. Nothing in CI can reproduce display power-off or occlusion.
- **A spawn while the window is hidden.** Opening an agent pane is
  webview-driven: `orch-spawn-request` → `openAgentPane` in `src/orchestration.ts`.
  The backend cancels a spawn that is not bound within `BIND_TIMEOUT` (20 s,
  #106). Event dispatch itself is not a timer, so these switches should cover
  it, but no one has checked whether pane creation runs to completion within
  20 s in a page that stays hidden. This is tracked on #1141, pending the live
  hour, rather than as its own issue.
- **Launch-time policy or environment arguments.** WebView2 also reads
  `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS` and the `AdditionalBrowserArguments`
  policy, and the E2E job sets the policy. How they combine with this config
  value is not established here. Either way the E2E suite is unaffected: it
  never hides the window.
