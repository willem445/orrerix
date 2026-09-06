# Structured agent pane — S0 mock

A static, animated mock of the structured pane from #2891, driven by a scripted
pi-shaped event stream. It exists so the human can **look at the thing** and
approve or redirect the visual language before S2 and S4 build it.

It is a demo. It is outside `src/`, so `tsc --noEmit`, `vite build`, `npm test`
and `test/theme.test.ts` do not see it — verified: `npm run build` is green and
`dist/` contains no string from this directory.

## How to open it

```
npm ci                                          # once per worktree
npx vite demo/structured-pane --host 127.0.0.1  # then open the URL it prints
```

Open the URL vite prints, not one from memory: it is `http://127.0.0.1:5173/`
on a free machine, but this directory has no `vite.config` and therefore no
`strictPort`, so a busy 5173 rolls silently to 5174 and up.

**Pass `--host 127.0.0.1`.** Without it, vite here binds IPv6 loopback only —
`[::1]:5173` answers 200 and `127.0.0.1:5173` refuses the connection — so a
browser that resolves `localhost` to IPv4 loads nothing at all, with no error
worth reading. With the flag, `127.0.0.1`, `[::1]` and `localhost` all answer,
and so do the fixtures. `--host localhost` works too.

**It is not the app's port 1420.** `npx vite demo/structured-pane` makes *this
directory* the vite root, and there is no `vite.config` in here, so the
repo-root config — which pins `port: 1420, strictPort: true` for the app — never
loads. Pass `--port` to pin one yourself.

**A plain `file://` open does not work.** Chrome refuses the module script
before any fixture is requested:

```
Access to script at '.../main.js' from origin 'null' has been blocked by CORS
policy: Cross origin requests are only supported for protocol schemes: chrome,
chrome-untrusted, data, http, https.
```

Any static server works; `npx vite` is just the one already in the repo's
devDependencies. If you open the page and see a red `[demo]` row instead of a
transcript, that is this failure, and the row says so.

## What to look at

The controls are in the bar across the top: play/pause (or the space bar),
restart, playback speed, which stream, and a **Motion on/off** toggle that
shows the reduced-motion reading without touching an OS setting.

`?at=<seconds>` jumps straight to a moment, which is faster than waiting:

| | |
|---|---|
| `?at=6` | thinking (open, streaming, dim), then tool cards |
| `?at=11` | a shell command that **fails** — it opens itself and shows the test that reddened |
| `?at=15` | a permission request the pane is waiting on: the warp goes amber and pulses |
| `?at=27` | that request settled, a human steer arriving, a retry, and a select dialog |
| `?at=40` | the turn receipt and the compaction seam |

Then switch the stream to **storm** and let it run — 461 tool calls, of which
one heavy `cargo check` and 460 rapid reads and greps. That fixture exists to
show what happens under load rather than to describe it: the `cargo check` card
emits 156,530 bytes and states the 90,994 it dropped from the head; the
transcript passes 400 rows and states how many rolled out of the pane buffer.
Both numbers are on screen.

Two panes are shown side by side on purpose. The signature element only
resolves into a picture when panes are tiled — one pane cannot show it.

## What is in here

| file | what it is |
|---|---|
| `index.html` | the page: two panes and the demo's own control bar |
| `pane.css` | the design. The token block at the top is copied from `src/styles.css` |
| `decode.js` | **pi RPC wire events → `HarnessEvent`.** The seam S1b replaces with `pi.rs` |
| `render.js` | **`HarnessEvent` → DOM**, plus `projectText()`, the text projection S2 owns |
| `icons.js` | authored SVG marks in the geometry `src/icons.ts` already ships |
| `main.js` | playback. Scaffolding — nothing downstream inherits it |
| `fixtures/*.jsonl` | the two streams, in pi's own wire shapes |
| `fixtures/make.mjs` | generates them: `node fixtures/make.mjs` |
| `DESIGN.md` | the visual-language decisions, for S2/S4 to inherit |

The fixtures are **pi's own RPC shapes**, not a convenient invention, so
`decode.js` is doing the real mapping work rather than reading a format written
to be easy. The renderer never sees a pi field name.

## How this squares with the ring

R1's `harness-adapters.md` §5.1 rendered a structured pane as **VT bytes into
the xterm ring** and rejected a DOM transcript, because a DOM view would break
`get_output`, replay, thumbnails and `last_exit_tail`. #2891 then asked for "a
designed surface … not an xterm emulation of a chat log", which a VT renderer
cannot give you — it cannot draw a fold or a button.

**S1a settles it** in PR #2942 — *not merged yet*, so `main`'s §5.1 still
carries R1's text and this paragraph is dated to that PR. As #2942 writes it,
§5.1 becomes *"Two projections, one log"*: the VT projection keeps feeding the
ring so all four consumers are untouched, and the human gets a DOM renderer in
the same grid cell fed by `orch-pane-event`. Its argument, in the design note's
own words — "keeping the ring and putting the human on a DOM surface are not
alternatives, because the ring is fed from the log rather than from the screen."

So this mock is the DOM projection, and `projectText()` in `render.js` is the
text one — both derived from the same event log, which is what makes them agree
by construction rather than by discipline. `DESIGN.md` §8 carries the detail.
