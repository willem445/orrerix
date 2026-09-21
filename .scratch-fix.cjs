const fs = require('node:fs');
function edit(p, pairs) {
  let s = fs.readFileSync(p, 'utf8');
  const eol = s.includes('\r\n') ? '\r\n' : '\n';
  const L = (t) => t.split('\n').join(eol);
  for (let [a, b] of pairs) {
    a = L(a); b = L(b);
    if (!s.includes(a)) throw new Error(p + ': anchor: ' + JSON.stringify(a.slice(0, 80)));
    if (s.split(a).length > 2) throw new Error(p + ': not unique: ' + a.slice(0, 70));
    s = s.split(a).join(b);
  }
  fs.writeFileSync(p, s);
}

edit('doc/design/delivery-triage.md', [
  // B1: the audit row now carries the text, and the frame points at it.
  [
    `A deferred notice is appended to \`<group-dir>/deferred.json\` and audited
\`delivery-triaged {to, from, kind, action: "rule:<name>"}\`. A delivered one is
audited with the same row and \`action\` set to the deliver reason.`,
    `A deferred notice is appended to \`<group-dir>/deferred.json\` and audited
\`delivery-triaged {to, from, kind, action: "rule:<name>", text}\`. A delivered
one is audited with the same row, \`action\` set to the deliver reason, and **no
\`text\`** — it already has a \`prompt\` row carrying its text, and a second copy
would be a second record to keep in step.

**The \`text\` on a deferral is load-bearing, not diagnostic** (review round 1,
B1). Before it, the full text of a held notice existed in exactly one place —
\`deferred.json\` — and only until the flush cleared it. The deferrable classes
audit no notice text of their own (the agent-exit row is \`{agent, exit_code}\`),
and \`prompt\` is deliberately not written for a deferral, so a flushed notice's
words existed nowhere afterwards. That made three of this feature's own claims
false at once: the flush frame's pointer, §4's "the notices are in the audit log
either way" below, and "NOTHING IS EVER DROPPED" for the crash between the clear
and the paste. One field makes all three true.`,
  ],
  [
    `[orrerix] 3 notices deferred over the last 12 min (flushed because something did
need you). Each closed by a shape rule, none of them a decision; full text via
list_deferred():`,
    `[orrerix] 3 notices deferred over the last 12 min (flushed because something did
need you). Each closed by a shape rule, none of them a decision; full text is on
this group's audit log as delivery-triaged:`,
  ],
  [
    `**Nothing is ever dropped.** The store is written BEFORE the notice leaves the
delivery path, and a write that fails DELIVERS: a deferral nobody recorded is a
notice lost at the next restart, and losing one is worse than spending a wake.
\`deferred.json\` survives a restart, and \`list_deferred()\` reads it back in full.`,
    `**Nothing is ever dropped.** The store is written BEFORE the notice leaves the
delivery path, and a write that fails DELIVERS: a deferral nobody recorded is a
notice lost at the next restart, and losing one is worse than spending a wake.
\`deferred.json\` survives a restart, and \`list_deferred()\` reads back what is
held **right now**; the audit row is what carries a notice's text after the
flush has cleared it. The frame therefore points at the audit log rather than at
the tool, which answers \`count: 0\` by the time anyone can read the frame.`,
  ],
  // B2 + premortem 1, in the bounds section.
  [
    `2. **\`max_defer_minutes\`** (default 30, refused outside \`1..=240\`) — on the
   watchdog's own timer, because bound (1) is a wait on a signal that may never
   come, and no wait on a fallible signal is unbounded;`,
    `2. **\`max_defer_minutes\`** (default 30, refused outside \`1..=240\`) — on the
   watchdog's own timer, because bound (1) is a wait on a signal that may never
   come, and no wait on a fallible signal is unbounded. It targets **live**
   orchestrators only (review round 1, B2): flushing at a dead pane would clear
   the store, have \`deliver_prompt\` refuse \`AgentDead\`, and lose every held
   notice — §1's placement argument, reintroduced on the flush side. A group
   with no live orchestrator keeps holding until one exists;`,
  ],
  [
    `3. **\`MAX_DEFERRED\`** (40) — a CI storm inside one window flushes early
   rather than growing a frame nobody can read.`,
    `3. **\`MAX_DEFERRED\`** (40) — a CI storm inside one window flushes early
   rather than growing a frame nobody can read.

**Turning triage OFF is a fourth release, and it has to be** (review round 1,
premortem 1). All three bounds above are gated on the same policy that created
the entries, so a store held by a policy since switched off — or whose file
stopped parsing — had nothing that would ever release it: \`list_deferred\`
answered \`enabled: false, count: N\` indefinitely. The flush tick now treats a
non-empty store under a disabled policy as due immediately. Turning the feature
off hands back what it is holding.`,
  ],
]);

console.log('design note: B1 + B2 + premortem 1');
