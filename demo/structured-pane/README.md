# Structured agent pane — S0 mock

A static, animated mock of the structured pane from #2891, driven by a scripted
pi-shaped event stream. It exists so the human can **look at the thing** and
approve or redirect the visual language before S2 and S4 build it.

It is a demo. It is outside `src/`, so `tsc --noEmit`, `vite build`, `npm test`
and `test/theme.test.ts` do not see it — verified: `npm run build` is green and
`dist/` contains no string from this directory.

## How to open it

```
npm ci                          # once per worktree, if you have not already
npx vite demo/structured-pane   # then open the URL it prints
```

**A plain `file://` open does not work.** That is measured, not assumed —
Chrome refuses the module script before any fixture is even requested:

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

Then switch the stream to **storm** and let it run. That fixture exists to show
what happens under load rather than to describe it: one command emits ~140 KiB
and the card states the 90,994 bytes it dropped from the head; the transcript
passes 400 rows and states how many rolled out of the pane buffer. Both numbers
are on screen.

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

## What this mock does not settle

`doc/design/harness-adapters.md` §5.1 renders a structured pane as **VT bytes
into the existing xterm ring**, and rejects a DOM transcript. #2891 then asked
for "a designed surface … not an xterm emulation of a chat log". Both cannot be
fully true, and **S1a owns that contract**, not this mock. `DESIGN.md` states
the three ways out. Everything here is a picture of the raised bar, so the
choice can be made against something visible.
