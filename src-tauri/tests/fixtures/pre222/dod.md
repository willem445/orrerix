A task is done when ALL of these hold:

1. The change implements the brief's acceptance criteria — if the brief is ambiguous,
   ask the orchestrator (`message_orchestrator`) before guessing.
2. **Tests test intent.** Add or extend unit/functional tests that would fail if the
   feature were broken or regressed — not vacuous assertions written to pass. Exercise
   the behavior the issue asks for, including at least one edge/failure case. Run the
   project's existing test suite and keep it green.
3. **Red before green — evidence, not assertion.** A test nobody has seen fail is a decoration,
   and "these tests would catch it" is the easiest sentence in software to write. So watch them
   fail first: run your new tests against the code *without* your change (check out the base
   branch, or set the implementation aside another way — a WIP commit, a copied file — and keep
   the tests; never `git stash` it, see below) and confirm they fail **for the reason
   you expect** — not on a compile error, which masks behavior rather than testing it. Put the
   evidence in the PR body's **agent layer** (see **Two layers** above) and your `done` report:
   the command, the failure line it printed, and the same command passing on your branch. If a new test can't be made to fail,
   either it isn't testing your change or your change isn't doing anything — find out which
   before you ship it.

   **What the evidence is owed for — and the exemption.** Every change to *behavior* adds a test,
   and that test owes the evidence. A change whose intent carries **no new testable behavior** owes
   something else, and there are exactly four of them:
   - **docs- or comment-only** (prose, a design note, a README section);
   - **a revert** to a known-good state;
   - **a pure rename/move** whose behavior the existing suite already pins;
   - **a re-blessed golden/snapshot fixture**, where the deliberate change *is* the fixture.

   For those, put **one line in the PR** naming which of the four it is, why no new test exists,
   and the existing suite green. That line is the evidence: "there was nothing to test" is a claim
   like any other — stated, it is reviewable; unstated, the PR is **not done**. Anything outside
   those four evidences the normal way, and a change that *feels* untestable but isn't on the list
   is a change you haven't found the test for yet.
4. **Every CI citation is re-derived after the push it describes.** A run citation is a fact
   about a **SHA**, not a fact about the PR — so any push or rebase silently invalidates every
   run id, run link and "green on all three platforms" already sitting in the body, and that
   text survives the push untouched: nothing rewrites it for you, and a reader has no way to
   tell. After **any** push or rebase, treat every citation in the body as **stale until
   re-derived**: list the runs for the new head (`gh run list --branch <your branch> --json
   headSha,databaseId,conclusion`), assert the run's `headSha` **is** the head you are reporting
   on (`git rev-parse HEAD`), then update the body's agent layer — before you `report`, not
   after a reviewer asks. Three stale-green citations landed in one batch: #571 cited a run three commits behind
   head, and #588 cited a pre-rebase run at review 1 and then the *same* pre-rebase run again
   after the rebase at review 2. Every one was caught by a reviewer; none by the worker who
   wrote it.
5. Docs updated: user-facing documentation for user-visible changes, plus a short design
   note (in the repo's docs convention) for non-obvious architecture decisions.
6. Code matches the repo's existing style, conventions, and **stated constraints**. Read the
   contributor docs (`CLAUDE.md` / `AGENTS.md` / `CONTRIBUTING.md`) and the design notes before
   you add a **dependency**, change a **public contract** (a command signature, a wire shape, a
   file format, a persisted schema), duplicate a mechanism the repo already has, or reach across
   a module boundary. Each of those needs its argument *in the PR* — and a contract change needs
   a design note — because that is the bar the orchestrator sends work back on, plan or PR.
7. PR is open, issue linked, and you have `report`ed `done` with the PR URL. **The link keyword
   has to match your scope:** `Closes #N` only when this PR finishes the issue outright —
   anything partial links as `Part of #N` (or `Mitigates #N`) instead. A **squash merge honors a
   `Closes` in the PR body regardless of how partial the change actually was**, and no hedging
   sentence elsewhere in the body stops it: #569 and #590 were both auto-closed that way this
   session with real scope still open on them, and had to be spotted and reopened by hand.

   **The keyword scan is textual and context-blind — grep your own prose for it.** GitHub
   matches `close`/`fix`/`resolve` (any inflection) immediately followed by `#N` **anywhere**
   in the PR body and in every commit message a squash merge aggregates: inside a blockquote,
   inside a caveat, inside a sentence asking a human to do it by hand, and inside the collapsed
   agent layer, which is part of the body however a squash message is cut. #569 was auto-closed a
   *second* time by PR #615 — which linked `Part of #569` deliberately, explained the choice
   at length, and ended that explanation "Please close #569 by hand if you agree", which is
   the closing directive it was arguing against. Before you open or update a `Part of` PR,
   grep the body you are about to post, and `git log` for the branch, for that
   keyword-next-to-`#N` pattern and reword it ("#569 stays open", "for the human to close
   out"). It costs one grep; the alternative is a live issue silently closed at merge.
