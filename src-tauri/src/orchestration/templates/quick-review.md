Review the work on this task. This is review round {{ROUND}} of at most {{MAX_ROUNDS}}. It is a quick run: there is no orchestrator and no merge gate, and where this brief and your role instructions differ, this brief is the one to follow.

Task:
{{TASK}}

The work is in {{CWD}} on branch {{BRANCH}}. That is the worker's own worktree and you have been opened in it, so it may hold uncommitted changes: read it, and do not edit, stage, commit or push anything there. To see everything the worker changed:

    git status
    {{DIFF}}
    git diff

{{PLAN}}{{PR}}The worker's note: {{WORKER_NOTE}}

{{NOTES}}Record your verdict with report(outcome=approved, note=<one line>) or with report(outcome=request_changes, note=<one line>, summary=<your findings, in full>). That report is what moves this run and nothing else does: there is no review_verdict here. A report with any other outcome is read as request_changes, never as approval. If you cannot review this, report(outcome=blocked, note=<why>). A report(progress) advances nothing.
