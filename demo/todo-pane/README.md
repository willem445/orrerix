# To-Do pane — S0 mock

A static mock of the To-Do pane from #3263, driven by fixtures. It exists so
the human can **look at the thing** and approve or redirect the visual language
before S4 builds it.

It is a demo. It is outside `src/`, so `tsc --noEmit`, `vite build`, `npm test`
and `test/theme.test.ts` do not see it — verified below.

## How to open it

```
npm ci                                   # once per worktree
npx vite demo/todo-pane --host 127.0.0.1 # then open the URL it prints
```

Open the URL vite prints, not one from memory: it is `http://127.0.0.1:5173/`
on a free machine, but this directory has no `vite.config` and therefore no
`strictPort`, so a busy 5173 rolls silently to 5174 and up.

**Pass `--host 127.0.0.1`.** Without it, vite here binds IPv6 loopback only —
`[::1]:5173` answers 200 and `127.0.0.1:5173` refuses the connection — so a
browser that resolves `localhost` to IPv4 loads nothing at all, with no error
worth reading. With the flag, `127.0.0.1`, `[::1]` and `localhost` all answer,
and so do the fixtures. `--host localhost` works too.

**It is not the app's port 1420.** `npx vite demo/todo-pane` makes *this
directory* the vite root, and there is no `vite.config` in here, so the
repo-root config — which pins `port: 1420, strictPort: true` for the app —
never loads. Pass `--port` to pin one yourself.

**A plain `file://` open does not work.** Chrome refuses the module script
before any fixture is requested:

```
Access to script at '.../main.js' from origin 'null' has been blocked by CORS
policy: Cross origin requests are only supported for protocol schemes: chrome,
chrome-untrusted, data, http, https.
```

Any static server works; `npx vite` is just the one already in the repo's
devDependencies. If you open the page and see a `[demo]` message where the list
should be, that is this failure, and the message says so.

## What to look at

The controls are in the bar across the top: which fixture, the **clock**
(09:00 / 14:00 / 23:00 — it really does change what "Today" and "overdue"
mean, because nothing under here reads the wall clock), Reset, and a
**Motion on/off** toggle that shows the reduced-motion reading without
touching an OS setting.

Two cells are shown side by side on purpose, driven from **one** state object:
a ~320 px grid cell and a full tile. The pane has to read at both, which is why
a row's detail expands **inline** rather than into a side panel.

| | |
|---|---|
| `?fixture=mixed` | the default — global + workspace, agent-authored rows, one overdue |
| `?fixture=empty` | the empty state, which **is** the quick-add bar |
| `?fixture=busy` | 500 items — scroll it, then type in the search field |
| `?view=planned` | the date buckets, Overdue first and in the attention dye |
| `?scope=global` | the other list the scope switch selects |

**The quick-add bar is the centrepiece.** Type into it and watch the chips:

```
ship the release notes tomorrow at 4pm #release !! *
    -> Tomorrow 16:00 | #release | !! | Important, and the title keeps
       exactly the words you meant
Friday retro notes
    -> no chips. A line that OPENS with a weekday is a title, not a date.
```

The chips are the parse, shown **before** Enter. A word that silently became a
due date is the failure mode that makes a quick-add bar untrustworthy, so
`quickadd.js` never consumes a token it did not understand and shows you every
token it did.

**Keyboard**, which is the point of the pane — `DESIGN.md` §6 is the full map:
`n` add, `/` search, `j`/`k` move, `space` complete, `e` details, `i`
important, `t` My Day, `u` undo, `g` scope, `1`–`5` views, `Alt+Up`/`Alt+Down`
reorder, `Esc` out.

## What is in here

| file | what it is |
|---|---|
| `index.html` | the page: two cells and the demo's own control bar |
| `todo.css` | the design. The token block at the top is copied from `src/styles.css` |
| `quickadd.js` | **the natural-language parser.** Pure, clock injected — S3 lifts this into `src/todoquickadd.ts` |
| `render.js` | `project()` (pure: state → view-model) and `renderInto()` (view-model → DOM) |
| `main.js` | demo controls, keyboard and the undo stack. Scaffolding — nothing downstream inherits it |
| `fixtures/*.json` | `mixed`, `empty`, `busy` |
| `fixtures/make.mjs` | generates `busy.json`: `node demo/todo-pane/fixtures/make.mjs` |
| `DESIGN.md` | the visual-language decisions, for S4 to inherit |

A fixture's `due` is a **day offset plus an optional wall time**
(`{"day": -1, "hm": "17:00"}`), resolved against the demo clock rather than
pinned to a calendar date. A mock authored with absolute dates reads as
entirely overdue a fortnight after it was written, which would teach a reviewer
exactly the wrong thing about the overdue dye.

`make.mjs` carries its own small LCG rather than calling `Math.random`, so
regenerating `busy.json` produces a byte-identical file and a diff on it means
someone changed the generator.

## Nothing in here reaches the app

`demo/` is outside every source root the build and the suites walk, so this
directory cannot affect either.

`npm run build` (which is `tsc --noEmit` then `vite build`) is green, and
`npm test` reports `tests 3322 / pass 3311 / fail 2`. **Those two failures are
pre-existing and are not this directory's**: they are `test/sshcommand.test.ts`'s
two `real cmd.exe` cases, which spawn a real `cmd.exe` and get an empty stdout
in this sandboxed worktree. The control is in the PR body — the same two fail,
and only those two, with `demo/todo-pane/` moved out of the repository entirely.

And the bundle really does contain nothing from here. Every distinctive string
this directory owns was searched for in a freshly-built `dist/`:

```
todo-pane        exit 1
parseQuickAdd    exit 1
plannedBucket    exit 1
ROW_BUDGET       exit 1
attr-dot         exit 1
quickadd         exit 1
```

Exit 1 is grep's "no match". A sweep whose success shape is zero needs a
positive control, so the same sweep was run for a string that IS in the bundle
(`sidedock`) and it matched three files at exit 0. Both runs, with their exact
command lines, are in the PR body.

## What S4 inherits, and what it does not

`quickadd.js` and `render.js`'s `project()` are written to be lifted — pure, no
DOM, no wall clock, no module-level state. `main.js` is not: it is a demo
player, and S4's `TodoPaneView` owns the real wiring (`todo-changed` events, a
`CoalescingRefresh`, per-viewer state in `localStorage`).

What `main.js` **does** demonstrate, and what S4 should keep, is the shape: one
state object, the DOM rebuilt from it wholesale, and no un-submitted value ever
living in an element. That is `CLAUDE.md`'s in-list-editor rule, and the board
learned it the hard way — a re-render triggered by an agent's write must not be
able to eat a half-typed row.
