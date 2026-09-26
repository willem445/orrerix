//! The task board: list/notes caps, dependency links, hierarchy, the Agile ladder, readiness and archive.
//!
//! One module of the `orchestration` integration-test target (`main.rs`),
//! split out of the former single-file `tests/orchestration.rs` by #3498 P1
//! as a pure move. Layout rules: docs/design/module-layout.md.

use super::*;

// ---------- task board ----------

pub(crate) fn patch(title: Option<&str>, status: Option<&str>, note: Option<&str>) -> TaskPatch {
    TaskPatch {
        title: title.map(String::from),
        status: status.map(String::from),
        note: note.map(String::from),
        ..Default::default()
    }
}

#[test]
fn task_lifecycle_create_edit_note_delete() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let t = reg.upsert_task(&g.id, "orch-1", None, patch(Some("Add retry logic"), None, None)).unwrap();
    assert_eq!(t.status, "queued", "new tasks start queued");
    assert_eq!(t.id, "t-1");
    // Edit status + append a note; note carries author and timestamp.
    let t = reg
        .upsert_task(&g.id, "human", Some(&t.id), patch(None, Some("in-progress"), Some("looks good")))
        .unwrap();
    assert_eq!(t.status, "in-progress");
    assert_eq!(t.notes.len(), 1);
    assert_eq!(t.notes[0].author, "human");
    assert!(t.notes[0].ts_ms > 0);
    // Invalid status rejected; unknown id rejected; title required for new.
    assert!(reg.upsert_task(&g.id, "x", Some(&t.id), patch(None, Some("nope"), None)).is_err());
    assert!(reg.upsert_task(&g.id, "x", Some("t-999"), patch(None, Some("done"), None)).is_err());
    assert!(reg.upsert_task(&g.id, "x", None, patch(None, None, None)).is_err());
    // Delete.
    reg.delete_task(&g.id, "human", &t.id).unwrap();
    assert!(reg.tasks(&g.id).is_empty());
    assert!(reg.delete_task(&g.id, "human", &t.id).is_err());
}

// ---------- #245: compact list_tasks + note cap ----------

pub(crate) fn note(ts_ms: u64, text: &str) -> TaskNote {
    TaskNote { ts_ms, author: "orch".into(), text: text.into() }
}

#[test]
fn task_summary_drops_notes_but_counts_them() {
    let t = Task {
        id: "t-1".into(),
        title: "Fix parser".into(),
        status: "in-progress".into(),
        issue: Some("#7".into()),
        pr: None,
        pr_base: None,
        assignee: Some("w-2".into()),
        session: Some("sess-1".into()),
        notes: vec![note(1, "a"), note(2, "b"), note(3, "c")],
        deps: vec![],
        related: vec![],
        parent: None,
        kind: None,
        sprint: None,
        links: vec![],
        demo_path: None,
        description: None,
        cleared_ms: None,
        updated_ms: 42,
    };
    let s = task_summary(&t, false, 0, 0);
    assert_eq!(s.id, "t-1");
    assert_eq!(s.title, "Fix parser");
    assert_eq!(s.status, "in-progress");
    assert_eq!(s.issue.as_deref(), Some("#7"));
    assert_eq!(s.assignee.as_deref(), Some("w-2"));
    assert_eq!(s.session.as_deref(), Some("sess-1"));
    assert_eq!(s.updated_ms, 42);
    assert_eq!(s.note_count, 3, "every note counts, even though the text is dropped");
    // The summary must serialize with no notes field at all — a caller reading
    // raw JSON (as an MCP client does) must never see note text.
    let v = serde_json::to_value(&s).unwrap();
    assert!(v.get("notes").is_none(), "TaskSummary must not carry a notes field");
}

#[test]
fn cap_task_notes_leaves_under_cap_history_untouched() {
    let notes = vec![note(1, "a"), note(2, "b"), note(3, "c")];
    let capped = cap_task_notes(notes.clone(), 20);
    assert_eq!(capped.len(), 3);
    assert_eq!(capped[0].text, "a", "no collapse below the cap");
}

#[test]
fn cap_task_notes_collapses_oldest_excess_into_one_placeholder() {
    // 25 notes capped at 5: keep the newest 4 verbatim + 1 placeholder = 5.
    let notes: Vec<TaskNote> = (1..=25).map(|i| note(i, &format!("note-{i}"))).collect();
    let capped = cap_task_notes(notes, 5);
    assert_eq!(capped.len(), 5, "collapsed history stays at exactly the cap");
    assert!(
        capped[0].text.contains("21 earlier notes collapsed"),
        "the placeholder names how many it swallowed: {}",
        capped[0].text
    );
    assert_eq!(capped[0].ts_ms, 1, "the placeholder is timestamped at the oldest note it swallowed");
    // The newest 4 real notes survive verbatim, oldest-of-the-kept first.
    assert_eq!(capped[1].text, "note-22");
    assert_eq!(capped[2].text, "note-23");
    assert_eq!(capped[3].text, "note-24");
    assert_eq!(capped[4].text, "note-25");
}

#[test]
fn cap_task_notes_zero_means_uncapped() {
    let notes: Vec<TaskNote> = (1..=25).map(|i| note(i, &format!("note-{i}"))).collect();
    let capped = cap_task_notes(notes.clone(), 0);
    assert_eq!(capped.len(), 25, "max=0 must not be read as \"drop everything\"");
}

#[test]
fn cap_task_notes_placeholder_count_accumulates_across_repeated_collapses() {
    // Review finding on #245: cap_task_notes runs once PER APPEND in the real
    // path (upsert_task calls it after every single note push), not once over
    // a big batch — so a placeholder from an earlier round routinely gets
    // swept into a later collapse. The reported count must accumulate the
    // TRUE total dropped over the task's lifetime, not reset to "how many
    // this round" (which, at steady state one-at-a-time, was always the same
    // small number no matter how much history had actually rolled off).
    let mut notes: Vec<TaskNote> = Vec::new();
    for i in 1..=30u64 {
        notes.push(note(i, &format!("note-{i}")));
        notes = cap_task_notes(notes, 20);
    }
    assert_eq!(notes.len(), 20);
    assert!(
        notes[0].text.contains("11 earlier notes collapsed"),
        "the true cumulative count (11 notes rolled off across repeated collapses) must survive, \
         not reset every round: {}",
        notes[0].text
    );
    assert_eq!(notes[0].ts_ms, 1, "the oldest timestamp is still the very first note ever dropped");
    // The newest max-1 real notes are kept verbatim regardless.
    assert_eq!(notes[1].text, "note-12");
    assert_eq!(notes.last().unwrap().text, "note-30");
}

#[test]
fn upsert_task_caps_live_note_history_as_notes_accumulate() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let mut t = reg.upsert_task(&g.id, "orch", None, patch(Some("Long-running task"), None, None)).unwrap();
    for i in 0..30 {
        t = reg
            .upsert_task(&g.id, "orch", Some(&t.id), patch(None, None, Some(&format!("update {i}"))))
            .unwrap();
    }
    assert_eq!(t.notes.len(), 20, "live notes stay capped even after 30 appends");
    assert!(
        t.notes[0].text.contains("collapsed"),
        "the oldest surviving entry is the collapse placeholder: {}",
        t.notes[0].text
    );
    // The newest note is always exactly what was just appended.
    assert_eq!(t.notes.last().unwrap().text, "update 29");
    // list_tasks (task_summaries) never carries this text at all.
    let summaries = reg.task_summaries(&g.id);
    let s = summaries.iter().find(|s| s.id == t.id).unwrap();
    assert_eq!(s.note_count, 20);
    let v = serde_json::to_value(&summaries).unwrap();
    assert!(!v.to_string().contains("update 29"), "no note text leaks into the compact summaries");
    // get_task still returns the full (capped) history.
    let full = reg.get_task(&g.id, &t.id).unwrap();
    assert_eq!(full.notes.len(), 20);
}

#[test]
fn delete_done_removes_only_done_and_notifies_once() {
    // #120: the board's "delete all done" clears every done task in one action,
    // and the orchestrator must hear about it ONCE — not once per task (the
    // per-task notices are the token waste the issue calls out).
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();

    // Five tasks, three of them done, interleaved with non-done ones.
    let ids: Vec<String> = ["a", "b", "c", "d", "e"]
        .iter()
        .map(|title| reg.upsert_task(&g.id, "orch", None, patch(Some(title), None, None)).unwrap().id)
        .collect();
    for (i, status) in [(0, "done"), (1, "in-progress"), (2, "done"), (3, "queued"), (4, "done")] {
        reg.upsert_task(&g.id, "human", Some(&ids[i]), patch(None, Some(status), None)).unwrap();
    }

    // Pause the group so the best-effort board-change notice is QUEUED and
    // audited rather than pasted — test mode has no real PTY to deliver into.
    // The pause branch fires inside deliver_to_orchestrator, past the coalescing
    // point, so the audited count equals the notice count (#569: it queues now,
    // which is why the orchestrator needs a pane to hold it).
    pause_with_pane(&reg, &g.id, &orch.id, 6400);

    let removed = reg.delete_done_tasks(&g.id, "human").unwrap();
    removed.iter().for_each(|id| assert!(ids.contains(id)));
    assert_eq!(removed.len(), 3, "exactly the three done tasks are removed");

    // Only the non-done tasks survive, in board order.
    let survivors: Vec<String> = reg.tasks(&g.id).iter().map(|t| t.status.clone()).collect();
    assert_eq!(survivors, ["in-progress", "queued"], "non-done tasks are untouched");

    // The heart of #120: ONE board-change notice for the whole batch.
    let notices: Vec<_> = reg
        .audit_log(&g.id)
        .into_iter()
        .filter(|e| {
            e.action == "prompt"
                && e.detail["text"].as_str().is_some_and(|s| s.contains("updated the task board"))
        })
        .collect();
    assert_eq!(notices.len(), 1, "the batch must coalesce to a single board-change notice");
    assert!(
        notices[0].detail["text"].as_str().unwrap().contains("3 done tasks"),
        "the single notice names the batch size, got: {}",
        notices[0].detail["text"]
    );

    // A second sweep with nothing done is a no-op: no delete, no new notice.
    let again = reg.delete_done_tasks(&g.id, "human").unwrap();
    assert!(again.is_empty(), "nothing left to delete");
    let notice_count = reg
        .audit_log(&g.id)
        .into_iter()
        .filter(|e| {
            e.action == "prompt"
                && e.detail["text"].as_str().is_some_and(|s| s.contains("updated the task board"))
        })
        .count();
    assert_eq!(notice_count, 1, "a no-op sweep must not notify");
}

#[test]
fn delete_selected_removes_only_named_ids_and_notifies_once() {
    // #120 follow-up: the board's multi-select "delete selected" clears exactly
    // the ticked rows in one action — one coalesced board-change notice for the
    // whole batch, unknown ids skipped (the board can shift under the human's
    // selection), and an empty selection a silent no-op.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();

    // Four tasks in assorted statuses — selection is by id, not status.
    let ids: Vec<String> = ["a", "b", "c", "d"]
        .iter()
        .map(|title| reg.upsert_task(&g.id, "orch", None, patch(Some(title), None, None)).unwrap().id)
        .collect();

    // Pause so the best-effort notice is queued-and-audited rather than pasted
    // (as in the delete-done test — test mode has no PTY to deliver into).
    pause_with_pane(&reg, &g.id, &orch.id, 6401);

    // Select two real ids plus one that never existed: the unknown id is
    // skipped, the two real ones go, and the removed set is exactly those two.
    let selection = vec![ids[1].clone(), ids[3].clone(), "t-nope".to_string()];
    let removed = reg.delete_tasks(&g.id, "human", &selection).unwrap();
    assert_eq!(removed.len(), 2, "only the two real selected tasks are removed");
    assert!(removed.contains(&ids[1]) && removed.contains(&ids[3]));
    assert!(!removed.iter().any(|id| id == "t-nope"), "the unknown id is skipped, not returned");

    // The un-ticked tasks survive, in board order; nothing else is touched.
    let survivors: Vec<String> = reg.tasks(&g.id).iter().map(|t| t.id.clone()).collect();
    assert_eq!(survivors, vec![ids[0].clone(), ids[2].clone()], "only the selected rows go");

    // The skipped id is recorded in the audit entry for traceability.
    let del = reg
        .audit_log(&g.id)
        .into_iter()
        .find(|e| e.action == "task-delete-selected")
        .expect("a delete-selected audit entry");
    let skipped: Vec<&str> = del.detail["skipped"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
    assert_eq!(skipped, vec!["t-nope"], "the audit notes the id that no longer named a row");

    // The heart of #120: ONE board-change notice for the whole batch.
    let notices = |reg: &OrchRegistry| {
        reg.audit_log(&g.id)
            .into_iter()
            .filter(|e| {
                e.action == "prompt"
                    && e.detail["text"].as_str().is_some_and(|s| s.contains("updated the task board"))
            })
            .count()
    };
    assert_eq!(notices(&reg), 1, "the batch must coalesce to a single board-change notice");

    // An empty selection is a silent no-op: no delete, no new notice.
    let none = reg.delete_tasks(&g.id, "human", &[]).unwrap();
    assert!(none.is_empty(), "empty selection removes nothing");
    // A selection of only unknown ids likewise no-ops (nothing matched).
    let miss = reg.delete_tasks(&g.id, "human", &["t-gone".to_string()]).unwrap();
    assert!(miss.is_empty(), "a selection matching nothing removes nothing");
    assert_eq!(notices(&reg), 1, "no-op deletes must not notify");
    assert_eq!(reg.tasks(&g.id).len(), 2, "the board is unchanged by the no-op sweeps");
}

#[test]
fn task_board_persists_and_reorders() {
    let dir = tempfile::tempdir().unwrap();
    let gid;
    {
        let reg = relaunch_registry(dir.path());
        reg.set_port(45999);
        let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
        gid = g.id.clone();
        for title in ["a", "b", "c"] {
            reg.upsert_task(&g.id, "orch-1", None, patch(Some(title), None, None)).unwrap();
        }
        // Move c first; unmentioned ids keep relative order behind it.
        reg.reorder_tasks(&g.id, "human", &["t-3".into()]).unwrap();
    }
    let reg = relaunch_registry(dir.path());
    reg.set_port(45999);
    let titles: Vec<String> = reg.tasks(&gid).iter().map(|t| t.title.clone()).collect();
    assert_eq!(titles, ["c", "a", "b"], "order must survive an app restart");
}

#[test]
fn task_tools_are_role_gated_but_listing_is_shared() {
    let (reg, _d, co, cw) = setup_mcp();
    let denied = dispatch(&reg, &cw, "tools/call",
        &json!({ "name": "upsert_task", "arguments": { "title": "sneaky" } })).unwrap();
    assert_eq!(denied["isError"], true, "workers must not edit the board");
    let ok = dispatch(&reg, &co, "tools/call",
        &json!({ "name": "upsert_task", "arguments": { "title": "Fix parser", "status": "in-progress", "issue": "#7" } }))
        .unwrap();
    assert_eq!(ok["isError"], false);
    let id = ok["content"][0]["text"].as_str().unwrap().split_whitespace().next().unwrap().to_string();
    dispatch(&reg, &co, "tools/call",
        &json!({ "name": "upsert_task", "arguments": { "id": id, "note": "reviewer flagged an edge case in the parser" } }))
        .unwrap();

    let listed = dispatch(&reg, &cw, "tools/call",
        &json!({ "name": "list_tasks", "arguments": {} })).unwrap();
    let text = listed["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("Fix parser") && text.contains("#7"),
        "workers must be able to read the board");
    // #245: list_tasks must return compact rows — no notes array, note_count instead.
    assert!(!text.contains("edge case"), "note text must not appear in the compact list_tasks view: {text}");
    assert!(!text.contains("\"notes\""), "compact rows must not carry a notes field: {text}");
    assert!(text.contains("note_count"), "compact rows surface a note_count: {text}");

    // get_task fetches the full record, including the note.
    let detail = dispatch(&reg, &cw, "tools/call",
        &json!({ "name": "get_task", "arguments": { "id": id } })).unwrap();
    let dtext = detail["content"][0]["text"].as_str().unwrap();
    assert!(dtext.contains("edge case"), "get_task returns the full note text: {dtext}");

    // Unknown id is a clean error, not a panic.
    let missing = dispatch(&reg, &cw, "tools/call",
        &json!({ "name": "get_task", "arguments": { "id": "t-999" } })).unwrap();
    assert_eq!(missing["isError"], true);
}

// ---------- #865: list_tasks done-row cap ----------

/// A bare `TaskSummary` row for the pure `filter_done_rows` tests — no
/// registry, no clock, `updated_ms` set explicitly so "newest" is
/// unambiguous instead of racing the real wall clock.
fn done_row(id: &str, status: &str, updated_ms: u64) -> TaskSummary {
    TaskSummary {
        id: id.into(),
        title: format!("task {id}"),
        status: status.into(),
        issue: None,
        pr: None,
        pr_base: None,
        assignee: None,
        session: None,
        updated_ms,
        note_count: 0,
        deps: vec![],
        related: vec![],
        parent: None,
        kind: None,
        sprint: None,
        links: vec![],
        // #1349: derived per read, so the literal that stands in for one carries
        // the token an empty row produces. `filter_done_rows` never reads it.
        link_etag: link_etag(&linked(id, status, &[], &[])),
        children: 0,
        children_done: 0,
        ready: false,
    }
}

#[test]
fn filter_done_rows_keeps_newest_done_and_every_non_done_row_in_board_order() {
    // Five rows, three `done` at different `updated_ms`, capped at 2: only the
    // oldest `done` row (t-1) should be dropped, and every survivor — done or
    // not — must keep the board's own (priority) order, not be reshuffled.
    let rows = vec![
        done_row("t-1", "done", 10),
        done_row("t-2", "queued", 5),
        done_row("t-3", "done", 30),
        done_row("t-4", "done", 20),
        done_row("t-5", "in-progress", 1),
    ];
    let (kept, omitted) = filter_done_rows(rows, 2);
    assert_eq!(omitted, 1, "exactly the one done row beyond the cap is elided");
    let ids: Vec<&str> = kept.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(
        ids,
        ["t-2", "t-3", "t-4", "t-5"],
        "t-1 (oldest done, updated_ms 10) is dropped; every other row survives in board order"
    );
}

#[test]
fn filter_done_rows_breaks_updated_ms_ties_by_board_order() {
    // Review finding (#865): the tie-break comment claimed ties fall back to
    // board (priority) order, but no prior test had two `done` rows at the
    // SAME `updated_ms` straddling the cap — distinct values everywhere else
    // meant `updated_ms` alone always decided it, and the tie-break arm
    // (`.then(a.cmp(&b))`) was unwitnessed: flipping it to `b.cmp(&a)` would
    // have stayed green. Two `done` rows tie at 10; capped at 1, the EARLIER
    // one on the board (t-1) must be the one that survives.
    let rows = vec![done_row("t-1", "done", 10), done_row("t-2", "done", 10), done_row("t-3", "queued", 5)];
    let (kept, omitted) = filter_done_rows(rows, 1);
    assert_eq!(omitted, 1, "one of the two tied done rows is elided");
    let ids: Vec<&str> = kept.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, ["t-1", "t-3"], "t-1 wins the tie by board order; t-2 (same updated_ms, later position) is dropped");
}

#[test]
fn filter_done_rows_is_a_noop_at_or_under_the_cap() {
    let rows = vec![done_row("t-1", "done", 1), done_row("t-2", "done", 2), done_row("t-3", "queued", 3)];
    let (kept, omitted) = filter_done_rows(rows.clone(), 2);
    assert_eq!(omitted, 0, "exactly at the cap: nothing is elided");
    assert_eq!(kept.len(), 3);
    let (kept, omitted) = filter_done_rows(rows, 5);
    assert_eq!(omitted, 0, "under the cap: nothing is elided");
    assert_eq!(kept.len(), 3);
}

#[test]
fn filter_done_rows_never_touches_a_board_with_no_done_rows() {
    let rows = vec![done_row("t-1", "queued", 1), done_row("t-2", "in-progress", 2)];
    let (kept, omitted) = filter_done_rows(rows, 0);
    assert_eq!(omitted, 0);
    assert_eq!(kept.len(), 2, "a cap of 0 still never elides a non-done row");
}

#[test]
fn task_summaries_for_list_tasks_defaults_to_capped_and_include_all_bypasses_it() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let live = reg.upsert_task(&g.id, "orch", None, patch(Some("still going"), Some("in-progress"), None)).unwrap();
    let total_done = LIST_TASKS_DONE_CAP + 5;
    let mut done_ids = Vec::new();
    for i in 0..total_done {
        let t = reg
            .upsert_task(&g.id, "orch", None, patch(Some(&format!("done-{i}")), Some("done"), None))
            .unwrap();
        done_ids.push(t.id);
    }

    // Default (capped) read.
    let (rows, omitted) = reg.task_summaries_for_list_tasks(&g.id, false, false);
    assert_eq!(omitted, total_done - LIST_TASKS_DONE_CAP, "the omitted count is exact, never approximated");
    let kept_done = rows.iter().filter(|r| r.status == "done").count();
    assert_eq!(kept_done, LIST_TASKS_DONE_CAP, "done rows are capped at the default");
    assert!(rows.iter().any(|r| r.id == live.id), "the one non-done row is never elided by the cap");
    assert_eq!(rows.len(), LIST_TASKS_DONE_CAP + 1, "total rows = capped done + the live row");

    // A dropped row is elided from list_tasks, not deleted from the board —
    // get_task still resolves it and it still counts in the full read below.
    assert!(reg.get_task(&g.id, &done_ids[0]).is_some(), "an elided done row is still on the board");

    // include_all bypasses the cap entirely.
    let (all_rows, all_omitted) = reg.task_summaries_for_list_tasks(&g.id, true, false);
    assert_eq!(all_omitted, 0, "include_all never reports an elision");
    assert_eq!(all_rows.len(), total_done + 1, "include_all returns the whole board");

    // hot_only (#1684) ABOVE the cap — the axis the small 3-done fixture in
    // hot_only_drops_every_done_row_and_still_counts_them cannot reach: with
    // every done row dropped, the count must still name ALL of them (25), not
    // the capped 20 or the overage 5, and only the non-done row survives.
    let (hot_rows, hot_omitted) = reg.task_summaries_for_list_tasks(&g.id, false, true);
    assert_eq!(hot_omitted, total_done, "hot_only counts every done row it dropped, cap or not");
    assert_eq!(hot_rows.len(), 1, "hot_only above the cap returns only the non-done row");
    assert!(hot_rows.iter().any(|r| r.id == live.id), "the non-done row survives hot_only");
}

#[test]
fn list_tasks_mcp_tool_reports_the_omitted_done_count_and_honors_include_all() {
    let (reg, _d, co, cw) = setup_mcp();
    for i in 0..(LIST_TASKS_DONE_CAP + 3) {
        dispatch(&reg, &co, "tools/call",
            &json!({ "name": "upsert_task", "arguments": { "title": format!("done-{i}"), "status": "done" } }))
            .unwrap();
    }
    dispatch(&reg, &co, "tools/call",
        &json!({ "name": "upsert_task", "arguments": { "title": "still active", "status": "queued" } })).unwrap();

    let listed = dispatch(&reg, &cw, "tools/call", &json!({ "name": "list_tasks", "arguments": {} })).unwrap();
    let text = listed["content"][0]["text"].as_str().unwrap();
    let body: Value = serde_json::from_str(text).unwrap();
    assert_eq!(body["omitted_done"], 3, "list_tasks names exactly how many done rows it left off: {text}");
    assert_eq!(body["tasks"].as_array().unwrap().len(), LIST_TASKS_DONE_CAP + 1, "capped done + the one live row");
    assert!(text.contains("still active"), "the non-done row always survives the cap: {text}");

    let full = dispatch(&reg, &cw, "tools/call",
        &json!({ "name": "list_tasks", "arguments": { "include_all": true } })).unwrap();
    let ftext = full["content"][0]["text"].as_str().unwrap();
    let fbody: Value = serde_json::from_str(ftext).unwrap();
    assert_eq!(fbody["omitted_done"], 0, "include_all reports nothing elided: {ftext}");
    assert_eq!(fbody["tasks"].as_array().unwrap().len(), LIST_TASKS_DONE_CAP + 4, "include_all returns every row");
}

#[test]
fn hot_only_drops_every_done_row_and_still_counts_them() {
    // #1684: the per-wake re-sync wants a board with NO done rows in it —
    // not the capped newest 20 — while still being told how many it did not
    // see, so a hot read can never be mistaken for the whole board. Three
    // done rows sit under the cap on purpose: the cap path keeps the newest
    // 20 and would return all three, so only hot_only may drop them.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let mut done_ids = Vec::new();
    for i in 0..3 {
        let t = reg
            .upsert_task(&g.id, "orch", None, patch(Some(&format!("done-{i}")), Some("done"), None))
            .unwrap();
        done_ids.push(t.id);
    }
    let queued_a = reg.upsert_task(&g.id, "orch", None, patch(Some("queued-a"), Some("queued"), None)).unwrap();
    let queued_b = reg.upsert_task(&g.id, "orch", None, patch(Some("queued-b"), Some("queued"), None)).unwrap();

    let (rows, omitted) = reg.task_summaries_for_list_tasks(&g.id, false, true);
    assert_eq!(omitted, 3, "hot_only counts every done row it dropped: {omitted}");
    assert_eq!(rows.len(), 2, "exactly the two non-done rows survive");
    let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
    assert!(ids.contains(&queued_a.id.as_str()) && ids.contains(&queued_b.id.as_str()),
        "both queued rows are present by id: {ids:?}");
    assert!(rows.iter().all(|r| r.status != "done"), "no done row of any age survives hot_only");

    // Dropping is elision, not deletion — same guarantee the cap gives.
    for id in &done_ids {
        assert!(reg.get_task(&g.id, id).is_some(), "an omitted done row is still on the board: {id}");
    }

    // The default read is unchanged by the new flag's existence.
    let (default_rows, default_omitted) = reg.task_summaries_for_list_tasks(&g.id, false, false);
    assert_eq!(default_omitted, 0, "three done rows sit under the cap: nothing is elided by default");
    assert_eq!(default_rows.len(), 5, "the default read returns the whole small board");
}

/// One-paragraph shape check for a user-facing message (#1426 B2 shape; the
/// two leak forms are a `\n` plus indentation shipped from the source literal,
/// or a collapsed `\` continuation leaving a ten-space run with no `\n`).
/// Copied from tests/manager_lifecycle.rs so both suites pin the same shape.
pub(crate) fn is_one_paragraph(msg: &str) -> bool {
    !msg.contains('\n') && !msg.contains("          ")
}

fn assert_one_paragraph(what: &str, msg: &str) {
    assert!(
        is_one_paragraph(msg),
        "{what}: a refusal is one paragraph — the house idiom is a `\\` line \
         continuation, which strips the newline AND the indentation. This one \
         ships a hard break or leaked indentation: {msg:?}"
    );
}

#[test]
fn hot_only_with_include_all_is_refused() {
    // #1684: the two flags answer opposite questions — one returns every
    // done row, the other refuses to carry any — so the quieter of the two
    // must never silently win. The refusal names BOTH flags.
    let (reg, _d, co, _cw) = setup_mcp();
    let refused = dispatch(&reg, &co, "tools/call",
        &json!({ "name": "list_tasks", "arguments": { "hot_only": true, "include_all": true } }))
        .unwrap();
    assert_eq!(refused["isError"], true, "the contradiction must be an error result, not a read: {refused}");
    let text = refused["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("hot_only") && text.contains("include_all"),
        "the refusal must name both flags: {text}");
    // The message is a `\`-continuation literal: pin its SHAPE beside the
    // content, because a `.contains` pin survives a collapsed continuation —
    // no asserted substring straddles the break (#1457's form).
    assert_one_paragraph("hot_only + include_all refusal", text);

    // Either flag alone still reads fine — the refusal is about the PAIR.
    let hot = dispatch(&reg, &co, "tools/call",
        &json!({ "name": "list_tasks", "arguments": { "hot_only": true } })).unwrap();
    assert_eq!(hot["isError"], false, "hot_only alone is a valid read: {hot}");
    let all = dispatch(&reg, &co, "tools/call",
        &json!({ "name": "list_tasks", "arguments": { "include_all": true } })).unwrap();
    assert_eq!(all["isError"], false, "include_all alone is a valid read: {all}");
}

#[test]
fn live_only_omits_dead_agents_and_keeps_live_ones() {
    // #1684: the per-wake re-sync only needs "who is live" — a dead agent's
    // session id already sits on the board rows that resume it, so carrying
    // the dead roster on every wake is payload for nothing. One dead + one
    // live: the default roster returns both, live_only returns exactly the
    // live one.
    let (reg, _d, co, cw) = setup_mcp();
    // setup_mcp's two agents are exactly the fixture the brief asks for once
    // the worker dies: one live (the orchestrator we dispatch as) + one dead.
    reg.mark_dead(&cw.agent_id, None);

    let default = dispatch(&reg, &co, "tools/call", &json!({ "name": "list_agents", "arguments": {} })).unwrap();
    let dtext = default["content"][0]["text"].as_str().unwrap();
    let dbody: Value = serde_json::from_str(dtext).unwrap();
    let drows = dbody.as_array().unwrap();
    assert_eq!(drows.len(), 2, "the default read is unchanged: the dead row stays, dead rows included: {dtext}");
    assert!(drows.iter().any(|a| a["id"] == json!(cw.agent_id) && a["status"] == json!("dead")),
        "the dead agent is on the default roster: {dtext}");

    let live = dispatch(&reg, &co, "tools/call",
        &json!({ "name": "list_agents", "arguments": { "live_only": true } })).unwrap();
    let ltext = live["content"][0]["text"].as_str().unwrap();
    let lbody: Value = serde_json::from_str(ltext).unwrap();
    let lrows = lbody.as_array().unwrap();
    assert_eq!(lrows.len(), 1, "live_only returns exactly the live agent: {ltext}");
    assert!(lrows.iter().all(|a| a["id"] == json!(co.agent_id)),
        "the surviving row is the live orchestrator: {ltext}");
    assert!(lrows.iter().all(|a| a["id"] != json!(cw.agent_id)),
        "the dead agent never survives live_only: {ltext}");
    assert!(lrows.iter().all(|a| a["status"] != json!("dead")),
        "every row live_only returns is a live one: {ltext}");
}

// ---------- #582: dependency links, derived readiness, atomic claim ----------

/// A `deps`-only patch — the shape most link edits take.
fn deps_patch(deps: &[&str]) -> TaskPatch {
    TaskPatch { deps: Some(deps.iter().map(|s| s.to_string()).collect()), ..Default::default() }
}

/// A claim patch: `assignee` + `claim`, the way the orchestrator hands work out.
fn claim_patch(assignee: &str) -> TaskPatch {
    TaskPatch { assignee: Some(assignee.into()), claim: true, ..Default::default() }
}

/// A `Task` literal for the pure readiness functions — no registry, no files.
pub(crate) fn linked(id: &str, status: &str, deps: &[&str], related: &[&str]) -> Task {
    Task {
        id: id.into(),
        title: format!("task {id}"),
        status: status.into(),
        issue: None,
        pr: None,
        pr_base: None,
        assignee: None,
        session: None,
        notes: vec![],
        deps: deps.iter().map(|s| s.to_string()).collect(),
        related: related.iter().map(|s| s.to_string()).collect(),
        parent: None,
        kind: None,
        sprint: None,
        links: vec![],
        demo_path: None,
        description: None,
        cleared_ms: None,
        updated_ms: 0,
    }
}

/// Seed `titles` as queued tasks and return the group.
pub(crate) fn board_with(reg: &OrchRegistry, titles: &[&str]) -> GroupId {
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    for title in titles {
        reg.upsert_task(&g.id, "orch", None, patch(Some(title), None, None)).unwrap();
    }
    g.id
}

/// The #133-adjacent compat guarantee (#582): the link fields are ADDITIVE, so
/// a board written before they existed loads unchanged, and a board that uses
/// no links never grows the keys on rewrite. Both directions matter — the
/// documented compat edge is an OLDER loomux reading a newer file and dropping
/// the unknown fields on its next write, which is only acceptable because a
/// dep-free board is byte-identical either way.
#[test]
fn pre_582_boards_load_unchanged_and_link_free_boards_stay_link_free() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let path = reg.state_root().join(g.id.as_str()).join("tasks.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    // Exactly what loomux wrote before #582 — no deps key, no related key.
    fs::write(
        &path,
        r##"[
  {"id":"t-1","title":"Ship the parser","status":"done","issue":"#7","pr":null,"assignee":"w-2","session":null,"notes":[],"updated_ms":11},
  {"id":"t-2","title":"Wire it up","status":"queued","issue":null,"pr":null,"assignee":null,"session":null,"notes":[],"updated_ms":12}
]"##,
    )
    .unwrap();

    let tasks = reg.tasks(&g.id);
    assert_eq!(tasks.len(), 2, "a pre-#582 board must still load — a parse failure reads as an EMPTY board");
    assert!(
        tasks.iter().all(|t| t.deps.is_empty() && t.related.is_empty()),
        "absent link fields deserialize to empty vecs, not an error"
    );
    // Readiness is derived for a board that never heard of deps, too.
    let rows = reg.task_summaries(&g.id);
    assert!(!rows[0].ready, "a done task is not 'startable'");
    assert!(rows[1].ready, "a queued task with no deps is ready");

    // A rewrite must not GAIN the new keys: that is what keeps an older loomux
    // (and a human reading the file) seeing exactly what it saw before.
    reg.upsert_task(&g.id, "orch", Some("t-2"), patch(None, Some("in-progress"), None)).unwrap();
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.contains("in-progress"), "the edit itself landed");
    assert!(!text.contains("\"deps\""), "a link-free board must not gain a deps key:\n{text}");
    assert!(!text.contains("\"related\""), "...nor a related key:\n{text}");
}

// ---------- #581 slice A: the PR's base branch on the task record ----------

/// The compat half of #581: `pr_base` is additive, so a `tasks.json` written
/// before it existed must load with the field simply absent. This is the whole
/// prerequisite — `tasks()` reports a parse failure as an EMPTY board, so a
/// non-defaulted field would silently erase every live board on upgrade rather
/// than error, exactly the failure mode #582's own compat test was written for.
#[test]
fn pre_581_boards_load_with_pr_base_absent() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let path = reg.state_root().join(g.id.as_str()).join("tasks.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    // Exactly what loomux wrote before #581 — a PR, and no pr_base key at all.
    fs::write(
        &path,
        r##"[
  {"id":"t-1","title":"Ship the parser","status":"pr","issue":"#7","pr":"#712","assignee":"w-2","session":null,"notes":[],"updated_ms":11}
]"##,
    )
    .unwrap();

    let tasks = reg.tasks(&g.id);
    assert_eq!(tasks.len(), 1, "a pre-#581 board must still load — a parse failure reads as an EMPTY board");
    assert_eq!(tasks[0].pr.as_deref(), Some("#712"), "the fields that were there are untouched");
    assert_eq!(
        tasks[0].pr_base, None,
        "an absent pr_base deserializes to None (base UNKNOWN), never an error"
    );
    // And the compact row an orchestrator reads says the same thing, rather
    // than inventing a base for a task that never recorded one.
    let rows = reg.task_summaries(&g.id);
    assert_eq!(rows[0].pr_base, None);
}

/// `pr_base` survives the write→read→summary path an orchestrator actually
/// uses, and clears on the empty string like every other optional text field
/// (`pr`'s idiom) — so "retargeted, base no longer known" is expressible
/// without hand-editing the board.
#[test]
fn pr_base_round_trips_through_the_board_and_clears_on_empty() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let t = reg.upsert_task(&g.id, "orch-1", None, patch(Some("Stack a sub-PR"), None, None)).unwrap();

    let mut p = patch(None, Some("pr"), None);
    p.pr = Some("#712".into());
    p.pr_base = Some("integration/581".into());
    let saved = reg.upsert_task(&g.id, "orch-1", Some(&t.id), p).unwrap();
    assert_eq!(saved.pr_base.as_deref(), Some("integration/581"));

    // Durable, not just in the returned snapshot: re-read from tasks.json.
    let path = reg.state_root().join(g.id.as_str()).join("tasks.json");
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.contains("integration/581"), "pr_base must reach the file:\n{text}");
    let reread = reg.tasks(&g.id);
    assert_eq!(reread[0].pr_base.as_deref(), Some("integration/581"));
    // The projection the MCP board read goes through carries it too — a
    // summary that dropped it would leave the orchestrator re-deriving the
    // base it just recorded.
    assert_eq!(reg.task_summaries(&g.id)[0].pr_base.as_deref(), Some("integration/581"));

    // Empty string clears (same filter as `pr`); omitting the field leaves it.
    let untouched = reg.upsert_task(&g.id, "orch-1", Some(&t.id), patch(None, None, Some("still open"))).unwrap();
    assert_eq!(untouched.pr_base.as_deref(), Some("integration/581"), "omitted means untouched");
    let clear = TaskPatch { pr_base: Some("   ".into()), ..Default::default() };
    let cleared = reg.upsert_task(&g.id, "orch-1", Some(&t.id), clear).unwrap();
    assert_eq!(cleared.pr_base, None, "a blank pr_base clears the field rather than storing whitespace");
}

// ---------- #1091 slice B: demo_path ----------

/// The compat half of #1091 slice B, the `pr_base`/#581 pattern applied to
/// `demo_path`: a `tasks.json` written before this field existed must still
/// load, with the field simply absent — never an error that would read a live
/// board as empty (the same failure mode `pre_581_boards_load_with_pr_base_absent`
/// guards). Also pins the OTHER half of the additive contract, the way #958
/// pins it for `parent`/`kind` in `pre_958_boards_load_and_a_flat_board_never_gains_the_hierarchy_keys`:
/// `demo_path` carries `skip_serializing_if = "Option::is_none"` (unlike
/// `pr`/`pr_base`, which write an explicit `null`), so a board that never sets
/// it must not GAIN the key on a later rewrite either.
#[test]
fn pre_1091_boards_load_with_demo_path_absent() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let path = reg.state_root().join(g.id.as_str()).join("tasks.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    // Exactly what loomux wrote before #1091 slice B — a prototype row, no
    // demo_path key at all.
    fs::write(
        &path,
        r##"[
  {"id":"t-1","title":"Ship the parser","status":"prototype","issue":null,"pr":null,"assignee":"w-2","session":null,"notes":[],"updated_ms":11}
]"##,
    )
    .unwrap();

    let tasks = reg.tasks(&g.id);
    assert_eq!(tasks.len(), 1, "a pre-#1091 board must still load — a parse failure reads as an EMPTY board");
    assert_eq!(tasks[0].status, "prototype", "the fields that were there are untouched");
    assert_eq!(
        tasks[0].demo_path, None,
        "an absent demo_path deserializes to None (no demo recorded), never an error"
    );

    // A rewrite that never touches demo_path must not GAIN the key — the
    // same guarantee #958 pins for `parent`/`kind`, so an older loomux (and a
    // human reading the file) keeps seeing exactly what it saw before.
    reg.upsert_task(&g.id, "orch", Some("t-1"), patch(None, Some("in-progress"), None)).unwrap();
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.contains("in-progress"), "the edit itself landed");
    assert!(!text.contains("\"demo_path\""), "a board that never set demo_path must not gain the key:\n{text}");
}

/// `demo_path` survives the write→read path an orchestrator actually uses
/// (the `upsert_task` engine call `mcp.rs`'s tool arm and the Tauri board-edit
/// sibling both funnel through), and clears on the empty string like every
/// other optional text field (`pr`/`pr_base`'s idiom) — so "the demo moved,
/// no longer at this path" is expressible without hand-editing the board.
#[test]
fn demo_path_round_trips_through_the_board_and_clears_on_empty() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let t = reg.upsert_task(&g.id, "orch-1", None, patch(Some("Demo the redesign"), None, None)).unwrap();

    let mut p = patch(None, Some("prototype"), None);
    p.demo_path = Some("C:/Projects/loomux-worktrees/feat/1091-demo-path".into());
    let saved = reg.upsert_task(&g.id, "orch-1", Some(&t.id), p).unwrap();
    assert_eq!(saved.demo_path.as_deref(), Some("C:/Projects/loomux-worktrees/feat/1091-demo-path"));

    // Durable, not just in the returned snapshot: re-read from tasks.json.
    let path = reg.state_root().join(g.id.as_str()).join("tasks.json");
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.contains("1091-demo-path"), "demo_path must reach the file:\n{text}");
    let reread = reg.tasks(&g.id);
    assert_eq!(reread[0].demo_path.as_deref(), Some("C:/Projects/loomux-worktrees/feat/1091-demo-path"));

    // Empty string clears (same filter as `pr`/`pr_base`); omitting the field
    // leaves it untouched.
    let untouched = reg.upsert_task(&g.id, "orch-1", Some(&t.id), patch(None, None, Some("still there"))).unwrap();
    assert_eq!(
        untouched.demo_path.as_deref(),
        Some("C:/Projects/loomux-worktrees/feat/1091-demo-path"),
        "omitted means untouched"
    );
    let clear = TaskPatch { demo_path: Some("   ".into()), ..Default::default() };
    let cleared = reg.upsert_task(&g.id, "orch-1", Some(&t.id), clear).unwrap();
    assert_eq!(cleared.demo_path, None, "a blank demo_path clears the field rather than storing whitespace");
}

// ---------- #3261: the row's plain-text description ----------

/// The compat half, `demo_path`'s guard applied to `description`: a board
/// written before the field existed must load with it simply absent, and a
/// board that never sets it must not GAIN the key on a later rewrite — the
/// additive promise kept on disk, not only at load.
#[test]
fn pre_3261_boards_load_with_description_absent() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let path = reg.state_root().join(g.id.as_str()).join("tasks.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        r##"[
  {"id":"t-1","title":"Ship the parser","status":"queued","issue":null,"pr":null,"assignee":null,"session":null,"notes":[],"updated_ms":11}
]"##,
    )
    .unwrap();

    let tasks = reg.tasks(&g.id);
    assert_eq!(tasks.len(), 1, "a pre-#3261 board must still load — a parse failure reads as an EMPTY board");
    assert_eq!(tasks[0].description, None, "an absent description deserializes to None, never an error");

    reg.upsert_task(&g.id, "orch", Some("t-1"), patch(None, Some("in-progress"), None)).unwrap();
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.contains("in-progress"), "the edit itself landed");
    assert!(
        !text.contains("\"description\""),
        "a board that never set a description must not gain the key:\n{text}"
    );
}

/// The description survives the write→read path both callers funnel through,
/// and clears on the empty string like every other optional text field.
#[test]
fn description_round_trips_through_the_board_and_clears_on_empty() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let t = reg.upsert_task(&g.id, "orch-1", None, patch(Some("Ship the parser"), None, None)).unwrap();

    let p = TaskPatch {
        description: Some("Reads .orrerix/workflow.yml and hands the blocks to the spawner.".into()),
        ..Default::default()
    };
    let saved = reg.upsert_task(&g.id, "orch-1", Some(&t.id), p).unwrap();
    assert_eq!(
        saved.description.as_deref(),
        Some("Reads .orrerix/workflow.yml and hands the blocks to the spawner.")
    );

    let path = reg.state_root().join(g.id.as_str()).join("tasks.json");
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.contains("hands the blocks to the spawner"), "description must reach the file:\n{text}");
    let reread = reg.tasks(&g.id);
    assert_eq!(
        reread[0].description.as_deref(),
        Some("Reads .orrerix/workflow.yml and hands the blocks to the spawner.")
    );

    // Omitted = untouched.
    let untouched = reg.upsert_task(&g.id, "orch-1", Some(&t.id), patch(None, Some("review"), None)).unwrap();
    assert!(untouched.description.is_some(), "omitting the field must leave it alone");

    // Blank clears, rather than storing whitespace — the `pr` rule.
    let clear = TaskPatch { description: Some("   ".into()), ..Default::default() };
    let cleared = reg.upsert_task(&g.id, "orch-1", Some(&t.id), clear).unwrap();
    assert_eq!(cleared.description, None, "a blank description clears the field");
}

/// An over-long description is REFUSED, not cut — and the refusal writes
/// nothing at all, which is the property that makes it safe to retry.
#[test]
fn an_over_long_description_is_refused_and_nothing_is_written() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let t = reg.upsert_task(&g.id, "orch-1", None, patch(Some("Ship the parser"), None, None)).unwrap();

    // Exactly at the cap: accepted.
    let at_cap = TaskPatch { description: Some("x".repeat(MAX_TASK_DESCRIPTION)), ..Default::default() };
    let ok = reg.upsert_task(&g.id, "orch-1", Some(&t.id), at_cap).unwrap();
    assert_eq!(ok.description.as_ref().map(|d| d.chars().count()), Some(MAX_TASK_DESCRIPTION));

    // One over: refused, and the error names the cap and the length so the fix
    // is one edit rather than a guess.
    let over = TaskPatch {
        description: Some("x".repeat(MAX_TASK_DESCRIPTION + 1)),
        status: Some("review".into()),
        ..Default::default()
    };
    let err = reg.upsert_task(&g.id, "orch-1", Some(&t.id), over).unwrap_err();
    assert!(err.contains(&(MAX_TASK_DESCRIPTION + 1).to_string()), "the error names the length: {err}");
    assert!(err.contains(&MAX_TASK_DESCRIPTION.to_string()), "the error names the cap: {err}");

    // NOTHING was written — not the description it refused, and not the status
    // that rode along in the same patch.
    let after = reg.tasks(&g.id);
    assert_eq!(
        after[0].description.as_ref().map(|d| d.chars().count()),
        Some(MAX_TASK_DESCRIPTION),
        "a refused write must leave the old description exactly as it was"
    );
    assert_eq!(after[0].status, "queued", "a refused write must not land the other fields of its patch");

    // The cap is in CHARACTERS, not bytes: 500 em dashes are 1500 bytes and
    // are accepted, which is this repo's own prose style not being refused.
    let wide = TaskPatch { description: Some("—".repeat(MAX_TASK_DESCRIPTION)), ..Default::default() };
    assert!(reg.upsert_task(&g.id, "orch-1", Some(&t.id), wide).is_ok(), "the cap counts characters, not bytes");
}

/// A description is ONE line of plain text, and a multi-line one is refused
/// rather than flattened — welding two sentences together loses the point the
/// second one made.
#[test]
fn a_multi_line_description_is_refused_rather_than_flattened() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let t = reg.upsert_task(&g.id, "orch-1", None, patch(Some("Ship the parser"), None, None)).unwrap();

    for bad in ["One line.\nAnd another.", "One line.\r\nAnd another.", "One line.\tIndented."] {
        let p = TaskPatch { description: Some(bad.into()), ..Default::default() };
        let err = reg.upsert_task(&g.id, "orch-1", Some(&t.id), p).unwrap_err();
        assert!(err.contains("one line"), "the refusal must say what the field is: {err}");
        assert_eq!(reg.tasks(&g.id)[0].description, None, "nothing was written for {bad:?}");
    }

    // The negative control: prose this repo really writes is not a control
    // character, and must be accepted — an em dash, curly quotes, an accent.
    let good = TaskPatch {
        description: Some("Parses the workflow file — the “blocks” list, naïvely.".into()),
        ..Default::default()
    };
    assert!(reg.upsert_task(&g.id, "orch-1", Some(&t.id), good).is_ok());
}

/// The SPLIT: the description rides the full-record reads and never the
/// compact one. It is written for a human, and `list_tasks` rides every row of
/// every board read an orchestrator makes.
#[test]
fn the_description_rides_get_task_and_never_the_compact_list_row() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let t = reg.upsert_task(&g.id, "orch-1", None, patch(Some("Ship the parser"), None, None)).unwrap();
    let p = TaskPatch { description: Some("Hands the blocks to the spawner.".into()), ..Default::default() };
    reg.upsert_task(&g.id, "orch-1", Some(&t.id), p).unwrap();
    let stored = reg.tasks(&g.id).remove(0);

    // list_tasks' row: absent, and the whole row must not carry the text under
    // any other key either — which is what a raw scan of the serialized row
    // says and a per-key assertion would not.
    let summary = serde_json::to_value(task_summary(&stored, true, 0, 0)).unwrap();
    assert!(
        !summary.to_string().contains("Hands the blocks"),
        "the compact row must not carry the description:\n{summary}"
    );

    // get_task's record: present.
    let view = serde_json::to_value(agent_task_view(&stored)).unwrap();
    assert_eq!(
        view.get("description").and_then(|v| v.as_str()),
        Some("Hands the blocks to the spawner."),
        "the full-record read is where an agent reads one back:\n{view}"
    );

    // And the human board's row carries it on every row, expanded or not —
    // the board is what decides to SHOW it only when the row is open.
    for with_notes in [false, true] {
        let board = serde_json::to_value(board_task(stored.clone(), with_notes)).unwrap();
        assert_eq!(
            board.get("description").and_then(|v| v.as_str()),
            Some("Hands the blocks to the spawner."),
            "with_notes={with_notes}: the board row carries it:\n{board}"
        );
    }

    // A row with none omits the key entirely on all three, so a board that
    // never uses the feature pays nothing for it.
    let bare = reg.upsert_task(&g.id, "orch-1", None, patch(Some("No description here"), None, None)).unwrap();
    for (what, v) in [
        ("summary", serde_json::to_value(task_summary(&bare, true, 0, 0)).unwrap()),
        ("agent view", serde_json::to_value(agent_task_view(&bare)).unwrap()),
        ("board row", serde_json::to_value(board_task(bare.clone(), false)).unwrap()),
    ] {
        assert!(v.get("description").is_none(), "{what} gained a dead description key:\n{v}");
    }
}

/// One text, ONE answer, whichever caller sends it (#3261 review round 1,
/// premortem 1).
///
/// The board's editor trims before it sends and MCP does not. While the check
/// read the RAW value, `upsert_task(description: "Ship it.\n")` was REFUSED
/// from an agent and the identical paste into the human's own box was SAVED —
/// one field, two callers, two outcomes, and nothing pinned either half.
#[test]
fn a_description_is_validated_and_stored_trimmed() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let t = reg.upsert_task(&g.id, "orch-1", None, patch(Some("Ship the parser"), None, None)).unwrap();

    // The agent's raw value, with the trailing newline a paste carries.
    let p = TaskPatch { description: Some("Ship it.\n".into()), ..Default::default() };
    let agent_wrote = reg.upsert_task(&g.id, "orch-1", Some(&t.id), p).unwrap();
    assert_eq!(
        agent_wrote.description.as_deref(),
        Some("Ship it."),
        "a trailing newline is trimmed, not refused — and not stored"
    );

    // The human board's own path, which trimmed before sending: same result.
    let h = TaskPatch { description: Some("Ship it.".into()), ..Default::default() };
    let human_wrote = reg.upsert_task_by_human(&g.id, "human", Some(&t.id), h).unwrap();
    assert_eq!(
        human_wrote.description, agent_wrote.description,
        "the two callers must not be able to disagree about one text"
    );

    // Surrounding whitespace never reaches the file either — tasks.json is a
    // file humans read and diff.
    let pad = TaskPatch { description: Some("   Padded.   ".into()), ..Default::default() };
    let saved = reg.upsert_task(&g.id, "orch-1", Some(&t.id), pad).unwrap();
    assert_eq!(saved.description.as_deref(), Some("Padded."));
    let text = fs::read_to_string(reg.state_root().join(g.id.as_str()).join("tasks.json")).unwrap();
    assert!(!text.contains("   Padded"), "untrimmed text must not reach the board file:\n{text}");

    // Trimming is NOT a licence to flatten: an interior control character is
    // still refused, which is the whole point of the refusal.
    let inner = TaskPatch { description: Some("Ship it.\nThen ship more.".into()), ..Default::default() };
    assert!(reg.upsert_task(&g.id, "orch-1", Some(&t.id), inner).is_err());
}

/// The POLL-PAYLOAD RESIDUAL, measured (#3261 review round 1, premortem 2).
///
/// `BoardTask` carries the description on EVERY row rather than only the ones
/// the caller named, which is the opposite call from `notes`. The argument is
/// that its weight is bounded by ROW COUNT alone — a description is written
/// once and capped, where note bodies accumulate for the life of the board —
/// and every payload test so far ran on rows that had no description at all,
/// so the argument went untested exactly where it matters.
///
/// This fills a board the way a bulk writer would and states the bound as a
/// number. If the description ever costs more than the cap accounts for, the
/// argument for carrying it whole is gone and this test says so.
#[test]
fn a_board_full_of_descriptions_stays_bounded_by_row_count() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();

    const ROWS: usize = 64;
    let filled = "d".repeat(MAX_TASK_DESCRIPTION);
    for i in 0..ROWS {
        let t = reg
            .upsert_task(&g.id, "orch-1", None, patch(Some(&format!("row {i}")), None, None))
            .unwrap();
        let p = TaskPatch { description: Some(filled.clone()), ..Default::default() };
        reg.upsert_task(&g.id, "orch-1", Some(&t.id), p).unwrap();
    }

    let board: Vec<_> = reg.tasks(&g.id).into_iter().map(|t| board_task(t, false)).collect();
    assert_eq!(board.len(), ROWS, "the fixture must really be a full board");
    let bytes = serde_json::to_string(&board).unwrap().len();

    // The bound: per row, the description contributes at most the cap plus its
    // key. Nothing here is a function of how long the group has RUN.
    let ceiling = ROWS * (MAX_TASK_DESCRIPTION + 256);
    assert!(
        bytes < ceiling,
        "a {ROWS}-row board of maximal descriptions serialized to {bytes} bytes, over the \
         {ceiling}-byte row-count bound — the 'bounded by row count alone' argument is false"
    );

    // The control that makes the number mean something: the SAME board with no
    // descriptions. A bound that held because the field was absent would pass
    // the assertion above while saying nothing at all.
    let bare: Vec<_> = reg
        .tasks(&g.id)
        .into_iter()
        .map(|mut t| {
            t.description = None;
            board_task(t, false)
        })
        .collect();
    let bare_bytes = serde_json::to_string(&bare).unwrap().len();
    let grew = bytes - bare_bytes;
    assert!(
        grew >= ROWS * MAX_TASK_DESCRIPTION,
        "descriptions added only {grew} bytes over {ROWS} maximal rows — the fixture is not \
         measuring what it claims to"
    );
    assert!(
        grew <= ROWS * (MAX_TASK_DESCRIPTION + 64),
        "descriptions added {grew} bytes, more than the cap accounts for"
    );
}

// ---------------------------------------------------------------------------
// #1152: "clear completed items" — the human's ARCHIVE action on their own
// board. The whole feature rests on it never being a delete, and on it never
// reaching into what agents read.
// ---------------------------------------------------------------------------

/// Clearing stamps the done rows and touches nothing else — not the board's
/// membership, not its order, not the rows' own content. The failure this pins
/// is the obvious one: a "clear" implemented as a delete would satisfy the
/// human's ask on screen and destroy the traceability the issue asks for in the
/// same click.
#[test]
fn clearing_done_tasks_archives_them_and_deletes_nothing() {
    let (reg, _d) = test_registry();
    let g = board_with(&reg, &["ship it", "review it", "old work", "older work"]);
    for id in ["t-3", "t-4"] {
        reg.upsert_task(&g, "orch", Some(id), patch(None, Some("done"), None)).unwrap();
    }
    reg.upsert_task(&g, "orch", Some("t-2"), patch(None, Some("in-progress"), Some("note"))).unwrap();

    let cleared = reg.clear_done_tasks(&g, "human").unwrap();
    assert_eq!(cleared, vec!["t-3".to_string(), "t-4".to_string()], "exactly the done rows, in board order");

    let after = reg.tasks(&g);
    assert_eq!(
        after.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
        vec!["t-1", "t-2", "t-3", "t-4"],
        "every row is still on the board, in the order it was in — nothing was deleted or moved"
    );
    assert!(after[2].cleared_ms.is_some() && after[3].cleared_ms.is_some(), "the done rows carry the stamp");
    assert!(after[0].cleared_ms.is_none() && after[1].cleared_ms.is_none(), "live rows are untouched");
    assert_eq!(after[1].notes.len(), 1, "content is untouched too");

    // Idempotent: a second click has nothing to archive and rewrites no stamp.
    let stamp = after[2].cleared_ms;
    assert!(reg.clear_done_tasks(&g, "human").unwrap().is_empty(), "nothing left to clear");
    assert_eq!(reg.tasks(&g)[2].cleared_ms, stamp, "an already-cleared row keeps its original archive date");
}

/// Clearing composes with the `list_tasks` done-cap instead of fighting it
/// (#865): the cap keeps the newest `LIST_TASKS_DONE_CAP` done rows **by
/// `updated_ms`**, so a clear that stamped a fresh `updated_ms` onto 250 rows
/// would silently rewrite which twenty the orchestrator sees next. The property
/// is that a human view action changes nothing an agent reads.
#[test]
fn clearing_does_not_disturb_what_list_tasks_shows_the_orchestrator() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    for i in 0..(LIST_TASKS_DONE_CAP + 5) {
        let title = format!("task {i}");
        reg.upsert_task(&g.id, "orch", None, patch(Some(&title), None, None)).unwrap();
    }
    // Every row done, so the board is genuinely over the cap and the kept set
    // is decided by `updated_ms` rather than by there being little to choose.
    let ids: Vec<String> = reg.tasks(&g.id).iter().map(|t| t.id.clone()).collect();
    for id in &ids {
        reg.upsert_task(&g.id, "orch", Some(id.as_str()), patch(None, Some("done"), None)).unwrap();
    }
    let before = reg.tasks(&g.id);
    let (kept_before, omitted_before) = filter_done_rows(board_summaries(&before), LIST_TASKS_DONE_CAP);
    let stamps_before: Vec<u64> = before.iter().map(|t| t.updated_ms).collect();

    reg.clear_done_tasks(&g.id, "human").unwrap();

    let after = reg.tasks(&g.id);
    assert_eq!(
        after.iter().map(|t| t.updated_ms).collect::<Vec<_>>(),
        stamps_before,
        "clearing must not touch updated_ms — it is the cap's own sort key"
    );
    let (kept_after, omitted_after) = filter_done_rows(board_summaries(&after), LIST_TASKS_DONE_CAP);
    assert_eq!(omitted_after, omitted_before, "the same number of rows is elided");
    assert!(omitted_before > 0, "the specimen must actually be over the cap, or this proves nothing");
    assert_eq!(
        kept_after.iter().map(|r| r.id.clone()).collect::<Vec<_>>(),
        kept_before.iter().map(|r| r.id.clone()).collect::<Vec<_>>(),
        "and the SAME twenty rows are the ones kept"
    );
}

/// The archive is reversible, per id, and never wider than the ids asked for.
#[test]
fn restoring_clears_the_stamp_for_exactly_the_named_rows() {
    let (reg, _d) = test_registry();
    let g = board_with(&reg, &["a", "b", "c"]);
    for id in ["t-1", "t-2", "t-3"] {
        reg.upsert_task(&g, "orch", Some(id), patch(None, Some("done"), None)).unwrap();
    }
    reg.clear_done_tasks(&g, "human").unwrap();

    // A real id, an id that names nothing, and an id that was never cleared:
    // only the first moves, and the miss is skipped rather than fatal.
    let restored = reg
        .restore_cleared_tasks(&g, "human", &["t-2".into(), "t-404".into()])
        .unwrap();
    assert_eq!(restored, vec!["t-2".to_string()]);
    let after = reg.tasks(&g);
    assert!(after[1].cleared_ms.is_none(), "t-2 is back in the working list");
    assert!(after[0].cleared_ms.is_some() && after[2].cleared_ms.is_some(), "its siblings stayed archived");
    assert!(
        reg.restore_cleared_tasks(&g, "human", &["t-2".into()]).unwrap().is_empty(),
        "restoring a row that carries no stamp changes nothing"
    );
}

/// The board's own `orch_upsert_task` path can set and clear the stamp per row
/// (that is what the per-row ↩ rides on), and — the property that keeps a
/// reopened task visible — reopening and un-archiving in ONE patch ends with
/// the stamp gone whichever order the caller wrote the arguments in.
#[test]
fn the_archive_stamp_is_settable_and_clearable_through_a_board_patch() {
    let (reg, _d) = test_registry();
    let g = board_with(&reg, &["a"]);
    let done = TaskPatch { status: Some("done".into()), cleared: Some(true), ..Default::default() };
    let t = reg.upsert_task(&g, "human", Some("t-1"), done).unwrap();
    assert!(t.cleared_ms.is_some(), "the human board can archive a single row");

    let untouched = reg.upsert_task(&g, "human", Some("t-1"), patch(None, None, Some("hi"))).unwrap();
    assert_eq!(untouched.cleared_ms, t.cleared_ms, "omitting `cleared` leaves it alone");

    let reopen = TaskPatch {
        status: Some("in-progress".into()),
        cleared: Some(false),
        ..Default::default()
    };
    let back = reg.upsert_task(&g, "human", Some("t-1"), reopen).unwrap();
    assert_eq!(back.cleared_ms, None, "reopen + un-archive in one write leaves no stamp behind");
    assert_eq!(back.status, "in-progress");
}

/// A board written before #1152 loads with no stamp, and a rewrite that never
/// archives anything must not GAIN the key — the same additive guarantee
/// `parent`/`kind`/`demo_path` carry, so an older loomux (and a human reading
/// `tasks.json`) keeps seeing exactly what it saw before.
#[test]
fn pre_1152_boards_load_with_cleared_ms_absent_and_do_not_gain_the_key() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let path = reg.state_root().join(g.id.as_str()).join("tasks.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        r##"[
  {"id":"t-1","title":"Ship the parser","status":"done","issue":null,"pr":null,"assignee":"w-2","session":null,"notes":[],"updated_ms":11}
]"##,
    )
    .unwrap();

    let tasks = reg.tasks(&g.id);
    assert_eq!(tasks.len(), 1, "a pre-#1152 board must still load — a parse failure reads as an EMPTY board");
    assert_eq!(tasks[0].cleared_ms, None, "an absent cleared_ms deserializes to None, never an error");

    reg.upsert_task(&g.id, "orch", Some("t-1"), patch(None, Some("in-progress"), None)).unwrap();
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.contains("in-progress"), "the edit itself landed");
    assert!(!text.contains("\"cleared_ms\""), "a board that never archived must not gain the key:\n{text}");
}

/// The archive is the HUMAN's view of their own board: no MCP tool writes it,
/// and none reads it back. An agent tidying rows out of the human's sight is
/// the one thing this feature must never become — and the compact `list_tasks`
/// row deliberately does not carry the field either, so nothing agent-facing
/// can start gating on it by accident.
#[test]
fn no_mcp_tool_can_archive_a_row_or_see_that_one_was_archived() {
    let (reg, _d, co, _cw) = setup_mcp();
    let created = dispatch(&reg, &co, "tools/call",
        &json!({ "name": "upsert_task", "arguments": { "title": "Ship it", "status": "done" } }))
        .unwrap();
    assert_eq!(created["isError"], false, "setup: {created}");
    let group = co.group.clone();

    // The tool surface offers no way in: `cleared` is not a documented argument
    // of any tool, and passing it anyway archives nothing.
    let schemas = dispatch(&reg, &co, "tools/list", &json!({})).unwrap().to_string();
    assert!(
        !schemas.contains("\"cleared\""),
        "no MCP tool may advertise the human board's archive field:\n{schemas}"
    );
    let _ = dispatch(&reg, &co, "tools/call",
        &json!({ "name": "upsert_task", "arguments": { "id": "t-1", "cleared": true } }));
    assert_eq!(
        reg.tasks(&group)[0].cleared_ms, None,
        "an agent passing the field anyway must not archive a row on the human's board"
    );

    // Nor can an agent tell that the human archived one. BOTH read paths, not
    // just the compact one: `get_task` used to serialize the stored `Task`
    // straight out, so it shipped `cleared_ms` the moment a stamp existed —
    // review round 1's blocking finding, and the reason the assertion below
    // exists at all. The test name claims the whole property; it has to check
    // every surface that could break it.
    reg.clear_done_tasks(&group, "human").unwrap();
    assert!(reg.tasks(&group)[0].cleared_ms.is_some(), "the human's clear did land");

    let listed = dispatch(&reg, &co, "tools/call", &json!({ "name": "list_tasks", "arguments": {} }))
        .unwrap()
        .to_string();
    assert!(!listed.contains("cleared"), "list_tasks must not leak the archive marker: {listed}");
    assert!(listed.contains("t-1"), "and the archived row is still listed for the orchestrator");

    let got = dispatch(&reg, &co, "tools/call",
        &json!({ "name": "get_task", "arguments": { "id": "t-1" } }))
        .unwrap()
        .to_string();
    // Completeness FIRST, absence second, and the order is load-bearing.
    // `get_task` exists to carry the note history `list_tasks` omits, so a
    // projection that "fixed" the leak by returning less than agents need is
    // the other failure direction and has to be caught separately. Asserting
    // absence first would abort the test on the leak and leave these two
    // unreached — which is the whole point of the rule that a red evidences
    // only the assertion it REACHED and MOVED. This way the leak mutation runs
    // them, they pass, and only the absence assertion below moves: the round
    // then proves the test can tell the two directions apart, instead of
    // merely asserting that it can.
    assert!(got.contains("t-1") && got.contains("\\\"notes\\\""), "get_task is still the full record: {got}");
    assert!(got.contains("done"), "...including the status: {got}");
    assert!(!got.contains("cleared"), "get_task must not leak the archive marker either: {got}");
}

/// The MCP surface #1091 slice B adds: `demo_path` is settable through the
/// SAME `upsert_task` tool as every other field (D2's "extend, don't add a
/// second tool" posture applied here too), and reads back through `get_task`
/// — the full-record read the not-yet-built NEEDS-YOU panel (slice C) and the
/// human board's `orch_tasks` both use. It deliberately does NOT reach the
/// compact `list_tasks` projection: #245 keeps that row minimal on purpose,
/// and slice B's plan (D7) never asked to widen it.
#[test]
fn demo_path_is_settable_through_the_upsert_task_tool_and_omitted_from_the_compact_list() {
    let (reg, _d, co, cw) = setup_mcp();
    let created = dispatch(&reg, &co, "tools/call",
        &json!({ "name": "upsert_task", "arguments": { "title": "Demo the redesign", "status": "prototype" } }))
        .unwrap();
    assert_eq!(created["isError"], false);
    let id = created["content"][0]["text"].as_str().unwrap().split_whitespace().next().unwrap().to_string();

    let updated = dispatch(&reg, &co, "tools/call",
        &json!({ "name": "upsert_task", "arguments": { "id": id, "demo_path": "C:/Projects/loomux-worktrees/feat/1091-demo-path" } }))
        .unwrap();
    assert_eq!(updated["isError"], false);

    let detail = dispatch(&reg, &cw, "tools/call",
        &json!({ "name": "get_task", "arguments": { "id": id } })).unwrap();
    let dtext = detail["content"][0]["text"].as_str().unwrap();
    assert!(dtext.contains("1091-demo-path"), "get_task must surface demo_path: {dtext}");

    let listed = dispatch(&reg, &cw, "tools/call",
        &json!({ "name": "list_tasks", "arguments": {} })).unwrap();
    let ltext = listed["content"][0]["text"].as_str().unwrap();
    assert!(
        !ltext.contains("1091-demo-path"),
        "the compact list_tasks row must not carry demo_path (#245): {ltext}"
    );
}

#[test]
fn link_writes_are_validated_and_normalized() {
    let (reg, _d) = test_registry();
    let gid = board_with(&reg, &["a", "b", "c"]);

    // An id naming no task on the board is refused, and the error says which.
    let err = reg.upsert_task(&gid, "orch", Some("t-3"), deps_patch(&["t-99"])).unwrap_err();
    assert!(err.contains("t-99"), "the rejection names the unknown id: {err}");
    // Self-links are refused on both arrays.
    let err = reg.upsert_task(&gid, "orch", Some("t-3"), deps_patch(&["t-3"])).unwrap_err();
    assert!(err.contains("itself"), "a self-link is refused: {err}");
    let self_related = TaskPatch { related: Some(vec!["t-3".into()]), ..Default::default() };
    assert!(reg.upsert_task(&gid, "orch", Some("t-3"), self_related).is_err(), "related is checked too");
    // Nothing a rejection touched was written.
    assert!(
        reg.get_task(&gid, "t-3").unwrap().deps.is_empty(),
        "a refused link write must leave the board exactly as it was"
    );

    // Normalization: trim, drop blanks, dedup (first occurrence wins), keep order.
    let messy = TaskPatch {
        deps: Some(vec!["t-2".into(), " t-1 ".into(), "t-2".into(), "   ".into()]),
        ..Default::default()
    };
    let t = reg.upsert_task(&gid, "orch", Some("t-3"), messy).unwrap();
    assert_eq!(t.deps, ["t-2", "t-1"], "trimmed, deduped, blank dropped, order preserved");

    // Arrays REPLACE: an edit that doesn't mention them leaves them alone,
    // and an empty array is the explicit clear.
    let t = reg.upsert_task(&gid, "orch", Some("t-3"), patch(None, None, Some("unrelated edit"))).unwrap();
    assert_eq!(t.deps, ["t-2", "t-1"], "a patch that omits deps must not clear them");
    let t = reg.upsert_task(&gid, "orch", Some("t-3"), deps_patch(&[])).unwrap();
    assert!(t.deps.is_empty(), "[] clears the array");
}

#[test]
fn dep_cycles_are_rejected_and_the_error_names_the_path() {
    let (reg, _d) = test_registry();
    let gid = board_with(&reg, &["a", "b", "c", "d"]);

    // Two-task cycle: t-2 → t-1 is fine; the edge that closes it is not.
    reg.upsert_task(&gid, "orch", Some("t-2"), deps_patch(&["t-1"])).unwrap();
    let err = reg.upsert_task(&gid, "orch", Some("t-1"), deps_patch(&["t-2"])).unwrap_err();
    assert!(err.contains("cycle"), "the refusal says what it is: {err}");
    assert!(err.contains("t-1 → t-2 → t-1"), "the error names the cycle path: {err}");

    // Three-task cycle, through a chain the write never mentions.
    reg.upsert_task(&gid, "orch", Some("t-3"), deps_patch(&["t-2"])).unwrap();
    let err = reg.upsert_task(&gid, "orch", Some("t-1"), deps_patch(&["t-3"])).unwrap_err();
    assert!(err.contains("t-1 → t-3 → t-2 → t-1"), "the whole path, in order: {err}");
    assert!(
        reg.get_task(&gid, "t-1").unwrap().deps.is_empty(),
        "the rejected edge is not written"
    );

    // A DIAMOND is not a cycle: t-4 depends on both t-2 and t-3, which both
    // reach t-1. Re-converging paths must not be mistaken for a loop.
    reg.upsert_task(&gid, "orch", Some("t-4"), deps_patch(&["t-2", "t-3"])).unwrap();

    // `related` is never cycle-checked — a mutual see-also pair is meaningful.
    let see_also = |other: &str| TaskPatch { related: Some(vec![other.to_string()]), ..Default::default() };
    reg.upsert_task(&gid, "orch", Some("t-1"), see_also("t-2")).unwrap();
    reg.upsert_task(&gid, "orch", Some("t-2"), see_also("t-1")).unwrap();
}

#[test]
fn readiness_counts_only_done_deps_and_ignores_related() {
    let board = vec![
        linked("t-1", "done", &[], &[]),
        linked("t-2", "pr", &[], &[]),
        linked("t-3", "queued", &["t-1"], &[]),
        linked("t-4", "queued", &["t-1", "t-2"], &[]),
        linked("t-5", "in-progress", &["t-1"], &[]),
        linked("t-6", "queued", &[], &["t-2"]),
        linked("t-7", "queued", &["t-404"], &[]),
    ];
    assert!(task_ready(&board[2], &board), "queued with every dep done = ready");
    assert!(
        !task_ready(&board[3], &board),
        "a dep at `pr` is NOT done — merged/accepted is the bar, so its dependent stays blocked"
    );
    assert_eq!(unmet_deps(&board[3], &board), ["t-2"], "only the unmet dep is named");
    assert!(!task_ready(&board[4], &board), "only a queued task can be ready");
    assert!(task_ready(&board[5], &board), "a related link never blocks");
    assert!(
        !task_ready(&board[6], &board),
        "an id naming no live task counts as UNMET — reading a typo as satisfied would silently unblock work"
    );

    // Every non-`done` status leaves a dependent blocked; `done` releases it.
    for status in ["queued", "in-progress", "review", "pr", "prototype", "human-testing", "blocked"] {
        let b = vec![linked("t-1", status, &[], &[]), linked("t-2", "queued", &["t-1"], &[])];
        assert!(!task_ready(&b[1], &b), "a dep sitting in {status} must not satisfy a dependency");
    }
    let b = vec![linked("t-1", "done", &[], &[]), linked("t-2", "queued", &["t-1"], &[])];
    assert!(task_ready(&b[1], &b), "done — and only done — releases it");
}

#[test]
fn list_rows_carry_link_ids_only_and_a_derived_ready() {
    let board = vec![linked("t-1", "done", &[], &[]), linked("t-2", "queued", &["t-1"], &["t-1"])];
    let rows = board_summaries(&board);
    assert!(rows[1].ready, "board context is what makes `ready` computable at all");

    let row = serde_json::to_value(&rows[1]).unwrap();
    assert_eq!(row["deps"], json!(["t-1"]), "ids only");
    assert_eq!(row["related"], json!(["t-1"]));
    assert_eq!(row["ready"], json!(true));
    // #245's size constraint: a link must never expand into the linked task.
    assert!(
        !row.to_string().contains("task t-1"),
        "a link must carry the id alone, never the linked task's title: {row}"
    );

    // A link-free row doesn't pay for the fields at all — but `ready` is always
    // present, because an absent flag would read as "unknown", not "false".
    let plain = serde_json::to_value(&rows[0]).unwrap();
    assert!(plain.get("deps").is_none() && plain.get("related").is_none(), "empty arrays are omitted: {plain}");
    assert_eq!(plain["ready"], json!(false));
}

#[test]
fn claim_is_atomic_and_guarded() {
    let (reg, _d) = test_registry();
    let gid = board_with(&reg, &["migrate schema", "consume schema", "unrelated"]);
    reg.upsert_task(&gid, "orch", Some("t-2"), deps_patch(&["t-1"])).unwrap();

    // Blocked: refused, naming the dep that holds it, and NOTHING is written.
    let err = reg.upsert_task(&gid, "orch", Some("t-2"), claim_patch("w-3")).unwrap_err();
    assert!(err.contains("t-1"), "the refusal names the unmet dep: {err}");
    let t2 = reg.get_task(&gid, "t-2").unwrap();
    assert_eq!(t2.status, "queued", "a refused claim writes nothing");
    assert!(t2.assignee.is_none(), "...not even the assignee it was asked to set");

    // Unblocked: assignee + session + status all land in ONE call.
    let claim = TaskPatch {
        assignee: Some("w-3".into()),
        session: Some("sess-9".into()),
        claim: true,
        ..Default::default()
    };
    let t = reg.upsert_task(&gid, "orch-1", Some("t-1"), claim).unwrap();
    assert_eq!(t.status, "in-progress", "claiming IS the transition");
    assert_eq!(t.assignee.as_deref(), Some("w-3"));
    assert_eq!(t.session.as_deref(), Some("sess-9"));
    // Audited under its own action, so the record says why the assignee moved.
    let claims: Vec<_> = reg
        .audit_log(&gid)
        .into_iter()
        .filter(|e| e.action == "task-claim")
        .collect();
    assert_eq!(claims.len(), 1, "one claim, one task-claim row");
    assert_eq!(claims[0].detail["id"], "t-1");
    assert_eq!(claims[0].detail["assignee"], "w-3");

    // A second agent cannot take it, and the refusal names who holds it.
    let err = reg.upsert_task(&gid, "orch-1", Some("t-1"), claim_patch("w-4")).unwrap_err();
    assert!(err.contains("w-3"), "the refusal names the holder: {err}");
    assert_eq!(reg.get_task(&gid, "t-1").unwrap().assignee.as_deref(), Some("w-3"), "unchanged");

    // The same agent re-claiming is idempotent — this is the post-compact
    // "did my claim land?" retry, and it must not error.
    let again = reg.upsert_task(&gid, "orch-1", Some("t-1"), claim_patch("w-3")).unwrap();
    assert_eq!((again.status.as_str(), again.assignee.as_deref()), ("in-progress", Some("w-3")));

    // A task past `queued` and held by nobody still can't be claimed.
    reg.upsert_task(&gid, "orch-1", Some("t-3"), patch(None, Some("review"), None)).unwrap();
    let err = reg.upsert_task(&gid, "orch-1", Some("t-3"), claim_patch("w-5")).unwrap_err();
    assert!(err.contains("review"), "the refusal names the status in the way: {err}");

    // Claim needs an existing row, and cannot contradict itself on status.
    let create_claim = TaskPatch { title: Some("new".into()), claim: true, ..Default::default() };
    assert!(reg.upsert_task(&gid, "orch-1", None, create_claim).is_err(), "nothing to guard on a create");
    let contradictory = TaskPatch { status: Some("done".into()), claim: true, ..Default::default() };
    assert!(
        reg.upsert_task(&gid, "orch-1", Some("t-2"), contradictory).is_err(),
        "a claim that also sets a different status would make one argument a lie"
    );

    // A PLAIN assignee write is untouched by all of this: the human's board and
    // every pre-#582 caller keep last-writer-wins.
    let steal = TaskPatch { assignee: Some("w-9".into()), ..Default::default() };
    let t = reg.upsert_task(&gid, "human", Some("t-1"), steal).unwrap();
    assert_eq!(t.assignee.as_deref(), Some("w-9"), "an unguarded write still wins — the guard is opt-in");
}

#[test]
fn deleting_a_task_strips_it_from_every_remaining_link() {
    let (reg, _d) = test_registry();
    let gid = board_with(&reg, &["a", "b", "c", "d", "e"]);
    // t-2 is blocked by t-1; t-3 merely relates to t-1.
    reg.upsert_task(&gid, "orch", Some("t-2"), deps_patch(&["t-1"])).unwrap();
    let rel = TaskPatch { related: Some(vec!["t-1".into()]), ..Default::default() };
    reg.upsert_task(&gid, "orch", Some("t-3"), rel).unwrap();
    assert!(!reg.task_summaries(&gid).iter().find(|r| r.id == "t-2").unwrap().ready);

    reg.delete_task(&gid, "human", "t-1").unwrap();
    assert!(reg.get_task(&gid, "t-2").unwrap().deps.is_empty(), "a deleted id must not survive on deps");
    assert!(reg.get_task(&gid, "t-3").unwrap().related.is_empty(), "...or on related");
    assert!(
        reg.task_summaries(&gid).iter().find(|r| r.id == "t-2").unwrap().ready,
        "with its blocker gone, the dependent is startable — not blocked forever by a dangling id"
    );
    // The audit says whose links moved.
    let del = reg.audit_log(&gid).into_iter().find(|e| e.action == "task-delete").unwrap();
    assert_eq!(del.detail["relinked"], json!(["t-2", "t-3"]), "the rewritten tasks are named: {}", del.detail);

    // Batch delete (multi-select) strips too.
    reg.upsert_task(&gid, "orch", Some("t-5"), deps_patch(&["t-4"])).unwrap();
    reg.delete_tasks(&gid, "human", &["t-4".to_string()]).unwrap();
    assert!(reg.get_task(&gid, "t-5").unwrap().deps.is_empty(), "delete-selected strips links");
    let sel = reg.audit_log(&gid).into_iter().find(|e| e.action == "task-delete-selected").unwrap();
    assert_eq!(sel.detail["relinked"], json!(["t-5"]));

    // And so does delete-all-done — the most likely way a dep disappears.
    reg.upsert_task(&gid, "orch", Some("t-2"), deps_patch(&["t-3"])).unwrap();
    reg.upsert_task(&gid, "orch", Some("t-3"), patch(None, Some("done"), None)).unwrap();
    reg.delete_done_tasks(&gid, "human").unwrap();
    assert!(reg.get_task(&gid, "t-2").unwrap().deps.is_empty(), "delete-all-done strips links");
    let done = reg.audit_log(&gid).into_iter().find(|e| e.action == "task-delete-done").unwrap();
    assert_eq!(done.detail["relinked"], json!(["t-2"]));
}

#[test]
fn link_and_claim_args_round_trip_through_the_mcp_shim() {
    let (reg, _d, co, cw) = setup_mcp();
    let call = |c: &Caller, args: Value| {
        dispatch(&reg, c, "tools/call", &json!({ "name": "upsert_task", "arguments": args })).unwrap()
    };
    let text_of = |r: &Value| r["content"][0]["text"].as_str().unwrap_or_default().to_string();
    call(&co, json!({ "title": "migrate schema" }));
    call(&co, json!({ "title": "consume schema" }));

    // Arrays parse and land.
    assert_eq!(call(&co, json!({ "id": "t-2", "deps": ["t-1"] }))["isError"], false);
    assert_eq!(reg.get_task(&co.group, "t-2").unwrap().deps, ["t-1"]);

    // list_tasks — readable by ANY role — carries the ids and the derived flag.
    let listed = dispatch(&reg, &cw, "tools/call", &json!({ "name": "list_tasks", "arguments": {} })).unwrap();
    let rows = text_of(&listed);
    assert!(rows.contains(r#""deps":["t-1"]"#), "compact rows carry link ids: {rows}");
    assert!(rows.contains(r#""ready":false"#), "a blocked queued task reads not-ready: {rows}");

    // Registry rejections surface as tool errors WITH the reason — a caller
    // must be able to tell a cycle from a permission problem.
    let cyc = call(&co, json!({ "id": "t-1", "deps": ["t-2"] }));
    assert_eq!(cyc["isError"], true);
    assert!(text_of(&cyc).contains("cycle"), "the cycle reason survives the shim: {}", text_of(&cyc));

    let blocked = call(&co, json!({ "id": "t-2", "assignee": "w-3", "claim": true }));
    assert_eq!(blocked["isError"], true);
    assert!(text_of(&blocked).contains("t-1"), "the unmet dep survives the shim: {}", text_of(&blocked));

    // Finish the blocker; the claim then succeeds and names its holder back.
    call(&co, json!({ "id": "t-1", "status": "done" }));
    let ok = call(&co, json!({ "id": "t-2", "assignee": "w-3", "claim": true }));
    assert_eq!(ok["isError"], false);
    assert!(text_of(&ok).contains("claimed by w-3"), "the result says who got it: {}", text_of(&ok));

    // Wrong-typed args are refused, never silently ignored: a caller that
    // passed a string where an array belongs must not be told it worked.
    assert_eq!(call(&co, json!({ "id": "t-2", "deps": "t-1" }))["isError"], true);
    assert_eq!(call(&co, json!({ "id": "t-2", "deps": [3] }))["isError"], true);
    assert_eq!(call(&co, json!({ "id": "t-2", "claim": "true" }))["isError"], true);
    assert_eq!(
        reg.get_task(&co.group, "t-2").unwrap().assignee.as_deref(),
        Some("w-3"),
        "a refused arg parse changes nothing"
    );

    // [] clears; and a worker still cannot write links at all.
    assert_eq!(call(&co, json!({ "id": "t-2", "deps": [] }))["isError"], false);
    assert!(reg.get_task(&co.group, "t-2").unwrap().deps.is_empty());
    assert_eq!(call(&cw, json!({ "id": "t-2", "deps": ["t-1"] }))["isError"], true, "the board stays orchestrator-write");
}

// ---------- #958 slice A: task hierarchy (parent + advisory kind) ----------

/// A `parent`-only patch — the shape a reparent takes.
pub(crate) fn parent_patch(parent: &str) -> TaskPatch {
    TaskPatch { parent: Some(parent.into()), ..Default::default() }
}

/// A `Task` literal WITH hierarchy, for the pure board projections — no
/// registry, no files (the `linked` helper's companion).
fn kinded(id: &str, status: &str, kind: &str, parent: Option<&str>) -> Task {
    Task {
        kind: Some(kind.into()),
        parent: parent.map(String::from),
        ..linked(id, status, &[], &[])
    }
}

/// The write-time contract for `parent` (#958), which is the #582 link
/// contract applied to containment: every rejection names what it refused and
/// leaves the board byte-for-byte as it was, because a container that names no
/// live task — or a chain that loops or runs past the cap — is a state no
/// reader should have to have an answer for.
#[test]
fn parent_writes_are_validated_against_the_whole_board() {
    let (reg, _d) = test_registry();
    let gid = board_with(&reg, &["a", "b", "c", "d", "e", "f"]);

    // An id naming no task on the board is refused, and the error says which.
    let err = reg.upsert_task(&gid, "orch", Some("t-2"), parent_patch("t-99")).unwrap_err();
    assert!(err.contains("t-99"), "the rejection names the unknown id: {err}");
    // A task cannot contain itself.
    let err = reg.upsert_task(&gid, "orch", Some("t-2"), parent_patch("t-2")).unwrap_err();
    assert!(err.contains("t-2"), "the self-parent rejection names the row: {err}");
    assert!(
        reg.get_task(&gid, "t-2").unwrap().parent.is_none(),
        "a refused hierarchy write must leave the board exactly as it was"
    );

    // A legal chain: t-1 → t-2 → t-3.
    reg.upsert_task(&gid, "orch", Some("t-2"), parent_patch("t-1")).unwrap();
    reg.upsert_task(&gid, "orch", Some("t-3"), parent_patch("t-2")).unwrap();

    // Reparenting a row under its OWN DESCENDANT is the cycle case, and the
    // refusal names the path rather than just saying no (`find_dep_cycle`'s
    // contract, on the container graph).
    let err = reg.upsert_task(&gid, "orch", Some("t-1"), parent_patch("t-3")).unwrap_err();
    assert!(err.contains("cycle"), "the refusal says what it is: {err}");
    assert!(err.contains("t-1 → t-3 → t-2 → t-1"), "the error names the whole chain, in order: {err}");
    assert!(reg.get_task(&gid, "t-1").unwrap().parent.is_none(), "the rejected edge is not written");

    // The depth cap. t-4 under t-3 is the fourth and last legal level; a fifth
    // is refused.
    reg.upsert_task(&gid, "orch", Some("t-4"), parent_patch("t-3")).unwrap();
    let err = reg.upsert_task(&gid, "orch", Some("t-5"), parent_patch("t-4")).unwrap_err();
    assert!(err.contains("depth"), "the refusal says what the limit is: {err}");
    assert!(reg.get_task(&gid, "t-5").unwrap().parent.is_none(), "nothing written");

    // The cap counts the moving row's OWN SUBTREE too, not just its new chain:
    // t-5 holds t-6, so t-5 is two levels tall and cannot fit under t-3 (which
    // is already three deep) even though t-5 ALONE would have.
    reg.upsert_task(&gid, "orch", Some("t-6"), parent_patch("t-5")).unwrap();
    let err = reg.upsert_task(&gid, "orch", Some("t-5"), parent_patch("t-3")).unwrap_err();
    assert!(
        err.contains("depth"),
        "a reparent must not smuggle an over-deep chain in from BELOW the moving row: {err}"
    );
    assert_eq!(
        reg.get_task(&gid, "t-6").unwrap().parent.as_deref(),
        Some("t-5"),
        "the subtree is untouched by the refusal"
    );

    // ...and the same move one level higher, where the subtree DOES fit, is
    // allowed — the cap is on the deepest resulting row, not on the mover.
    reg.upsert_task(&gid, "orch", Some("t-5"), parent_patch("t-2")).unwrap();

    // A link to your own container is a mistake in either direction — writing
    // the link under an existing container, or moving the container under an
    // existing link.
    let err = reg.upsert_task(&gid, "orch", Some("t-4"), deps_patch(&["t-3"])).unwrap_err();
    assert!(err.contains("deps") && err.contains("t-3"), "a dep on your own container is refused: {err}");
    let rel = TaskPatch { related: Some(vec!["t-3".into()]), ..Default::default() };
    let err = reg.upsert_task(&gid, "orch", Some("t-4"), rel).unwrap_err();
    assert!(err.contains("related"), "...and so is a see-also on it: {err}");
    assert!(reg.get_task(&gid, "t-4").unwrap().deps.is_empty(), "nothing written");

    // The parent-write direction: t-6 already deps on t-1, so moving t-6 INTO
    // t-1 is the same mistake arriving from the other side.
    reg.upsert_task(&gid, "orch", Some("t-6"), deps_patch(&["t-1"])).unwrap();
    let err = reg.upsert_task(&gid, "orch", Some("t-6"), parent_patch("t-1")).unwrap_err();
    assert!(
        err.contains("deps") && err.contains("t-1"),
        "reparenting under a task you already depend on is refused too: {err}"
    );
    assert_eq!(reg.get_task(&gid, "t-6").unwrap().parent.as_deref(), Some("t-5"), "nothing written");

    // The kind is validated like the status, and the error names the vocabulary.
    let bad = TaskPatch { kind: Some("saga".into()), ..Default::default() };
    let err = reg.upsert_task(&gid, "orch", Some("t-2"), bad).unwrap_err();
    assert!(err.contains("saga") && err.contains("epic"), "an unknown kind is refused by name: {err}");
    assert!(reg.get_task(&gid, "t-2").unwrap().kind.is_none(), "nothing written");
}

/// Levels are ENFORCED (#1156, overturning #958's advisory position at the
/// human's direction), and a container is still an ordinary work item that can
/// be claimed. The second half is unchanged and still worth pinning: enforcing
/// the ladder must not turn a container into a special non-work row.
#[test]
fn levels_are_enforced_and_a_container_is_still_ordinary_work() {
    let (reg, _d) = test_registry();
    let gid = board_with(&reg, &["the epic", "a slice"]);
    let epic = TaskPatch { kind: Some("epic".into()), ..Default::default() };
    reg.upsert_task(&gid, "orch", Some("t-1"), epic).unwrap();
    assert_eq!(reg.get_task(&gid, "t-1").unwrap().kind.as_deref(), Some("epic"));

    // A story straight under an epic, with no feature between them: REFUSED,
    // where #958 accepted and stored it. The refusal names the level that is
    // missing rather than only saying no.
    let mut p = parent_patch("t-1");
    p.kind = Some("story".into());
    let err = reg.upsert_task(&gid, "orch", Some("t-2"), p).unwrap_err();
    assert!(
        err.contains("story") && err.contains("feature"),
        "skipping a level is refused, and the error names the level that belongs between them: {err}"
    );
    let t2 = reg.get_task(&gid, "t-2").unwrap();
    assert_eq!((t2.parent, t2.kind), (None, None), "a refused ladder write lands NEITHER field");

    // The legal shape, one rung at a time.
    let mut p = parent_patch("t-1");
    p.kind = Some("feature".into());
    let feature = reg.upsert_task(&gid, "orch", Some("t-2"), p).unwrap();
    assert_eq!((feature.parent.as_deref(), feature.kind.as_deref()), (Some("t-1"), Some("feature")));

    // And the epic itself is claimable — hierarchy is not a work/container
    // split, so nothing about having children changes the claim guards.
    let t = reg.upsert_task(&gid, "orch", Some("t-1"), claim_patch("w-3")).unwrap();
    assert_eq!(t.status, "in-progress", "a container with children is still claimable work");
    assert_eq!(t.assignee.as_deref(), Some("w-3"));
}

/// `parent`/`kind` follow the `pr` rule (#958): omitted leaves them alone, an
/// empty string clears. Without the clear, "this is no longer inside anything"
/// would need a hand-edited `tasks.json`.
#[test]
fn parent_and_kind_clear_on_empty_and_stay_untouched_when_omitted() {
    let (reg, _d) = test_registry();
    let gid = board_with(&reg, &["the epic", "a slice"]);

    let epic = TaskPatch { kind: Some("epic".into()), ..Default::default() };
    reg.upsert_task(&gid, "orch", Some("t-1"), epic).unwrap();
    let mut p = parent_patch("t-1");
    p.kind = Some("feature".into());
    let t = reg.upsert_task(&gid, "orch", Some("t-2"), p).unwrap();
    assert_eq!(t.parent.as_deref(), Some("t-1"));
    assert_eq!(t.kind.as_deref(), Some("feature"));
    // Durable, not just in the returned snapshot.
    assert_eq!(reg.tasks(&gid)[1].parent.as_deref(), Some("t-1"));

    // A create can name its container AND its level in the SAME call — the
    // orchestrator's actual pattern is "make the epic, then hang each feature
    // under it". The ladder is judged on that one write, not on a half-built
    // row: creating it at the wrong level is refused the same way.
    let mut create = patch(Some("nested at birth"), None, None);
    create.parent = Some("t-1".into());
    create.kind = Some("feature".into());
    let born = reg.upsert_task(&gid, "orch", None, create).unwrap();
    assert_eq!(born.parent.as_deref(), Some("t-1"), "a new row can be created already inside its container");

    // Omitted = untouched: an ordinary status/note edit must never orphan a row.
    let t = reg.upsert_task(&gid, "orch", Some("t-2"), patch(None, None, Some("unrelated edit"))).unwrap();
    assert_eq!(t.parent.as_deref(), Some("t-1"), "a patch that omits parent must not clear it");
    assert_eq!(t.kind.as_deref(), Some("feature"));

    // Empty (or blank) clears rather than storing whitespace. Both fields in
    // ONE write, which is also the only way out for a levelled row: clearing
    // just the container would leave a top-level feature, which the ladder
    // refuses (#1156) — the escape is to stop claiming the level too.
    let clear = TaskPatch { parent: Some("  ".into()), kind: Some("".into()), ..Default::default() };
    let t = reg.upsert_task(&gid, "orch", Some("t-2"), clear).unwrap();
    assert_eq!(t.parent, None, "a blank parent promotes the row to top level");
    assert_eq!(t.kind, None, "...and a blank kind clears the label, it is not an invalid kind");
}

// ---------- #1156: the strict Agile ladder + kind-prefixed ids ----------

/// A create that names its level and its container in ONE call — the shape the
/// tool description teaches, and the only way to build a legal ladder.
fn create_at(title: &str, kind: &str, parent: Option<&str>) -> TaskPatch {
    TaskPatch {
        title: Some(title.into()),
        kind: Some(kind.into()),
        parent: parent.map(String::from),
        ..Default::default()
    }
}

/// A level-only patch — the shape a re-level takes. `""` is the clear.
fn kind_patch(kind: &str) -> TaskPatch {
    TaskPatch { kind: Some(kind.into()), ..Default::default() }
}

/// The whole ladder as a table, on this side.
///
/// This test does NOT pin the board's copy — it asserts the Rust table against
/// Rust literals, so what it catches is a rule edited without its own test.
/// The CROSS-LANGUAGE pin is `the board's ladder table is the backend's, read
/// out of the Rust source` (`test/taskboard.test.ts`), which reads the arms of
/// `ladder_rule` out of this file's source and compares them to the board's
/// table; that is the one that reddens when the two ladders part. Naming the
/// wrong one here was a review finding — a pair of same-shaped tests looks like
/// an equivalence and is not one.
#[test]
fn the_ladder_table_is_pinned_on_the_rust_side() {
    assert_eq!(ladder_rule(Some("epic")), LadderRule::TopLevelOnly);
    assert_eq!(ladder_rule(Some("feature")), LadderRule::Inside("epic"));
    assert_eq!(ladder_rule(Some("story")), LadderRule::Inside("feature"));
    assert_eq!(ladder_rule(Some("task")), LadderRule::Inside("story"));
    // The exemption, at the level of the rule itself.
    assert_eq!(ladder_rule(None), LadderRule::Exempt);
    assert_eq!(ladder_rule(Some("sprint")), LadderRule::Exempt);
    // Every level in the VOCABULARY has a place on the ladder, so adding a
    // fifth kind to `TASK_KINDS` without placing it reddens here rather than
    // shipping a level that is silently exempt from the rule it looks like it
    // should obey.
    for k in TASK_KINDS {
        assert_ne!(ladder_rule(Some(k)), LadderRule::Exempt, "{k} must have a place on the ladder");
    }
}

/// AC1: every rung, refused from every wrong place, with the fix named.
///
/// The refusal has to name the fix for the same reason the cycle refusal names
/// the path: the caller has exactly two moves out of a ladder violation — nest
/// the row where its level belongs, or stop claiming that level — and an error
/// that only says no leaves them guessing between them.
#[test]
fn the_ladder_refuses_every_wrong_container_and_names_the_fix() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let gid = g.id;
    let epic = reg.upsert_task(&gid, "orch", None, create_at("v1.2.0", "epic", None)).unwrap();
    let feat = reg.upsert_task(&gid, "orch", None, create_at("board sort", "feature", Some(&epic.id))).unwrap();
    let story = reg.upsert_task(&gid, "orch", None, create_at("sort by status", "story", Some(&feat.id))).unwrap();
    // The row every refusal below is attempted ON, so each one can be checked
    // for having written nothing.
    let spare = reg.upsert_task(&gid, "orch", None, patch(Some("spare"), None, None)).unwrap();

    // (level to claim, container to claim it in, the word the error must name)
    let cases: [(&str, Option<&str>, &str); 6] = [
        // A feature with no epic above it: the level claims to break an epic
        // down, and there is no epic.
        ("feature", None, "epic"),
        // A story straight under the epic — the shape #958 explicitly allowed.
        ("story", Some(epic.id.as_str()), "feature"),
        // A task under the feature, skipping the story.
        ("task", Some(feat.id.as_str()), "story"),
        // Down the ladder instead of up.
        ("feature", Some(story.id.as_str()), "epic"),
        // An epic is top-level only, wherever you try to put it.
        ("epic", Some(epic.id.as_str()), "top-level only"),
        ("epic", Some(story.id.as_str()), "top-level only"),
    ];
    for (kind, parent, must_name) in cases {
        let p = TaskPatch {
            kind: Some(kind.into()),
            parent: Some(parent.unwrap_or("").to_string()),
            ..Default::default()
        };
        let err = reg.upsert_task(&gid, "orch", Some(&spare.id), p).unwrap_err();
        assert!(
            err.contains(must_name),
            "a {kind} in {parent:?} must be refused naming {must_name}: {err}"
        );
        assert!(
            err.contains("clear its level"),
            "...and every refusal names the other way out: {err}"
        );
        let after = reg.get_task(&gid, &spare.id).unwrap();
        assert_eq!(
            (after.kind, after.parent),
            (None, None),
            "a refused ladder write lands NEITHER field ({kind} in {parent:?})"
        );
    }

    // The negative control: the same rows, placed right, all land. Without this
    // "refuse everything" would pass every assertion above.
    assert_eq!(story.parent.as_deref(), Some(feat.id.as_str()));
    let t = reg.upsert_task(&gid, "orch", None, create_at("a subtask", "task", Some(&story.id))).unwrap();
    assert_eq!((t.kind.as_deref(), t.parent.as_deref()), (Some("task"), Some(story.id.as_str())));
}

/// AC2, the direction a row's own container pointer cannot see: re-levelling a
/// row is judged against the rows INSIDE it too.
///
/// The witness has to be a board the ladder would not have built, because on a
/// ladder-legal board a row's own link already pins its level exactly — the
/// only level a row inside an epic may carry is `feature`, so every re-level of
/// it is refused by its own link before any child is consulted. The shape where
/// the child check is the ONLY thing standing between the caller and a stranded
/// row is the legacy one: a top-level `feature` holding a `story`, which #958
/// §2 called the dominant real shape. Promoting that feature to the epic the
/// ladder now wants is legal for its own link and wrong for the row inside it.
#[test]
fn a_re_level_is_judged_against_the_rows_inside_it_too() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let path = reg.state_root().join(g.id.as_str()).join("tasks.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        r##"[
  {"id":"t-1","title":"The board","status":"queued","issue":null,"pr":null,"assignee":null,"session":null,"notes":[],"kind":"feature","updated_ms":11},
  {"id":"t-2","title":"Sorting","status":"queued","issue":null,"pr":null,"assignee":null,"session":null,"notes":[],"parent":"t-1","kind":"story","updated_ms":12}
]"##,
    )
    .unwrap();
    let gid = g.id;

    // An epic belongs at top level, so t-1's OWN link is happy — and the write
    // is still refused, naming the row inside that it would strand.
    let err = reg.upsert_task(&gid, "orch", Some("t-1"), kind_patch("epic")).unwrap_err();
    assert!(err.contains("t-2"), "the refusal names the row inside that would be stranded: {err}");
    assert!(err.contains("t-1 cannot be an epic"), "...and what it refused to do: {err}");
    // Clearing t-1's level strands the same child — an unlevelled container
    // holds only unlevelled rows — so that is refused too.
    let err = reg.upsert_task(&gid, "orch", Some("t-1"), kind_patch("")).unwrap_err();
    assert!(err.contains("cleared"), "clearing a container's level is refused too: {err}");
    assert_eq!(
        reg.get_task(&gid, "t-1").unwrap().kind.as_deref(),
        Some("feature"),
        "neither refusal wrote anything"
    );

    // The CHILD's level is exactly what makes both refusals: deal with it first
    // and the same two writes land. This is the negative control — a check that
    // refused every re-level would pass both assertions above.
    reg.upsert_task(&gid, "orch", Some("t-2"), kind_patch("")).unwrap();
    reg.upsert_task(&gid, "orch", Some("t-1"), kind_patch("epic")).unwrap();
    assert_eq!(reg.get_task(&gid, "t-1").unwrap().kind.as_deref(), Some("epic"));
    // ...and the child can now be re-levelled onto the rung that actually fits
    // under an epic, which is the whole migration path in three writes.
    reg.upsert_task(&gid, "orch", Some("t-2"), kind_patch("feature")).unwrap();

    // A pure REPARENT of a container is NOT a re-level, so it does not re-judge
    // the rows inside it: a child's rule reads its container's LEVEL, never
    // where that container itself sits. t-1 keeps its story-turned-feature
    // child while moving under another epic.
    let epic2 = reg.upsert_task(&gid, "orch", None, create_at("v1.3.0", "epic", None)).unwrap();
    let err = reg.upsert_task(&gid, "orch", Some("t-1"), parent_patch(&epic2.id)).unwrap_err();
    assert!(
        err.contains("top-level only"),
        "an epic still cannot be nested, and that is its OWN link talking, not its children's: {err}"
    );
    reg.upsert_task(&gid, "orch", Some("t-1"), kind_patch("")).unwrap_err();
    let plain = reg.upsert_task(&gid, "orch", None, patch(Some("a plain container"), None, None)).unwrap();
    let kid = reg.upsert_task(&gid, "orch", None, patch(Some("a plain child"), None, None)).unwrap();
    reg.upsert_task(&gid, "orch", Some(&kid.id), parent_patch(&plain.id)).unwrap();
    reg.upsert_task(&gid, "orch", Some(&plain.id), parent_patch(&epic2.id)).unwrap();
    assert_eq!(
        reg.get_task(&gid, &plain.id).unwrap().parent.as_deref(),
        Some(epic2.id.as_str()),
        "a reparent carrying children is judged on the mover's own link alone"
    );
}

/// The exemption, as a first-class guarantee rather than a migration
/// allowance: a row with NO level may sit anywhere, forever.
///
/// This is the shape of every pre-#1156 board, and it is also the shape a group
/// that runs no Agile at all wants — loomux is a generic agentic-dev tool and
/// must not require a methodology (CLAUDE.md constraint 8). Ladder enforcement
/// that reached level-less rows would make the board unusable for both.
#[test]
fn a_level_less_row_is_exempt_from_the_ladder() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let gid = g.id;
    let epic = reg.upsert_task(&gid, "orch", None, create_at("v1.2.0", "epic", None)).unwrap();
    let feat = reg.upsert_task(&gid, "orch", None, create_at("board sort", "feature", Some(&epic.id))).unwrap();

    // A plain row goes anywhere on a levelled board — including places no
    // LEVELLED row could go (straight inside an epic, beside a feature).
    let plain = reg.upsert_task(&gid, "orch", None, patch(Some("a plain row"), None, None)).unwrap();
    for container in [epic.id.as_str(), feat.id.as_str()] {
        reg.upsert_task(&gid, "orch", Some(&plain.id), parent_patch(container)).unwrap();
        assert_eq!(reg.get_task(&gid, &plain.id).unwrap().parent.as_deref(), Some(container));
    }
    // ...and back to the top level, which a feature could not do.
    reg.upsert_task(&gid, "orch", Some(&plain.id), parent_patch("")).unwrap();
    assert_eq!(reg.get_task(&gid, &plain.id).unwrap().parent, None);

    // A plain row also CONTAINS plain rows — the whole flat board, on which the
    // ladder is invisible.
    let child = reg.upsert_task(&gid, "orch", None, patch(Some("a plain child"), None, None)).unwrap();
    reg.upsert_task(&gid, "orch", Some(&child.id), parent_patch(&plain.id)).unwrap();
    assert_eq!(reg.get_task(&gid, &child.id).unwrap().parent.as_deref(), Some(plain.id.as_str()));

    // The one thing exemption does NOT buy: a LEVELLED row inside an
    // unlevelled one. "Inside a feature" is a claim about the container, and a
    // row carrying no level does not make it.
    let err = reg.upsert_task(&gid, "orch", Some(&child.id), kind_patch("story")).unwrap_err();
    assert!(err.contains("carries no level"), "the refusal says which side is missing a level: {err}");
}

/// The migration guarantee, on a board written before any of this existed: an
/// existing row must not become UNEDITABLE because its shape predates the rule.
///
/// This is the whole reason the ladder is triggered by the write that asserts
/// the shape rather than by the row's existence. Judging every write would have
/// frozen the status, notes, assignee and deps of every row on every board that
/// used #958's advisory levels — which is the DOMINANT shape #958 §2 describes,
/// a top-level `feature` with its slices under it.
#[test]
fn a_pre_1156_board_stays_editable_everywhere_except_its_shape() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let path = reg.state_root().join(g.id.as_str()).join("tasks.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    // A board loomux itself wrote under #958: t- ids throughout, a TOP-LEVEL
    // `feature` (the illegal row — a feature owes an epic), a story inside it
    // (that rung is legal, then and now), and a level-less row.
    fs::write(
        &path,
        r##"[
  {"id":"t-1","title":"The board","status":"queued","issue":null,"pr":null,"assignee":null,"session":null,"notes":[],"kind":"feature","updated_ms":11},
  {"id":"t-2","title":"Sorting","status":"queued","issue":null,"pr":null,"assignee":null,"session":null,"notes":[],"parent":"t-1","kind":"story","updated_ms":12},
  {"id":"t-3","title":"Plain old row","status":"queued","issue":null,"pr":null,"assignee":null,"session":null,"notes":[],"updated_ms":13}
]"##,
    )
    .unwrap();
    let gid = g.id;
    assert_eq!(reg.tasks(&gid).len(), 3, "a pre-#1156 board still loads");
    // The premise, asserted rather than assumed: t-1 is a row the ladder
    // refuses, so every edit below is landing on one.
    assert!(
        reg.upsert_task(&gid, "orch", Some("t-1"), kind_patch("feature")).is_err(),
        "the fixture's t-1 must really be an illegal shape, or this test witnesses nothing"
    );

    // EVERY field but the two that assert the shape is still writable on that
    // illegal row — this is the guarantee, and it is the one worth the most.
    // The claim goes first: it guards on the row's own deps being `done`, so
    // adding an unmet one below would refuse it for a reason that has nothing
    // to do with the ladder.
    reg.upsert_task(&gid, "orch", Some("t-1"), claim_patch("w-1")).unwrap();
    reg.upsert_task(&gid, "orch", Some("t-1"), deps_patch(&["t-3"])).unwrap();
    reg.upsert_task(&gid, "orch", Some("t-1"), patch(None, Some("review"), Some("a note"))).unwrap();
    let t1 = reg.get_task(&gid, "t-1").unwrap();
    assert_eq!(t1.deps, vec!["t-3".to_string()], "deps still land on a row the ladder refuses");
    assert_eq!(t1.assignee.as_deref(), Some("w-1"), "...and so does a claim");
    assert_eq!(t1.status, "review", "...and a status");
    assert_eq!(t1.notes.len(), 1, "...and a note");
    assert_eq!(
        (t1.kind.as_deref(), t1.parent.as_deref()),
        (Some("feature"), None),
        "and none of those edits quietly repaired — or destroyed — the legacy shape"
    );

    // Only a write that RE-ASSERTS the shape is judged. Re-writing the level it
    // already carries is such a write, deliberately: it is a fresh claim about
    // where this row sits, and the board answers it honestly. (That is the
    // assertion the premise check above already made; this one pins the TEXT.)
    let err = reg.upsert_task(&gid, "orch", Some("t-1"), kind_patch("feature")).unwrap_err();
    assert!(err.contains("must sit inside"), "re-asserting an illegal shape is refused: {err}");
    assert!(err.contains("clear its level"), "...naming the fix, as every ladder refusal does: {err}");

    // The way out, three writes and no id rewritten. t-1 cannot become the epic
    // the ladder wants while a `story` sits inside it, so the child goes first —
    // the both-directions rule doing exactly what it is for.
    let blocked = reg.upsert_task(&gid, "orch", Some("t-1"), kind_patch("epic")).unwrap_err();
    assert!(blocked.contains("t-2"), "the child is what blocks the promotion, and is named: {blocked}");
    reg.upsert_task(&gid, "orch", Some("t-2"), kind_patch("")).unwrap();
    reg.upsert_task(&gid, "orch", Some("t-1"), kind_patch("epic")).unwrap();
    reg.upsert_task(&gid, "orch", Some("t-2"), kind_patch("feature")).unwrap();
    let ids: Vec<(String, Option<String>)> =
        reg.tasks(&gid).iter().map(|t| (t.id.clone(), t.kind.clone())).collect();
    assert_eq!(
        ids,
        vec![
            ("t-1".to_string(), Some("epic".to_string())),
            ("t-2".to_string(), Some("feature".to_string())),
            ("t-3".to_string(), None),
        ],
        "the board is legal now, and NO id was rewritten getting there"
    );
    assert_eq!(reg.get_task(&gid, "t-2").unwrap().parent.as_deref(), Some("t-1"));
}

/// Promote-on-delete is the one path that can land a row where no write could
/// have put it, and it stays that way on purpose (#958 §4/§5).
///
/// The alternatives are all worse: refusing the human's delete, cascading it
/// into the work items inside, or silently stripping the survivor's level —
/// destroying data to preserve an invariant about a label.
#[test]
fn promote_on_delete_can_leave_a_shape_no_write_could_have_asked_for() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let gid = g.id;
    let epic = reg.upsert_task(&gid, "orch", None, create_at("v1.2.0", "epic", None)).unwrap();
    let feat = reg.upsert_task(&gid, "orch", None, create_at("board sort", "feature", Some(&epic.id))).unwrap();
    reg.upsert_task(&gid, "orch", Some(&feat.id), patch(None, None, Some("real work here"))).unwrap();

    reg.delete_task(&gid, "human", &epic.id).unwrap();
    let orphan = reg.get_task(&gid, &feat.id).unwrap();
    assert_eq!(orphan.parent, None, "the delete promoted it rather than cascading into it");
    assert_eq!(orphan.kind.as_deref(), Some("feature"), "...and kept its level, notes and all");
    assert_eq!(orphan.notes.len(), 1);

    // The row is perfectly readable and perfectly editable — it is only the
    // next write that re-asserts its shape that has to resolve it, and that
    // error names both ways out.
    reg.upsert_task(&gid, "orch", Some(&feat.id), patch(None, Some("in-progress"), None)).unwrap();
    let err = reg.upsert_task(&gid, "orch", Some(&feat.id), kind_patch("feature")).unwrap_err();
    assert!(err.contains("nest it under") && err.contains("clear its level"), "both ways out: {err}");
    reg.upsert_task(&gid, "orch", Some(&feat.id), kind_patch("")).unwrap();
    assert_eq!(reg.get_task(&gid, &feat.id).unwrap().kind, None);
}

/// AC3: a new row's id carries the level it was created at, off ONE shared
/// counter.
///
/// Shared rather than per-prefix so that every number on a board is used once:
/// with a counter per prefix a board holds `e-1`, `f-1`, `us-1` and `t-1` at
/// the same time, and a half-remembered "1" with the wrong prefix names a real
/// but WRONG row — a silent mis-link in `deps`/`parent`, which is the exact
/// class of confusion this feature exists to remove.
#[test]
fn new_ids_carry_their_level_off_one_shared_counter() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let gid = g.id;
    let epic = reg.upsert_task(&gid, "orch", None, create_at("v1.2.0", "epic", None)).unwrap();
    let feat = reg.upsert_task(&gid, "orch", None, create_at("board sort", "feature", Some(&epic.id))).unwrap();
    let story = reg.upsert_task(&gid, "orch", None, create_at("sort by status", "story", Some(&feat.id))).unwrap();
    let task = reg.upsert_task(&gid, "orch", None, create_at("the sort fn", "task", Some(&story.id))).unwrap();
    let plain = reg.upsert_task(&gid, "orch", None, patch(Some("a plain row"), None, None)).unwrap();
    assert_eq!(
        [epic.id.as_str(), feat.id.as_str(), story.id.as_str(), task.id.as_str(), plain.id.as_str()],
        ["e-1", "f-2", "us-3", "t-4", "t-5"],
        "each level mints its own prefix, and the NUMBER is unique across all of them"
    );

    // The high-water mark is read off the LIVE board, so deleting the newest
    // row does hand its number back out. That is pre-#1156 behaviour, carried
    // over deliberately rather than fixed here: the board is the only state
    // there is, so "never reissue" would need a persisted counter — a new
    // durable field, on every group, for a hazard that predates this change.
    // Pinned so a future attempt at it is a deliberate change to a stated
    // property rather than a surprise.
    reg.delete_task(&gid, "human", &plain.id).unwrap();
    let next = reg.upsert_task(&gid, "orch", None, patch(Some("after the delete"), None, None)).unwrap();
    assert_eq!(next.id, "t-5", "the mark is over the live board, so a deleted number IS reissued");

    // What the shared counter does guarantee is the property #1156 needs: two
    // LIVE rows never share a number, whatever their prefixes.
    let ids: Vec<String> = reg.tasks(&gid).iter().map(|t| t.id.clone()).collect();
    let numbers: Vec<&str> = ids.iter().map(|i| i.split_once('-').unwrap().1).collect();
    let unique: HashSet<&&str> = numbers.iter().collect();
    assert_eq!(unique.len(), numbers.len(), "no two live rows share a number: {ids:?}");
}

/// The other half of AC4, and the rule with the most references riding on it:
/// levelling an EXISTING row never rewrites its id.
///
/// An id is quoted by `deps`, `related` and `parent` on other rows, by every
/// audit line, by agents' stored session state, and by a human's memory.
/// Rewriting one would have to rewrite all of those atomically and could not
/// touch the last two at all, so the id a row is minted with is the id it keeps
/// — the `kind` field, not the prefix, is what says what a row IS.
#[test]
fn levelling_an_existing_row_never_rewrites_its_id() {
    let (reg, _d) = test_registry();
    let gid = board_with(&reg, &["the container", "a slice"]);
    // Both minted before anyone thought about levels — the legacy id space.
    assert_eq!(reg.tasks(&gid).iter().map(|t| t.id.clone()).collect::<Vec<_>>(), ["t-1", "t-2"]);
    reg.upsert_task(&gid, "orch", Some("t-1"), kind_patch("epic")).unwrap();
    let mut p = parent_patch("t-1");
    p.kind = Some("feature".into());
    reg.upsert_task(&gid, "orch", Some("t-2"), p).unwrap();

    let board = reg.tasks(&gid);
    assert_eq!(
        board.iter().map(|t| (t.id.as_str(), t.kind.as_deref())).collect::<Vec<_>>(),
        [("t-1", Some("epic")), ("t-2", Some("feature"))],
        "a t- id that became an epic KEEPS its id — the prefix is where it started, kind is what it is"
    );
    // ...and the reference that was already pointing at it still resolves,
    // which is the whole point of not rewriting.
    assert_eq!(board[1].parent.as_deref(), Some("t-1"));

    // A NEW row on that same board is minted at its level, above the legacy
    // high-water mark — so the two id shapes coexist without collision.
    let fresh = reg.upsert_task(&gid, "orch", None, create_at("v1.3.0", "epic", None)).unwrap();
    assert_eq!(fresh.id, "e-3");
}

/// Promote-on-delete (#958), the `strip_deleted_links` reasoning applied to
/// containment: refusing the delete would fight the human's authority over
/// their own board, and cascading would silently destroy work items along with
/// their PR/session refs — the worst failure direction. So the children are
/// promoted, in the SAME locked write, on all three delete paths.
#[test]
fn deleting_a_container_promotes_its_children_to_the_nearest_surviving_ancestor() {
    let (reg, _d) = test_registry();

    // --- single delete: the child lands on the deleted row's own container.
    let gid = board_with(&reg, &["epic", "feature", "story", "bystander"]);
    reg.upsert_task(&gid, "orch", Some("t-2"), parent_patch("t-1")).unwrap();
    reg.upsert_task(&gid, "orch", Some("t-3"), parent_patch("t-2")).unwrap();
    reg.delete_task(&gid, "human", "t-2").unwrap();
    assert!(reg.get_task(&gid, "t-3").is_some(), "the child SURVIVES — a delete never cascades");
    assert_eq!(
        reg.get_task(&gid, "t-3").unwrap().parent.as_deref(),
        Some("t-1"),
        "promoted one level, not left pointing at a row nobody can resolve"
    );
    let del = reg.audit_log(&gid).into_iter().find(|e| e.action == "task-delete").unwrap();
    assert_eq!(del.detail["reparented"], json!(["t-3"]), "the audit names whose container moved: {}", del.detail);

    // --- batch delete taking a parent AND its grandparent in ONE write: the
    // survivor must land on the NEAREST SURVIVING ancestor. This is the whole
    // reason the promotion walks the removed chain instead of reading one
    // pointer — reading one would leave t-4 pointing at the already-deleted t-2.
    //
    // A FRESH registry per scenario, not just a fresh `board_with`: the helper
    // creates its group from the same repo path every time, so a second call on
    // one registry lands on the same board and mints ids from t-5 up.
    let (reg, _d2) = test_registry();
    let gid = board_with(&reg, &["epic", "feature", "story", "task"]);
    reg.upsert_task(&gid, "orch", Some("t-2"), parent_patch("t-1")).unwrap();
    reg.upsert_task(&gid, "orch", Some("t-3"), parent_patch("t-2")).unwrap();
    reg.upsert_task(&gid, "orch", Some("t-4"), parent_patch("t-3")).unwrap();
    reg.delete_tasks(&gid, "human", &["t-2".to_string(), "t-3".to_string()]).unwrap();
    assert_eq!(
        reg.get_task(&gid, "t-4").unwrap().parent.as_deref(),
        Some("t-1"),
        "two levels went in one write — the child lands on the nearest SURVIVOR, not at top level"
    );
    let sel = reg.audit_log(&gid).into_iter().find(|e| e.action == "task-delete-selected").unwrap();
    assert_eq!(sel.detail["reparented"], json!(["t-4"]));

    // When the WHOLE chain goes, the survivor lands at top level rather than
    // keeping a dangling pointer.
    reg.delete_tasks(&gid, "human", &["t-1".to_string()]).unwrap();
    assert_eq!(reg.get_task(&gid, "t-4").unwrap().parent, None, "no surviving ancestor = top level");

    // --- delete-all-done, the likeliest way a container disappears.
    let (reg, _d3) = test_registry();
    let gid = board_with(&reg, &["epic", "slice"]);
    reg.upsert_task(&gid, "orch", Some("t-2"), parent_patch("t-1")).unwrap();
    reg.upsert_task(&gid, "orch", Some("t-1"), patch(None, Some("done"), None)).unwrap();
    reg.delete_done_tasks(&gid, "human").unwrap();
    assert!(reg.get_task(&gid, "t-2").is_some(), "a done container's unfinished child is not swept with it");
    assert_eq!(reg.get_task(&gid, "t-2").unwrap().parent, None, "delete-all-done promotes too");
    let done = reg.audit_log(&gid).into_iter().find(|e| e.action == "task-delete-done").unwrap();
    assert_eq!(done.detail["reparented"], json!(["t-2"]));
}

/// rev-611 NB2: promotion can hand a row a container it already links to, and
/// that state must not wedge the row's other fields — nor be "fixed" by
/// silently dropping the link.
///
/// Depending on your GRANDparent is not a mistake, so it is accepted; deleting
/// the row in between is what turns that grandparent into a parent. The overlap
/// therefore arrives from the system, not from a hand-edit, which is the one
/// case the read-tolerance argument has to cover rather than assume away.
#[test]
fn promotion_may_hand_a_row_a_container_it_already_deps_on_without_wedging_it() {
    let (reg, _d) = test_registry();
    let gid = board_with(&reg, &["epic", "feature", "slice"]);
    reg.upsert_task(&gid, "orch", Some("t-2"), parent_patch("t-1")).unwrap();
    // Legal at write time: t-3's container is t-2, and t-1 is merely its
    // grandparent — "finish the epic's other half before starting this" is an
    // ordinary thing to record.
    let mut nested = parent_patch("t-2");
    nested.deps = Some(vec!["t-1".into()]);
    reg.upsert_task(&gid, "orch", Some("t-3"), nested).unwrap();

    // Deleting the middle row promotes t-3 onto t-1 — which it also deps on.
    reg.delete_task(&gid, "human", "t-2").unwrap();
    let t3 = reg.get_task(&gid, "t-3").unwrap();
    assert_eq!(t3.parent.as_deref(), Some("t-1"), "promoted to the nearest survivor as usual");
    assert_eq!(
        t3.deps,
        ["t-1"],
        "the dep SURVIVES: t-1 is still live and unfinished, and dropping it here would unblock \
         work whose blocker never went away — the one thing a delete of an unrelated row must not do"
    );
    assert!(
        !reg.task_summaries(&gid).iter().find(|r| r.id == "t-3").unwrap().ready,
        "so readiness is unchanged by the promotion — still blocked by t-1"
    );

    // And the row is not wedged: a write that does not re-assert the overlap
    // goes through, instead of being refused over a field it never touched.
    reg.upsert_task(&gid, "orch", Some("t-3"), patch(None, Some("in-progress"), None)).unwrap();
    let rel = TaskPatch { related: Some(vec!["t-1".into()]), ..Default::default() };
    let err = reg.upsert_task(&gid, "orch", Some("t-3"), rel).unwrap_err();
    assert!(err.contains("related"), "a `related` write IS judged on `related`: {err}");
    let other = TaskPatch { related: Some(vec![]), ..Default::default() };
    reg.upsert_task(&gid, "orch", Some("t-3"), other)
        .expect("a related-only write must not be refused for a deps overlap it never touched");

    // Re-asserting the overlap on the field being written is still refused —
    // the rule is intact, it is just scoped to the write that makes it.
    let err = reg.upsert_task(&gid, "orch", Some("t-3"), deps_patch(&["t-1"])).unwrap_err();
    assert!(err.contains("deps"), "writing the container back into deps is still a mistake: {err}");
}

/// The compat guarantee (#958), the same one #582 and #581 shipped: a board
/// written before hierarchy existed loads unchanged, and a board that uses no
/// hierarchy never grows the keys on rewrite. `tasks()` reports a parse failure
/// as an EMPTY board, so a non-defaulted field would silently erase every live
/// board on upgrade rather than error.
#[test]
fn pre_958_boards_load_and_a_flat_board_never_gains_the_hierarchy_keys() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let path = reg.state_root().join(g.id.as_str()).join("tasks.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    // Exactly what loomux wrote before #958 — no parent key, no kind key.
    fs::write(
        &path,
        r##"[
  {"id":"t-1","title":"Ship the parser","status":"queued","issue":"#7","pr":null,"assignee":null,"session":null,"notes":[],"updated_ms":11}
]"##,
    )
    .unwrap();

    let tasks = reg.tasks(&g.id);
    assert_eq!(tasks.len(), 1, "a pre-#958 board must still load — a parse failure reads as an EMPTY board");
    assert_eq!(tasks[0].parent, None, "an absent parent deserializes to None, never an error");
    assert_eq!(tasks[0].kind, None);

    // A rewrite must not GAIN the keys: that is what keeps an older loomux (and
    // a human reading the file) seeing exactly what it saw before.
    reg.upsert_task(&g.id, "orch", Some("t-1"), patch(None, Some("in-progress"), None)).unwrap();
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.contains("in-progress"), "the edit itself landed");
    assert!(!text.contains("\"parent\""), "a flat board must not gain a parent key:\n{text}");
    assert!(!text.contains("\"kind\""), "...nor a kind key:\n{text}");

    // The compact row an orchestrator reads says the same thing — and carries
    // no zero counts either: absent IS "this board has no hierarchy" (#245's
    // pay-nothing-for-what-you-don't-use rule).
    let row = serde_json::to_value(&reg.task_summaries(&g.id)[0]).unwrap();
    assert!(row.get("parent").is_none() && row.get("kind").is_none(), "flat rows carry neither: {row}");
    assert!(row.get("children").is_none() && row.get("children_done").is_none(), "nor a zero count: {row}");
}

/// The `list_tasks` row projection (#958): container, level, and DIRECT child
/// counts derived in the same per-call board scan `ready` already uses — counts
/// only, never the child rows (#245).
#[test]
fn list_rows_carry_the_container_and_derived_child_counts() {
    let board = vec![
        kinded("t-1", "queued", "epic", None),
        kinded("t-2", "done", "story", Some("t-1")),
        kinded("t-3", "queued", "task", Some("t-1")),
        kinded("t-4", "queued", "task", Some("t-2")),
    ];
    let rows = board_summaries(&board);
    assert_eq!(rows[0].children, 2, "DIRECT children only — t-4 is a grandchild, not the epic's child");
    assert_eq!(rows[0].children_done, 1);

    let v = serde_json::to_value(&rows[0]).unwrap();
    assert_eq!(v["kind"], json!("epic"));
    assert_eq!(v["children"], json!(2));
    assert_eq!(v["children_done"], json!(1));
    assert!(v.get("parent").is_none(), "a root pays nothing for the field: {v}");
    // A count must never expand into the child row itself (#245's size rule,
    // which a tree shape is exactly what tempts an expansion of).
    assert!(!v.to_string().contains("task t-2"), "counts only, never the child's title: {v}");

    let leaf = serde_json::to_value(&rows[3]).unwrap();
    assert_eq!(leaf["parent"], json!("t-2"));
    assert!(leaf.get("children").is_none(), "a childless row pays nothing for the counts: {leaf}");

    // A container whose own status lags its children keeps its own status —
    // nothing here ever rolls up INTO `status` (the `ready` precedent: derived
    // at read time, never written back).
    assert_eq!(rows[1].status, "done");
    assert_eq!(rows[1].children, 1);
    assert_eq!(rows[1].children_done, 0, "a done container with an unfinished child says so");
}

// ---------- #958 slice R: readiness climbs the container chain ----------

/// A row is startable only when its own deps AND every container above it are
/// clear (#958 slice R). The failure direction is what made this worth
/// changing: a slice inside a feature that could not itself start used to
/// advertise itself as startable, so the board's answer to "what can begin
/// now" included work that could not begin.
#[test]
fn readiness_climbs_the_container_chain() {
    let board = vec![
        linked("t-1", "in-progress", &[], &[]),
        // The feature, waiting on t-1; the slice inside it declares no deps.
        Task { kind: Some("feature".into()), ..linked("feat", "queued", &["t-1"], &[]) },
        kinded("slice", "queued", "task", Some("feat")),
    ];
    assert!(unmet_deps(&board[2], &board).is_empty(), "a container is still not a dependency");
    assert_eq!(
        blocking_ancestor(&board[2], &board),
        Some("feat"),
        "the container it sits in is what is holding it"
    );
    assert!(!task_ready(&board[2], &board), "a slice whose feature is still waiting cannot start");
    assert!(!task_ready(&board[1], &board), "and the feature is blocked the ordinary way");

    // Finish what the feature was waiting on and BOTH become startable in the
    // same read — nothing was written to make that happen.
    let cleared: Vec<Task> =
        board.iter().map(|t| Task { status: if t.id == "t-1" { "done".into() } else { t.status.clone() }, ..t.clone() }).collect();
    assert_eq!(blocking_ancestor(&cleared[2], &cleared), None);
    assert!(task_ready(&cleared[2], &cleared) && task_ready(&cleared[1], &cleared));

    // The WHOLE chain, and the NEAREST blocker: one level of walking would miss
    // a grandparent's dep, and a caller that names the wrong row sends a human
    // to the wrong place.
    let deep = vec![
        linked("t-1", "queued", &[], &[]),
        linked("t-2", "queued", &[], &[]),
        Task { kind: Some("epic".into()), ..linked("epic", "queued", &["t-1"], &[]) },
        Task { parent: Some("epic".into()), ..linked("feat", "queued", &["t-2"], &[]) },
        kinded("slice", "queued", "task", Some("feat")),
    ];
    assert_eq!(blocking_ancestor(&deep[4], &deep), Some("feat"), "nearest first, not the root");
    let only_epic: Vec<Task> =
        deep.iter().map(|t| if t.id == "feat" { Task { deps: vec![], ..t.clone() } } else { t.clone() }).collect();
    assert_eq!(
        blocking_ancestor(&only_epic[4], &only_epic),
        Some("epic"),
        "two levels up still reaches the grandchild"
    );
    assert!(!board_summaries(&only_epic)[4].ready, "and the projected row says so");

    // Orthogonality survives: a row's OWN dep still blocks it regardless of
    // where it sits, and `related` still participates in nothing.
    let own = vec![
        linked("t-1", "queued", &[], &[]),
        kinded("feat", "queued", "feature", None),
        Task { parent: Some("feat".into()), ..linked("slice", "queued", &["t-1"], &["feat"]) },
    ];
    assert_eq!(blocking_ancestor(&own[2], &own), None, "nothing above it is waiting");
    assert!(!task_ready(&own[2], &own), "but its own dep is");
    assert_eq!(unmet_deps(&own[2], &own), ["t-1"]);
}

/// Only an ancestor's **deps** participate, never its `status` (#958 slice R).
/// `blocked` is the status for blockers OUTSIDE the board — it says nothing
/// about the work inside a container — and a feature at `in-progress` is the
/// normal state while its slices are the startable work. Reading either would
/// make a slice's readiness a function of how promptly someone maintains the
/// container row, where deps are the ordering primitive #582 defined.
///
/// Swept over `TASK_STATUSES` itself rather than a list written out here: a
/// ninth status added later must be covered by this sweep the day it lands, not
/// silently escape it (CLAUDE.md — a concrete list goes stale).
#[test]
fn an_ancestors_status_never_enters_readiness() {
    for status in TASK_STATUSES {
        let board = vec![
            kinded("feat", status, "feature", None),
            kinded("slice", "queued", "task", Some("feat")),
        ];
        assert_eq!(blocking_ancestor(&board[1], &board), None, "container at {status} blocks nothing");
        assert!(task_ready(&board[1], &board), "a child of a {status} container is startable");
    }
}

/// A hand-edited container tolerates rather than wedges (§5 of
/// docs/design/task-hierarchy.md) — the OPPOSITE direction from an unknown dep
/// id, which deliberately blocks. The asymmetry is the point: an unknown dep is
/// an ordering claim that cannot be verified, while an unknown container is a
/// row with no container at all, and blocking it forever would hide work with
/// nothing on the board explaining why.
#[test]
fn a_hand_edited_container_never_wedges_readiness() {
    let hand_edited = vec![
        kinded("t-1", "queued", "task", Some("t-404")),
        kinded("t-2", "queued", "task", Some("t-3")),
        kinded("t-3", "queued", "task", Some("t-2")),
        kinded("t-4", "queued", "task", Some("t-4")),
    ];
    let rows = board_summaries(&hand_edited);
    assert!(rows[0].ready, "an orphan parent blocks nothing — there is no container to wait on");
    assert!(rows[1].ready && rows[2].ready, "nor does a hand-edited container cycle");
    assert!(rows[3].ready, "nor a self-parent: its own deps are unmet_deps' answer, not this one's");
    assert_eq!(rows[1].children, 1, "the counts still answer, without walking into the loop");

    // A cycle whose member carries a REAL unmet dep still reports it, having
    // terminated on the repeat rather than spinning. (Termination, not
    // deduplication: a cycle that does not contain the start row re-scans its
    // entry member once — see `blocking_ancestor`'s doc.)
    let cyclic_but_blocked = vec![
        linked("t-0", "queued", &[], &[]),
        kinded("t-1", "queued", "task", Some("t-2")),
        Task { parent: Some("t-1".into()), ..linked("t-2", "queued", &["t-0"], &[]) },
    ];
    assert_eq!(blocking_ancestor(&cyclic_but_blocked[1], &cyclic_but_blocked), Some("t-2"));
    assert!(!task_ready(&cyclic_but_blocked[1], &cyclic_but_blocked));
}

/// Readiness is a HINT; `claim` is the GATE — and §7's metadata-only stance
/// binds the gate, not the hint. So the claim guard still judges a row's OWN
/// deps and never reads `parent`: a hand-edited container can dim a row on the
/// board, and can never refuse a write. This asymmetry is deliberate, and this
/// test is what stops it being "fixed" into a hierarchy-reading gate.
#[test]
fn a_claim_is_judged_on_the_rows_own_deps_never_its_container() {
    let (reg, _d) = test_registry();
    let gid = board_with(&reg, &["the blocker", "the feature", "the slice"]);
    reg.upsert_task(&gid, "orch", Some("t-2"), deps_patch(&["t-1"])).unwrap();
    reg.upsert_task(&gid, "orch", Some("t-3"), parent_patch("t-2")).unwrap();

    let board = reg.tasks(&gid);
    let slice = board.iter().find(|t| t.id == "t-3").unwrap();
    assert!(!task_ready(slice, &board), "the board says the slice is not startable yet");

    // …and the claim still lands, because the slice's own deps are clear.
    let claimed = reg.upsert_task(&gid, "orch", Some("t-3"), claim_patch("w-1")).unwrap();
    assert_eq!(
        (claimed.status.as_str(), claimed.assignee.as_deref()),
        ("in-progress", Some("w-1")),
        "hierarchy is metadata: it never decides whether an action may happen (§7)"
    );
}

#[test]
fn hierarchy_args_round_trip_through_the_mcp_shim() {
    let (reg, _d, co, cw) = setup_mcp();
    let call = |c: &Caller, args: Value| {
        dispatch(&reg, c, "tools/call", &json!({ "name": "upsert_task", "arguments": args })).unwrap()
    };
    let text_of = |r: &Value| r["content"][0]["text"].as_str().unwrap_or_default().to_string();
    // A row created WITH a level is minted at that level's prefix (#1156);
    // one created without keeps `t-`, and both draw on the same counter.
    call(&co, json!({ "title": "the epic", "kind": "epic" }));
    call(&co, json!({ "title": "slice A" }));
    assert!(reg.get_task(&co.group, "e-1").is_some(), "an epic is minted e-N through the shim");
    assert!(reg.get_task(&co.group, "t-2").is_some(), "a level-less row keeps t-N, off the shared counter");

    // Both fields parse and land.
    assert_eq!(call(&co, json!({ "id": "t-2", "parent": "e-1", "kind": "feature" }))["isError"], false);
    let t2 = reg.get_task(&co.group, "t-2").unwrap();
    assert_eq!((t2.parent.as_deref(), t2.kind.as_deref()), (Some("e-1"), Some("feature")));
    assert_eq!(reg.get_task(&co.group, "e-1").unwrap().kind.as_deref(), Some("epic"), "kind lands on a CREATE too");
    assert_eq!(t2.id, "t-2", "...and levelling a row NEVER rewrites the id it was minted with");

    // list_tasks — readable by ANY role — carries the container and the count.
    let listed = dispatch(&reg, &cw, "tools/call", &json!({ "name": "list_tasks", "arguments": {} })).unwrap();
    let rows = text_of(&listed);
    assert!(rows.contains(r#""parent":"e-1""#), "compact rows carry the container id: {rows}");
    assert!(rows.contains(r#""children":1"#), "...and the derived child count: {rows}");

    // Registry rejections surface as tool errors WITH the reason — a caller
    // must be able to tell a cycle from a permission problem, or from the
    // ladder. The cycle pair is level-less, so it is the CYCLE being reported
    // and not the ladder refusing first.
    call(&co, json!({ "title": "plain A" }));
    call(&co, json!({ "title": "plain B" }));
    assert_eq!(call(&co, json!({ "id": "t-4", "parent": "t-3" }))["isError"], false);
    let cyc = call(&co, json!({ "id": "t-3", "parent": "t-4" }));
    assert_eq!(cyc["isError"], true);
    assert!(text_of(&cyc).contains("cycle"), "the cycle reason survives the shim: {}", text_of(&cyc));
    let bad_kind = call(&co, json!({ "id": "t-2", "kind": "saga" }));
    assert_eq!(bad_kind["isError"], true);
    assert!(text_of(&bad_kind).contains("epic"), "the vocabulary is named back: {}", text_of(&bad_kind));
    let bad_rung = call(&co, json!({ "id": "t-2", "kind": "story" }));
    assert_eq!(bad_rung["isError"], true);
    assert!(
        text_of(&bad_rung).contains("must sit inside"),
        "and so does the ladder's own refusal: {}",
        text_of(&bad_rung)
    );

    // Empty string clears through this same shim, the way `pr` does — for BOTH
    // fields. The level goes first: a feature promoted to top level is a shape
    // the ladder refuses, so the clear order is itself part of the contract.
    assert_eq!(call(&co, json!({ "id": "t-2", "kind": "" }))["isError"], false);
    assert_eq!(reg.get_task(&co.group, "t-2").unwrap().kind, None, "\"\" clears the level, it is not an invalid kind");
    assert_eq!(call(&co, json!({ "id": "t-2", "parent": "" }))["isError"], false);
    assert_eq!(reg.get_task(&co.group, "t-2").unwrap().parent, None);

    // And the board stays orchestrator-write.
    assert_eq!(call(&cw, json!({ "id": "t-2", "parent": "e-1" }))["isError"], true);
}

/// rev-611 NB1: the advertised schema must admit the clear its own description
/// documents. `kind` carries a JSON-schema `enum`, so a client that enforces
/// the enum — which is the point of publishing one — could otherwise never
/// send the `""` the same property's description calls the clear. `parent`
/// has no enum, so its clear was always reachable; this is `kind`'s problem
/// alone, and it is a contract defect rather than a backend one (the backend's
/// trim-then-check carve-out has always treated `""` as the clear).
#[test]
fn the_upsert_task_schema_admits_the_kind_clear_it_documents() {
    let (reg, _d, co, _cw) = setup_mcp();
    let listed = dispatch(&reg, &co, "tools/list", &json!({})).unwrap();
    let upsert = listed["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "upsert_task")
        .expect("upsert_task is listed for an orchestrator");
    let kind = &upsert["inputSchema"]["properties"]["kind"];
    assert_eq!(
        kind["enum"],
        json!(["epic", "feature", "story", "task", ""]),
        "the four levels PLUS the empty-string clear: {kind}"
    );
    let described = kind["description"].as_str().unwrap_or_default();
    assert!(
        described.contains("clear"),
        "and the description still says so, so the two cannot drift apart silently: {described}"
    );
}

/// rev-611 NB3: a wrong-typed hierarchy arg is REFUSED, never silently
/// dropped — the rule `deps` already states in this same tool. Reporting
/// success for a `parent` that never landed is the worst failure of the set:
/// the caller believes it hung a slice under an epic and the board never heard
/// about it, which is a wrong TREE rather than one wrong field.
#[test]
fn wrong_typed_hierarchy_args_are_refused_not_silently_dropped() {
    let (reg, _d, co, _cw) = setup_mcp();
    let call = |args: Value| {
        dispatch(&reg, &co, "tools/call", &json!({ "name": "upsert_task", "arguments": args })).unwrap()
    };
    call(json!({ "title": "the epic" }));
    call(json!({ "title": "slice A" }));

    assert_eq!(call(json!({ "id": "t-2", "parent": 5 }))["isError"], true, "a number is not a task id");
    assert_eq!(call(json!({ "id": "t-2", "kind": ["epic"] }))["isError"], true, "nor is an array a level");
    assert_eq!(call(json!({ "id": "t-2", "parent": true }))["isError"], true);
    let t2 = reg.get_task(&co.group, "t-2").unwrap();
    assert_eq!((t2.parent, t2.kind), (None, None), "a refused arg parse changes nothing");

    // null is still "leave it alone", not a type error — that is what keeps an
    // omitted-vs-explicitly-null caller working the way every other field does.
    // (t-1 was minted level-less and KEEPS its t- id after becoming an epic:
    // #1156 grandfathers every existing id rather than rewriting references.)
    assert_eq!(call(json!({ "id": "t-1", "kind": "epic" }))["isError"], false);
    assert_eq!(call(json!({ "id": "t-2", "parent": "t-1", "kind": "feature" }))["isError"], false);
    assert_eq!(call(json!({ "id": "t-2", "parent": null, "kind": null }))["isError"], false);
    let t2 = reg.get_task(&co.group, "t-2").unwrap();
    assert_eq!((t2.parent.as_deref(), t2.kind.as_deref()), (Some("t-1"), Some("feature")), "null left both alone");
}

#[test]
fn copilot_trust_config_edit_preserves_content_and_dedupes() {
    let existing = "// User settings belong in settings.json.\n// This file is managed automatically.\n{\n  \"firstLaunchAt\": \"2026-07-04\",\n  \"trustedFolders\": [\n    \"C:\\\\Projects\\\\cattle-worker\"\n  ]\n}\n";
    // New folder: appended, comments and existing fields intact.
    let updated = add_trusted_folder(existing, r"C:\Projects\other").unwrap();
    assert!(updated.starts_with("// User settings"), "comment header must survive");
    assert!(updated.contains("firstLaunchAt"), "unknown fields must survive");
    assert!(updated.contains(r"C:\\Projects\\cattle-worker") || updated.contains("cattle-worker"));
    assert!(updated.contains("other"));
    // Already trusted: no rewrite at all.
    assert!(add_trusted_folder(existing, r"C:\Projects\cattle-worker").is_none());
    // A CASE/separator variant collapses only where the host filesystem is
    // case-insensitive and treats `/` as a separator — Windows (#803 review
    // B1). The fixture is a Windows-shaped path, so off Windows it is simply a
    // different string and appending it is the correct answer, not a bug. The
    // full platform matrix for this rule lives in
    // `path_keys_follow_the_hosts_own_path_semantics`, which drives BOTH
    // shapes on every CI platform rather than only the host's.
    let case_variant = add_trusted_folder(existing, r"c:/projects/cattle-worker");
    assert_eq!(
        case_variant.is_none(),
        cfg!(windows),
        "a case/separator variant is the same folder on Windows and a different one elsewhere"
    );
    // Empty/missing config: created from scratch.
    let fresh = add_trusted_folder("", r"C:\Projects\x").unwrap();
    assert!(fresh.contains("trustedFolders"));
    // Corrupt config must NOT be clobbered.
    assert!(add_trusted_folder("// c\n{ not json", r"C:\x").is_none());
}

// #475: `pre_trust_copilot_folder` is the I/O wrapper around `add_trusted_folder`
// above — it resolves a home directory, reads that home's real config.json, and
// (best-effort) writes it back. That resolve-read-write path had no test seam at
// all, so nothing ever exercised it; the tests below are what a seam is for.
//
// #502 round 3 folded the free entry point into `OrchRegistry::pre_trust_
// copilot_folder`, now the only one, so containment cannot be bypassed by
// calling the wrong one. These drive the method instead — semantics are
// unchanged for them: the thread-local seam is still consulted FIRST, so a
// fixtured home wins exactly as before, and which registry they go through
// is irrelevant to the path under test.

#[test]
fn pre_trust_copilot_folder_writes_fixture_and_leaves_real_home_untouched() {
    // Snapshot the real home's copilot config (if any) so this test can prove
    // its own write never lands there — the exact failure mode #475 flagged:
    // a test reaching this function would otherwise modify the developer's
    // REAL ~/.copilot/config.json rather than a fixture.
    let real_config = dirs::home_dir().map(|h| h.join(".copilot").join("config.json"));
    let real_before = real_config.as_ref().and_then(|p| fs::read(p).ok());

    let fixture = tempfile::tempdir().unwrap();
    fs::write(
        fixture.path().join("config.json"),
        "// User settings belong in settings.json.\n{\n  \"firstLaunchAt\": \"2026-07-04\",\n  \"trustedFolders\": []\n}\n",
    )
    .unwrap();
    set_copilot_trust_home_for_test(Some(fixture.path().to_path_buf()));

    let (reg, _dir) = test_registry();
    reg.pre_trust_copilot_folder(r"C:\Projects\some-repo", r"C:\Projects\some-agent-workspace");

    set_copilot_trust_home_for_test(None);

    let written = fs::read_to_string(fixture.path().join("config.json")).unwrap();
    assert!(written.contains("some-agent-workspace"), "folder must be trusted in the fixture: {written}");
    assert!(written.contains("firstLaunchAt"), "existing fields must survive the write: {written}");

    let real_after = real_config.as_ref().and_then(|p| fs::read(p).ok());
    assert_eq!(real_before, real_after, "a seamed call must never touch the real home's copilot config");
}

#[test]
fn pre_trust_copilot_folder_creates_config_when_home_is_missing() {
    // The seam's target directory need not exist yet — `pre_trust_copilot_folder`
    // must `create_dir_all` it, same as it would for a first-ever `~/.copilot`.
    let fixture = tempfile::tempdir().unwrap();
    let home = fixture.path().join("not-yet-created");
    set_copilot_trust_home_for_test(Some(home.clone()));

    let (reg, _dir) = test_registry();
    reg.pre_trust_copilot_folder(r"C:\Projects\fresh-repo", r"C:\Projects\fresh-workspace");

    set_copilot_trust_home_for_test(None);

    let written = fs::read_to_string(home.join("config.json"))
        .expect("config.json must be created along with its parent directory");
    assert!(written.contains("fresh-workspace"));
    assert!(written.contains("trustedFolders"));
}

#[test]
fn pre_trust_copilot_folder_never_clobbers_an_unparseable_config() {
    let fixture = tempfile::tempdir().unwrap();
    fs::write(fixture.path().join("config.json"), "// c\n{ not json").unwrap();
    set_copilot_trust_home_for_test(Some(fixture.path().to_path_buf()));

    let (reg, _dir) = test_registry();
    reg.pre_trust_copilot_folder(r"C:\Projects\x", r"C:\Projects\x");

    set_copilot_trust_home_for_test(None);

    let unchanged = fs::read_to_string(fixture.path().join("config.json")).unwrap();
    assert_eq!(unchanged, "// c\n{ not json", "an unparseable config must be left exactly alone, never clobbered");
}

/// #803 review B1: loomux ships macOS and Linux builds, not just Windows, and
/// the location key it writes into copilot's `permissions-config.json` must be
/// the one copilot will look up on THAT host.
///
/// The regression this pins is not hypothetical: an unconditional `'/' -> '\\'`
/// rewrite turns `/home/u/repo` into `\home\u\repo`, a key copilot never
/// matches while looking up `/home/u/repo` — a permission write that silently
/// does nothing, which is the exact failure class #802 reports.
///
/// Both shapes are driven explicitly, so a Linux CI leg still tests the Windows
/// rule and vice versa — a `#[cfg]`-split implementation would compile only its
/// host's half and leave the other half unexercised everywhere.
#[test]
fn path_keys_follow_the_hosts_own_path_semantics() {
    // ── Windows semantics: `/` and `\` are both separators, case folds.
    assert_eq!(normalize_path_key_for(r"C:/Projects/demo/", true), r"C:\Projects\demo");
    assert_eq!(normalize_path_key_for(r"C:\Projects\demo", true), r"C:\Projects\demo");
    assert!(same_path_key_for(r"C:\Projects\Demo", "c:/projects/demo/", true));

    // ── POSIX semantics: `/` only, case is significant.
    assert_eq!(normalize_path_key_for("/home/u/repo/", false), "/home/u/repo");
    assert_eq!(normalize_path_key_for("/home/u/repo", false), "/home/u/repo");
    assert!(same_path_key_for("/home/u/repo", "/home/u/repo/", false));
    assert!(
        !same_path_key_for("/srv/App", "/srv/app", false),
        "case-folding off Windows would merge two directories that genuinely differ on Linux"
    );

    // A POSIX path must survive normalization UNCHANGED — this is the blocker
    // itself: `\home\u\repo` is a key copilot would never match.
    assert_eq!(normalize_path_key_for("/home/u/repo", false), "/home/u/repo");
    assert!(
        !normalize_path_key_for("/home/u/repo", false).contains('\\'),
        "a POSIX key must never grow a backslash"
    );
    // …and a backslash inside a POSIX path is a legal FILENAME character, so it
    // is data, not a separator: rewriting or trimming it would corrupt the path.
    assert_eq!(normalize_path_key_for(r"/home/u/we\ird", false), r"/home/u/we\ird");
    assert_eq!(normalize_path_key_for(r"/home/u/trailing\", false), r"/home/u/trailing\");

    // A bare root keeps its separator rather than normalizing to the empty key.
    assert_eq!(normalize_path_key_for("/", false), "/");
    assert_eq!(normalize_path_key_for(r"C:\", true), r"C:");

    // …and the PRODUCTION path agrees with whichever half applies here. Driven
    // through `copilot_permissions_grant` rather than the private host
    // wrappers: what matters is not that a one-line wrapper delegates, it is
    // that the key actually written to the file is one THIS host's copilot
    // would look up.
    let (loc, wt) = if cfg!(windows) {
        (r"C:\Projects\demo", r"C:\Projects\demo-worktrees\w-1")
    } else {
        ("/home/u/demo", "/home/u/demo-worktrees/w-1")
    };
    let written = copilot_permissions_grant("", loc, wt, "orrerix").unwrap();
    let v: serde_json::Value = serde_json::from_str(&written).unwrap();
    assert!(
        !v["locations"][loc].is_null(),
        "the location key must be this host's own path shape, verbatim: {written}"
    );
    if !cfg!(windows) {
        assert!(
            !written.contains(r"\home"),
            "the #803 B1 regression: a POSIX key rewritten with backslashes is one copilot \
             never matches, so the grant silently does nothing: {written}"
        );
    }
}

/// #803 review: a block that re-declares something loomux already grants must
/// not make the one `--allow-tool` value say it twice — that value is now the
/// single place a reader checks to see what an agent was granted.
#[test]
fn copilot_tool_permissions_do_not_repeat_a_pattern() {
    // The MCP grant and `shell(git:*)` are both already in the attended baseline.
    let extra = vec!["orrerix".to_string(), "shell(git:*)".to_string(), "shell(make:*)".to_string()];
    let (allow, _) = copilot_tool_permissions(false, Containment::None, &extra);
    assert_eq!(
        allow,
        ["orrerix", "shell(git:*)", "shell(gh:*)", "shell(make:*)"],
        "each pattern appears once, at its FIRST position: {allow:?}"
    );
}

// ── #802: copilot's DOCUMENTED permission store ────────────────────────────
//
// `trustedFolders` in `config.json` is the pre-1.0.77 surface and appears in
// none of copilot's current reference pages: `config.json` is now described as
// automatically-managed application state whose user settings migrate to
// `settings.json`, while directory and tool grants are documented to live in
// `permissions-config.json`. These pin the grant loomux now writes there —
// including the MCP approval, which is the durable form of `--allow-tool
// loomux` and whose absence is exactly what #802 reported ("the CLI lists the
// loomux MCP server as available, but the agent has no permission to use its
// tools").

#[test]
fn copilot_permissions_grant_writes_the_documented_shape() {
    const REPO: &str = r"C:\Projects\demo";
    const WORKTREE: &str = r"C:\Projects\demo-worktrees\fix-1";

    let fresh = copilot_permissions_grant("", REPO, WORKTREE, "orrerix").unwrap();
    let v: serde_json::Value = serde_json::from_str(&fresh).unwrap();
    let entry = &v["locations"][REPO];
    assert_eq!(
        entry["allowed_directories"],
        json!([WORKTREE]),
        "the agent's own workspace must be added to the path gate: {fresh}"
    );
    assert_eq!(
        entry["tool_approvals"],
        json!([{ "kind": "mcp", "serverName": "orrerix", "toolName": null }]),
        "toolName: null is the documented 'every tool on the server' approval: {fresh}"
    );

    // Idempotent: re-granting the same pair rewrites nothing, so a suite (or a
    // long-lived group) can't grow this file without bound the way the
    // `trustedFolders` list once did.
    assert!(
        copilot_permissions_grant(&fresh, REPO, WORKTREE, "orrerix").is_none(),
        "an already-granted location must produce no write at all"
    );

    // A SECOND worktree under the same repo joins the existing location rather
    // than creating a new one — that is the point of keying by git root.
    let second =
        copilot_permissions_grant(&fresh, REPO, r"C:\Projects\demo-worktrees\fix-2", "orrerix")
            .expect("a new workspace is a real change");
    let v: serde_json::Value = serde_json::from_str(&second).unwrap();
    assert_eq!(v["locations"].as_object().unwrap().len(), 1, "one repo, one location key: {second}");
    assert_eq!(v["locations"][REPO]["allowed_directories"].as_array().unwrap().len(), 2);
    assert_eq!(
        v["locations"][REPO]["tool_approvals"].as_array().unwrap().len(),
        1,
        "the MCP approval must not be appended twice: {second}"
    );

    // The user's own saved approvals — for other repos and for this one — are
    // additive-only: this file is theirs, loomux only ever adds to it.
    let existing = serde_json::to_string_pretty(&json!({
        "locations": {
            r"C:\Other\repo": { "tool_approvals": [{ "kind": "write" }] },
            r"C:\Projects\demo": { "tool_approvals": [{ "kind": "commands", "commandIdentifiers": ["git:*"] }] },
        }
    }))
    .unwrap();
    let merged = copilot_permissions_grant(&existing, REPO, WORKTREE, "orrerix").unwrap();
    let v: serde_json::Value = serde_json::from_str(&merged).unwrap();
    assert_eq!(v["locations"][r"C:\Other\repo"]["tool_approvals"], json!([{ "kind": "write" }]));
    let approvals = v["locations"][REPO]["tool_approvals"].as_array().unwrap();
    assert_eq!(approvals.len(), 2, "the user's own approval must survive: {merged}");
    assert_eq!(approvals[0]["kind"], "commands");
    assert_eq!(approvals[1]["serverName"], "orrerix");

    // An existing key naming the SAME directory is REUSED, never duplicated:
    // copilot loads exactly one key ("If the key doesn't match, the saved
    // approvals won't apply"), so a second spelling would strand the user's own
    // approvals under whichever one copilot didn't pick.
    //
    // What counts as "the same directory" is the host's rule, not a universal
    // one (#803 review B1): Windows folds case and separators, POSIX folds only
    // a trailing separator. Each leg uses a variant that is genuinely the same
    // directory ON THAT HOST, so the property under test is identical
    // everywhere even though the spelling can't be.
    let existing_key = if cfg!(windows) { "c:/projects/demo/" } else { r"C:\Projects\demo/" };
    let mut locs = serde_json::Map::new();
    locs.insert(existing_key.to_string(), json!({ "tool_approvals": [{ "kind": "write" }] }));
    let seeded =
        serde_json::to_string_pretty(&json!({ "locations": serde_json::Value::Object(locs) }))
            .unwrap();
    let variant = copilot_permissions_grant(&seeded, REPO, WORKTREE, "orrerix").unwrap();
    let v: serde_json::Value = serde_json::from_str(&variant).unwrap();
    assert_eq!(
        v["locations"].as_object().unwrap().len(),
        1,
        "a variant spelling of the same path is one location, not two: {variant}"
    );
    assert_eq!(v["locations"][existing_key]["tool_approvals"].as_array().unwrap().len(), 2);

    // A pre-existing approval for ONE tool on the same server is a narrower
    // grant and must NOT be mistaken for the server-wide one — otherwise a user
    // who once approved `report` by hand would silently keep every other loomux
    // tool ungranted, which is #802's symptom with extra steps.
    let narrow = serde_json::to_string_pretty(&json!({
        "locations": { r"C:\Projects\demo": { "tool_approvals": [
            { "kind": "mcp", "serverName": "orrerix", "toolName": "report" }
        ] } }
    }))
    .unwrap();
    let widened = copilot_permissions_grant(&narrow, REPO, WORKTREE, "orrerix")
        .expect("a per-tool approval does not satisfy the server-wide grant");
    let v: serde_json::Value = serde_json::from_str(&widened).unwrap();
    let approvals = v["locations"][REPO]["tool_approvals"].as_array().unwrap();
    assert_eq!(approvals.len(), 2, "{widened}");
    assert!(approvals.iter().any(|a| a["toolName"].is_null()), "{widened}");

    // Corrupt file: never clobbered, same policy as `add_trusted_folder`.
    assert!(copilot_permissions_grant("{ not json", REPO, WORKTREE, "orrerix").is_none());
}

#[test]
fn pre_trust_writes_both_permission_surfaces_and_keys_the_grant_by_repo() {
    let fixture = tempfile::tempdir().unwrap();
    set_copilot_trust_home_for_test(Some(fixture.path().to_path_buf()));

    let (reg, _dir) = test_registry();
    reg.pre_trust_copilot_folder(r"C:\Projects\demo", r"C:\Projects\demo-worktrees\w-1");

    set_copilot_trust_home_for_test(None);

    let perms = fs::read_to_string(fixture.path().join("permissions-config.json"))
        .expect("the documented permission store must be written");
    let v: serde_json::Value = serde_json::from_str(&perms).unwrap();
    // Keyed by the REPO, not the worktree: copilot resolves a linked worktree
    // back to the main repository root for permission scoping, so a grant filed
    // under the worktree would never be found.
    let entry = &v["locations"][r"C:\Projects\demo"];
    assert!(!entry.is_null(), "the grant must be keyed by the repo's git root: {perms}");
    assert_eq!(entry["allowed_directories"], json!([r"C:\Projects\demo-worktrees\w-1"]));
    assert_eq!(entry["tool_approvals"][0]["serverName"], "orrerix");
    assert!(entry["tool_approvals"][0]["toolName"].is_null());

    // The legacy surface is still written — the docs' silence about
    // `trustedFolders` is an absence, not a documented removal, and loomux
    // cannot see which copilot build a machine runs.
    let legacy = fs::read_to_string(fixture.path().join("config.json")).unwrap();
    assert!(legacy.contains("trustedFolders") && legacy.contains("w-1"), "{legacy}");
}

#[test]
fn pre_trust_edits_the_legacy_extensionless_permissions_file_when_that_is_the_live_one() {
    // "Older builds used an extensionless file named `permissions-config`. If
    // `permissions-config.json` doesn't exist but the extensionless file does,
    // the CLI still honors the legacy file." Creating the `.json` beside it
    // would make copilot stop honoring the legacy one — silently discarding
    // every approval the user had saved.
    let fixture = tempfile::tempdir().unwrap();
    fs::write(
        fixture.path().join("permissions-config"),
        serde_json::to_string_pretty(&json!({
            "locations": { r"C:\Projects\demo": { "tool_approvals": [{ "kind": "write" }] } }
        }))
        .unwrap(),
    )
    .unwrap();
    set_copilot_trust_home_for_test(Some(fixture.path().to_path_buf()));

    let (reg, _dir) = test_registry();
    reg.pre_trust_copilot_folder(r"C:\Projects\demo", r"C:\Projects\demo");

    set_copilot_trust_home_for_test(None);

    assert!(
        !fixture.path().join("permissions-config.json").exists(),
        "writing the .json name would orphan the user's live legacy file"
    );
    let text = fs::read_to_string(fixture.path().join("permissions-config")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    let approvals = v["locations"][r"C:\Projects\demo"]["tool_approvals"].as_array().unwrap();
    assert_eq!(approvals.len(), 2, "the user's own approval must survive the grant: {text}");
    assert_eq!(approvals[1]["serverName"], "orrerix");
}

/// The pure posture function both copilot command builders share — pinned here
/// so a regression in the ORDER or CONTENT of the lists is caught without
/// having to read it back out of a formatted command line.
#[test]
fn copilot_tool_permissions_lead_with_the_mcp_grant() {
    let extra = vec!["shell(make:*)".to_string()];

    let (allow, deny) = copilot_tool_permissions(true, Containment::None, &extra);
    assert_eq!(allow, ["orrerix", "shell(make:*)"]);
    assert!(deny.is_empty());

    let (allow, deny) = copilot_tool_permissions(false, Containment::None, &extra);
    assert_eq!(allow, ["orrerix", "shell(git:*)", "shell(gh:*)", "shell(make:*)"]);
    assert!(deny.is_empty());

    // A reviewer (NoEdits) denies writes and nothing git-related — its shell IS
    // its job (#462).
    let (allow, deny) = copilot_tool_permissions(true, Containment::NoEdits, &[]);
    assert_eq!(allow, ["orrerix"]);
    assert_eq!(deny, ["write"]);

    // A planner (ReadOnly) is the ladder's top rung: the reviewer's denials
    // PLUS the git-mutation pair, never instead of them.
    let (_, deny) = copilot_tool_permissions(true, Containment::ReadOnly, &[]);
    assert_eq!(deny, ["write", "shell(git commit)", "shell(git push)"]);
}

#[test]
fn scratch_planted_bare_registry_construction_3498() {
    let d = tempfile::tempdir().unwrap();
    let _reg = OrchRegistry::new(d.path().to_path_buf());
}
