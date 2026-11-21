// #3898 — copilot's TYPED fork: the line it opens on, the marker its child is
// named and learned by, the one-shot discharge of a line that carries no fork
// token, and the reconciler's title gate (#3888's crossing, closed for forks).
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  adoptableSessionId,
  agentForkCommand,
  canForkCli,
  canForkDelegateCli,
  forkLineNamesParent,
  forkPremintsChild,
  forkRecordCommand,
  forkTypedCommand,
  hasForkSession,
} from "../src/panerestore.ts";
import { forkMarker, forkMarkerNonce } from "../src/typedfork.ts";
import { planSessionAdoption, type SessionRecord } from "../src/sessionreconcile.ts";

const PARENT = "11111111-2222-3333-4444-555555555555";
const CHILD = "99999999-8888-7777-6666-555555555555";

test("#3898: copilot forks by a TYPED /fork — the line is its plain resume of the parent, nothing more", () => {
  assert.equal(canForkCli("copilot"), true, "the Solo gesture admits copilot");
  assert.equal(forkTypedCommand("copilot"), "/fork");
  // The other CLIs fork on the LINE; none of them is typed anything.
  for (const cli of ["claude", "codex", "opencode", "pi", "gemini"]) assert.equal(forkTypedCommand(cli), null, cli);
  // The line: the recorded copilot line, its OWN session excised (a pane
  // forked twice must not carry two), and `--resume=<parent>` — the `=` form
  // copilot's reference tells callers to pass. No fork token, no child id.
  const recorded = `copilot --model gpt-5 --resume=${CHILD} --allow-tool "x"`;
  const forked = agentForkCommand(recorded, null, PARENT, "minted-but-unused");
  assert.equal(forked?.command, `copilot --model gpt-5 --allow-tool "x" --resume=${PARENT}`);
  assert.doesNotMatch(forked?.command ?? "", /\/fork|minted-but-unused/, "the fork is typed, and the child is copilot's to name");
  const argv = agentForkCommand(null, ["copilot", "--resume", CHILD, "--model", "m"], PARENT, "x");
  assert.deepEqual(argv?.argv, ["copilot", "--model", "m", `--resume=${PARENT}`]);
  assert.equal(forkPremintsChild("copilot"), false, "copilot names its child; orrerix learns it");
});

test("#3898: a copilot DELEGATE is not forkable — the frontend mirror of fork_delegate_refusal", () => {
  assert.equal(canForkDelegateCli("copilot"), false);
  // Controls: every line-fork CLI forks a delegate, and an unforkable one does not.
  for (const cli of ["claude", "codex", "opencode", "pi"]) assert.equal(canForkDelegateCli(cli), true, cli);
  assert.equal(canForkDelegateCli("gemini"), false);
});

test("#3898: a typed fork's line never adopts the parent's id — forkOf is what tells the two apart", () => {
  const line = `copilot --model m --resume=${PARENT}`;
  assert.equal(hasForkSession(line, null), false, "no token on the line: the token scan cannot see this fork");
  assert.equal(forkLineNamesParent(line, null, PARENT), true);
  assert.equal(adoptableSessionId(line, null, PARENT), null, "recording the parent would make a restart resume IT");
  // Controls, from both poles: the same line on a pane that is not a fork is a
  // plain resume and adopts its id, and a fork pane whose line names its CHILD
  // (a discharged record) adopts the child.
  assert.equal(adoptableSessionId(line, null, null), PARENT);
  assert.equal(adoptableSessionId(line, null), PARENT, "the defaulted parameter reads as before");
  assert.equal(adoptableSessionId(`copilot --resume=${CHILD}`, null, PARENT), CHILD);
  assert.equal(forkLineNamesParent(`copilot --resume=${CHILD}`, null, PARENT), false);
});

test("#3898: a typed fork's record is discharged — resume the CHILD, or a fresh line, never the parent", () => {
  const live = `copilot --model m --resume=${PARENT}`;
  const liveArgv = ["copilot", "--model", "m", `--resume=${PARENT}`];
  const rec = forkRecordCommand(live, liveArgv, CHILD, PARENT);
  assert.equal(rec.command, `copilot --model m --resume=${CHILD}`);
  assert.deepEqual(rec.argv, ["copilot", "--model", "m", `--resume=${CHILD}`]);
  assert.equal(adoptableSessionId(rec.command, rec.argv, PARENT), CHILD, "the restored pane is the child");
  assert.deepEqual(forkRecordCommand(rec.command, rec.argv, CHILD, PARENT), rec, "idempotent");
  // No child learned yet: drop the parent too. Two panes resumed into one
  // copilot session is the shared-session hazard, not a degraded restore.
  const fresh = forkRecordCommand(live, liveArgv, null, PARENT);
  assert.equal(fresh.command, "copilot --model m");
  assert.deepEqual(fresh.argv, ["copilot", "--model", "m"]);
  // A HUMAN's own `--resume=<id>` line is not a fork: byte-identical.
  assert.deepEqual(forkRecordCommand(live, liveArgv, null, null), { command: live, argv: liveArgv });
});

test("#3898: forkMarker is one token copilot cannot read as anything but a name", () => {
  // The grammar `slash_fork_text` re-checks in the backend: [a-z0-9-], 1-64,
  // no leading '-', and never the bare word `worktree` (`/fork worktree` is a
  // fork into a new git worktree).
  const grammar = /^[a-z0-9][a-z0-9-]{0,63}$/;
  const names = [
    "poc (fork)",
    "worktree",
    "Worktree",
    "",
    "   ",
    "--fork",
    "/fork x",
    "a\r\nb",
    "Ünïcödé pane 日本",
    "x".repeat(200),
    "@file -n name",
  ];
  for (const name of names) {
    for (const nonce of ["3f9a2c", "", "ZZZ", "abcdef0123456789"]) {
      const m = forkMarker(name, nonce);
      assert.match(m, grammar, `${JSON.stringify(name)} / ${JSON.stringify(nonce)} -> ${m}`);
      assert.notEqual(m, "worktree", `${JSON.stringify(name)} -> ${m}`);
    }
  }
  // Readable in copilot's own /resume picker: the pane's name, slugged.
  assert.equal(forkMarker("poc (fork)", "3f9a2c"), "poc-fork-3f9a2c");
  assert.equal(forkMarker("", "3f9a2c"), "fork-3f9a2c", "an empty name still names the child");
  // Two forks of one pane differ: the nonce is what the reconciler tells them apart by.
  const a = forkMarker("poc", forkMarkerNonce());
  const b = forkMarker("poc", forkMarkerNonce());
  assert.match(a, /^poc-[0-9a-f]{6}$/);
  assert.notEqual(a, b);
});

test("#3898: a typed fork adopts ONLY the session carrying its marker — never a sibling's in the same folder (#3888)", () => {
  const marker = "poc-fork-3f9a2c";
  const pane = { key: "7", cli: "copilot" as const, cwd: "C:\\repo", eligibleSinceMs: 1_000, title: marker };
  const session = (id: string, title: string): SessionRecord => ({
    id,
    cli: "copilot",
    cwd: "C:/repo/",
    modifiedMs: 2_000,
    title,
    resumeCommand: `copilot --resume=${id}`,
  });
  const sibling = session("sibling-new", "Copilot session");
  const child = session(CHILD, marker);
  // The child and a sibling pane's new session, both new and in this folder:
  // without the title rule that is two candidates and nothing is adopted; with
  // it, the child is adopted by identity.
  assert.deepEqual(planSessionAdoption([pane], [sibling, child], new Set()), [{ key: "7", sessionId: CHILD }]);
  // The crossing #3888 describes: the child not written yet, the sibling's
  // session the only new one here. Refused — the pane stays unrecorded.
  assert.deepEqual(planSessionAdoption([pane], [sibling], new Set()), []);
  // The control: the same pane WITHOUT a title adopts that sibling, which is
  // exactly the wrong binding the title rule exists to refuse.
  const { title: _omit, ...untitled } = pane;
  assert.deepEqual(planSessionAdoption([untitled], [sibling], new Set()), [{ key: "7", sessionId: "sibling-new" }]);
  // The parent, claimed, is never a candidate even if its title matched.
  assert.deepEqual(planSessionAdoption([pane], [session(PARENT, marker)], new Set([PARENT])), []);
});
