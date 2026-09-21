// The To-Do pane's scope/refresh race (#3263 S5), pulled out of `todopane.ts`
// so it can be driven rather than reasoned about (#3293 round 6 residual 3).
//
// WHAT EVERY TEST BELOW IS REALLY ABOUT. The engine resolves an
// `update`/`complete`/`delete` by id with NO scope check, so a frame in which
// one scope's rows are painted under the other scope's header is a frame in
// which a click writes to the store the header says it is not looking at. That
// is the failure each arm here prevents, and it is why "it corrects itself a
// round-trip later" is not good enough.

import { test } from "node:test";
import assert from "node:assert/strict";
import {
  accepts,
  beginRead,
  initialScope,
  isLoaded,
  readFailed,
  readLanded,
  scopeChanged,
} from "../src/todoscope.ts";

const WS = "C:/proj/thing";

test("a fresh state has looked at nothing, whatever scope it is on", () => {
  for (const root of [null, WS]) {
    const s = initialScope<string>(root);
    assert.equal(s.root, root);
    assert.equal(s.snapshot, null);
    assert.equal(s.loadFailed, false);
    assert.equal(isLoaded(s), false, "a fresh pane claimed to have read the list");
  }
});

test("a read that lands on the scope it was asked for is published", () => {
  let s = initialScope<string>(WS);
  const token = beginRead(s);
  s = readLanded(s, token, "workspace rows");
  assert.equal(s.snapshot, "workspace rows");
  assert.equal(isLoaded(s), true);
  assert.equal(s.loadFailed, false);
});

test("THE RACE: a read that lands after the scope moved is dropped", () => {
  // The whole reason a read carries a token: a `TodoSnapshot` has no scope of
  // its own, so it cannot identify itself. Without this the WORKSPACE list
  // paints under a header that says Global.
  let s = initialScope<string>(WS);
  const inFlight = beginRead(s); // a refresh starts against the workspace…
  s = scopeChanged(s, null); //    …the human presses `g`…
  const after = readLanded(s, inFlight, "workspace rows"); // …and it lands.

  assert.equal(after, s, "a stale read was published — and by identity, so the pane repaints");
  assert.equal(after.snapshot, null, "the other scope's rows are on screen");
  assert.equal(isLoaded(after), false, "the pane claims to have read a list it has not");
});

test("THE RACE, failure arm: a read that FAILED for a scope we have left says nothing", () => {
  // Flagging the pane "stale" over it would be a warning about a list nobody
  // is looking at — and it would sit there until the next read of the NEW
  // scope happened to succeed.
  let s = initialScope<string>(WS);
  const inFlight = beginRead(s);
  s = scopeChanged(s, null);
  const after = readFailed(s, inFlight);
  assert.equal(after, s, "a stale failure was published");
  assert.equal(after.loadFailed, false);
});

test("a failed read KEEPS the list it had — it never publishes an emptiness", () => {
  // The asymmetry with `scopeChanged` below, and it is the point of having two
  // functions: here the list we hold is still the truth for the scope we are
  // on, so replacing it with nothing would destroy it.
  let s = initialScope<string>(WS);
  s = readLanded(s, beginRead(s), "rows");
  s = readFailed(s, beginRead(s));
  assert.equal(s.snapshot, "rows", "a failed read blanked a good list");
  assert.equal(s.loadFailed, true);
  assert.equal(isLoaded(s), true, "the pane still has a list, and says so");

  // …and a later success clears the flag.
  s = readLanded(s, beginRead(s), "fresher rows");
  assert.equal(s.loadFailed, false);
  assert.equal(s.snapshot, "fresher rows");
});

test("switching scope DROPS the snapshot, with no race needed at all", () => {
  // `todopane.ts` re-renders synchronously on the switch, BEFORE the new read
  // has been asked for. Keeping the old snapshot paints the wrong list for at
  // least a frame, and those rows are live.
  let s = initialScope<string>(WS);
  s = readLanded(s, beginRead(s), "workspace rows");
  s = readFailed(s, beginRead(s));
  const after = scopeChanged(s, null);
  assert.equal(after.root, null);
  assert.equal(after.snapshot, null, "the old scope's rows survived the switch");
  assert.equal(after.loadFailed, false, "the old scope's staleness survived the switch");
});

test("switching to the scope we are already on is a NO-OP, by identity", () => {
  // A redundant call must not blank a list the pane already has — `setScope`
  // recomputes the root on every render path, and `getRoot()` answering the
  // same string twice is the normal case rather than the exception.
  let s = initialScope<string>(WS);
  s = readLanded(s, beginRead(s), "rows");
  const again = scopeChanged(s, WS);
  assert.equal(again, s, "a redundant switch produced a new object");
  assert.equal(again.snapshot, "rows", "a redundant switch blanked the list");
});

test("null and a workspace root are DIFFERENT scopes, both ways", () => {
  // The global list is `null`, not `""` and not absent — so the token compare
  // has to treat it as a value. A guard written as `if (!token)` would accept
  // a global read into a workspace pane and vice versa.
  const global = initialScope<string>(null);
  assert.equal(accepts(global, null), true);
  assert.equal(accepts(global, WS), false);
  const ws = initialScope<string>(WS);
  assert.equal(accepts(ws, null), false, "a global read was accepted into a workspace pane");
  assert.equal(accepts(ws, WS), true);
});

test("two reads in flight: the one for the CURRENT scope wins, whichever lands last", () => {
  // Coalescing does not order responses. A read for the scope we left landing
  // after the read for the scope we are on must not overwrite it.
  let s = initialScope<string>(WS);
  const wsRead = beginRead(s);
  s = scopeChanged(s, null);
  const globalRead = beginRead(s);

  s = readLanded(s, globalRead, "global rows");
  assert.equal(s.snapshot, "global rows");
  s = readLanded(s, wsRead, "workspace rows"); // the old one, arriving late
  assert.equal(s.snapshot, "global rows", "the late workspace read overwrote the global one");
});
