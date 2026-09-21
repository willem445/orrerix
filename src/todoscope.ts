// The To-Do pane's scope/refresh race, as a pure state machine (#3263 S5).
//
// WHY THIS IS A MODULE AND NOT FOUR IF-STATEMENTS IN `todopane.ts`. S4 shipped
// this logic inline in the view, where the only way to exercise it is to mount
// a DOM and win a race — so it was hand-validated and nothing pinned it. It is
// also the part of the pane with the worst failure mode: every arm below,
// wrong, paints one scope's rows under the other scope's header, and those rows
// are LIVE. Completing one sends the op with the other scope's root, and the
// engine resolves `update`/`complete`/`delete` by id with no scope check
// (`docs/design/todo-pane.md`), so the write lands on whichever store actually
// holds the id while the header says otherwise. Deferred from #3293 round 6 as
// S5's, and this is it.
//
// THE THREE RULES, and each is one function below:
//
//  1. A read is TAGGED with the scope it was asked for, because a
//     `TodoSnapshot` carries no scope of its own and cannot identify itself.
//     A response whose tag is not the scope we are on now is DROPPED — both
//     the success and the failure arm, because a read that failed for a scope
//     we have since left says nothing about the one we are on.
//  2. Switching scope DROPS the snapshot, and it needs no race at all: the
//     pane re-renders synchronously, before the new read has even been asked
//     for, so keeping the old one paints the wrong list for at least a frame.
//  3. `snapshot === null` is "we have not LOOKED", never "the list is empty".
//     Every caller that would say something about the list's contents —
//     the empty-state sentence, the view-strip counts — asks [`isLoaded`]
//     first (#3293 round 6 residual 1: the count chips read 0 in that frame).
//
// Generic over the snapshot type so the test can use a string and the pane can
// use a `TodoSnapshot` — this module decides nothing about what a snapshot IS.

/** The scope a read was asked for: a workspace root, or `null` for the global
 *  list. It is a TOKEN here — this module never inspects it beyond `===`, and
 *  in particular never turns it into a path or a key (CLAUDE.md constraint 6:
 *  the backend derives the workspace key from the root, and the frontend must
 *  never name one). */
export type ScopeToken = string | null;

export interface ScopeState<S> {
  /** The scope the pane is on RIGHT NOW. */
  root: ScopeToken;
  /** What the backend last said about `root`, or null when nothing has landed
   *  for it yet. */
  snapshot: S | null;
  /** The last read for `root` failed. The pane shows "stale" and keeps
   *  whatever `snapshot` it had — a failed read never publishes an empty list
   *  over a real one. */
  loadFailed: boolean;
}

export function initialScope<S>(root: ScopeToken): ScopeState<S> {
  return { root, snapshot: null, loadFailed: false };
}

/**
 * The token a read starting NOW should carry.
 *
 * A function rather than "just use `state.root`" so the capture is a named
 * step at the call site: the whole defect class is reading the scope AFTER the
 * `await` instead of before it, and a line that says `beginRead` is one a
 * reviewer can see is on the right side of the boundary.
 */
export function beginRead<S>(state: ScopeState<S>): ScopeToken {
  return state.root;
}

/** Is this response still about the scope we are on? */
export function accepts<S>(state: ScopeState<S>, token: ScopeToken): boolean {
  return token === state.root;
}

/**
 * A read landed.
 *
 * Returns the state unchanged — by identity, so a caller can skip a render —
 * when the token is stale. Nothing is lost by dropping it: the switch that
 * made it stale asked for a fresh read of its own, and the pane's
 * `CoalescingRefresh` guarantees a trailing run.
 */
export function readLanded<S>(state: ScopeState<S>, token: ScopeToken, snapshot: S): ScopeState<S> {
  if (!accepts(state, token)) return state;
  return { root: state.root, snapshot, loadFailed: false };
}

/**
 * A read failed.
 *
 * Keeps the snapshot it had: the list we hold is still the truth for the scope
 * we are on, and publishing an empty one over it would destroy it. This is the
 * arm that differs from [`scopeChanged`], where the list we hold is
 * definitively the WRONG scope's and dropping it is right rather than merely
 * safe.
 */
export function readFailed<S>(state: ScopeState<S>, token: ScopeToken): ScopeState<S> {
  if (!accepts(state, token)) return state;
  return { root: state.root, snapshot: state.snapshot, loadFailed: true };
}

/**
 * The pane moved to another scope.
 *
 * Drops the snapshot AND the failure flag. A no-op (by identity) when the
 * scope did not actually change, so a redundant call cannot blank a list the
 * pane already has.
 */
export function scopeChanged<S>(state: ScopeState<S>, next: ScopeToken): ScopeState<S> {
  if (next === state.root) return state;
  return { root: next, snapshot: null, loadFailed: false };
}

/**
 * Has anything landed for the scope we are on?
 *
 * The one question the "nothing here" / "nothing yet" distinction turns on,
 * and the reason it has a name: `state.snapshot !== null` spelled at four call
 * sites is four places to get the polarity wrong.
 */
export function isLoaded<S>(state: ScopeState<S>): boolean {
  return state.snapshot !== null;
}
