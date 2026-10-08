// The quick runs this window is showing (#3679): which groups have one, the
// last status read for each, and the poll that keeps a WORKING run's chip
// current. DOM-free — the host hands it how to read a status and how to paint
// one — so the polling rule is testable with a hand-cranked timer.
//
// **Only a working run is polled.** A parked run moves when the human resumes
// or stops it, which goes through this module and refreshes it; a finished one
// never moves. So the timer is wanted while some run is doing something on its
// own and not otherwise — when the task ends, nothing keeps polling, here as in
// the backend. On top of that it goes through the poll gate (`pollgate.ts`),
// so a hidden window polls nothing at all, and every tick is single-flighted.
//
// Design note: docs/design/quick-orchestration.md.

import { PollGate, type PollGateOptions } from "./pollgate.ts";
import { quickIsWorking, type QuickStatus } from "./quickchip.ts";
import { SingleFlight } from "./singleflight.ts";

/** How often a working run's status is re-read. A hop is announced by the
 *  pane that opens for it; this only has to keep the chip's words honest. */
export const QUICK_POLL_MS = 4000;

export interface QuickRunsDeps {
  /** Read one group's status — `orch_quick_status`. */
  status(group: string): Promise<QuickStatus>;
  /** Paint a status onto that group's panes. Called on every read, changed or
   *  not: a pane that opened since the last read still needs its chip. */
  apply(group: string, status: QuickStatus): void;
  /** Test seams: the timer pair, and the poll gate's own (visibility). */
  setInterval?: (fn: () => void) => unknown;
  clearInterval?: (handle: unknown) => void;
  gate?: PollGateOptions;
}

export class QuickRuns {
  private readonly statuses = new Map<string, QuickStatus>();
  private readonly deps: QuickRunsDeps;
  private readonly setTimer: (fn: () => void) => unknown;
  private readonly clearTimer: (handle: unknown) => void;
  private pollTimer: unknown = null;
  /** One tick at a time: a status read slower than the cadence is skipped
   *  rather than stacked (performance.md INV-4). */
  private tickFlight = new SingleFlight();
  private readonly gate: PollGate;

  constructor(deps: QuickRunsDeps) {
    this.deps = deps;
    this.setTimer = deps.setInterval ?? ((fn) => setInterval(fn, QUICK_POLL_MS));
    this.clearTimer = deps.clearInterval ?? ((h) => clearInterval(h as ReturnType<typeof setInterval>));
    this.gate = new PollGate(
      {
        arm: () => {
          // Clear-before-arm: a leftover timer would double the cadence.
          if (this.pollTimer !== null) this.clearTimer(this.pollTimer);
          this.pollTimer = this.setTimer(() => void this.tick());
        },
        disarm: () => {
          if (this.pollTimer !== null) this.clearTimer(this.pollTimer);
          this.pollTimer = null;
        },
        // The catch-up when a hidden window comes back.
        refresh: () => void this.tick(),
      },
      deps.gate
    );
  }

  /** The last status read for `group`, or `null` when it is not a quick group
   *  this window knows about. What the pane menu reads. */
  statusOf(group: string | null): QuickStatus | null {
    return group ? (this.statuses.get(group) ?? null) : null;
  }

  /** Whether the poll timer is running right now. */
  get polling(): boolean {
    return this.gate.armed;
  }

  /** Read `group`'s status now, remember it if the group has a run, and paint
   *  it. Answers the status, or `null` when the read failed — a failed read
   *  changes nothing that was known, since "could not look" is not "no run".
   *
   *  This is also how a group is first noticed: a restored tab's group is
   *  refreshed once at boot, and a group with no run is simply not kept. */
  async refresh(group: string): Promise<QuickStatus | null> {
    let status: QuickStatus;
    try {
      status = await this.deps.status(group);
    } catch {
      return null;
    }
    this.accept(group, status);
    return status;
  }

  /** Take a status some other call already returned — every control verb
   *  answers the run's status, so there is no need to ask twice. */
  accept(group: string, status: QuickStatus): void {
    if (!status.exists) {
      this.statuses.delete(group);
    } else {
      this.statuses.set(group, status);
      this.deps.apply(group, status);
    }
    this.retime();
  }

  /** Stop showing `group` — its group ended (`orch-group-ended` reaches
   *  this through the wiring's `forgetGroup`). */
  forget(group: string): void {
    this.statuses.delete(group);
    this.retime();
  }

  /** Keep only the runs `keep` answers true for — how a run whose tab was
   *  closed stops being polled (#3679).
   *
   *  A run is shown on its panes, and its panes are in the tab its group is
   *  bound to. Close that tab and there is nothing left in this window to
   *  paint the status onto, so asking for it every few seconds is a poll with
   *  no reader. The run itself is not ended by this: it parks when the engine
   *  sees the pane that held the turn is gone, and it is listed in the
   *  launcher's Quick task form to resume or stop. */
  retain(keep: (group: string) => boolean): void {
    let dropped = false;
    for (const group of [...this.statuses.keys()]) {
      if (!keep(group)) {
        this.statuses.delete(group);
        dropped = true;
      }
    }
    if (dropped) this.retime();
  }

  /** Want the poll exactly while something is working. */
  private retime(): void {
    if ([...this.statuses.values()].some(quickIsWorking)) this.gate.enable();
    else this.gate.disable();
  }

  /** One poll: re-read every WORKING run. A parked or finished one is left
   *  alone, which is the whole of the "nothing keeps polling" rule. */
  async tick(): Promise<void> {
    await this.tickFlight.run(async () => {
      const working = [...this.statuses.entries()].filter(([, s]) => quickIsWorking(s)).map(([g]) => g);
      await Promise.all(working.map((g) => this.refresh(g)));
    });
  }
}
