// Icons for the structured-pane mock.
//
// Drawn, not typed. CLAUDE.md's icon convention and the Impeccable craft floor
// both refuse unicode glyphs or emoji standing in for an icon system, so every
// mark below is authored SVG in the same geometry the app already ships:
// src/icons.ts pins Lucide 1.31.0 (ISC) — a 24x24 viewBox, 2px strokes, round
// caps and joins, `currentColor`. These are hand-authored in that geometry
// rather than copied, so nothing here needs a vendor pin of its own; where a
// shape matches a Lucide primitive it matches because the grid and the stroke
// rules are the same, which is the point of having them.
//
// A mark never names a hue. It names a ROLE, and CSS resolves the role to a
// token (see .tool[data-family] in pane.css) — the same rule as the app's
// role->dye table, so a consumer cannot pick a colour by hand.

const VIEWBOX = "0 0 24 24";

const BODY = {
  // --- transcript structure
  chevron: `<path d="m6 9 6 6 6-6"/>`,
  inbound: `<path d="M12 5v14"/><path d="m19 12-7 7-7-7"/>`,
  human: `<path d="M19 21v-2a4 4 0 0 0-4-4H9a4 4 0 0 0-4 4v2"/><circle cx="12" cy="7" r="4"/>`,
  fold: `<path d="M21 8H3"/><path d="M21 12H9"/><path d="M21 16H3"/>`,

  // --- thinking. Three descending strokes: the mark reads as a train of
  //     thought at 13px, which a spiral does not — an involute drawn on a 24px
  //     grid collapses into an illegible blob once it is scaled to a line of
  //     11px text, and this block's mark is only ever seen at that size. It
  //     belongs to nothing else in the app, which is the point: thinking is the
  //     one block that is the model's process rather than its product.
  spiral: `<path d="M4 7h10"/><path d="M7 12h13"/><path d="M4 17h9"/>`,

  // --- tool families
  file: `<path d="M6 22a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h8l6 6v12a2 2 0 0 1-2 2z"/><path d="M14 2v5a1 1 0 0 0 1 1h5"/>`,
  pencil: `<path d="M21.2 6.8a1 1 0 0 0-4-4L3.8 16.2a2 2 0 0 0-.5.8l-1.3 4.4a.5.5 0 0 0 .6.6l4.4-1.3a2 2 0 0 0 .8-.5z"/><path d="m15 5 4 4"/>`,
  search: `<circle cx="11" cy="11" r="7"/><path d="m21 21-4.3-4.3"/>`,
  terminal: `<path d="m4 17 6-5-6-5"/><path d="M12 19h8"/>`,
  branch: `<circle cx="6" cy="6" r="3"/><circle cx="18" cy="18" r="3"/><path d="M6 9v6a3 3 0 0 0 3 3h6"/>`,
  globe: `<circle cx="12" cy="12" r="10"/><path d="M12 2a14.5 14.5 0 0 0 0 20 14.5 14.5 0 0 0 0-20"/><path d="M2 12h20"/>`,
  users: `<path d="M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2"/><circle cx="9" cy="7" r="4"/><path d="M22 21v-2a4 4 0 0 0-3-3.87"/><path d="M16 3.13a4 4 0 0 1 0 7.75"/>`,
  box: `<path d="M21 8v8a2 2 0 0 1-1 1.73l-7 4a2 2 0 0 1-2 0l-7-4A2 2 0 0 1 3 16V8a2 2 0 0 1 1-1.73l7-4a2 2 0 0 1 2 0l7 4A2 2 0 0 1 21 8"/><path d="m3.3 7 8.7 5 8.7-5"/><path d="M12 22V12"/>`,

  // --- events
  shield: `<path d="M20 13c0 5-3.5 7.5-7.66 8.95a1 1 0 0 1-.67-.01C7.5 20.5 4 18 4 13V6a1 1 0 0 1 1-1c2 0 4.5-1.2 6.24-2.72a1.17 1.17 0 0 1 1.52 0C14.51 3.81 17 5 19 5a1 1 0 0 1 1 1z"/>`,
  compact: `<path d="m8 3-4 4 4 4"/><path d="M4 7h13a3 3 0 0 1 0 6h-1"/><path d="m16 21 4-4-4-4"/><path d="M20 17H7a3 3 0 0 1 0-6h1"/>`,
  layers: `<path d="m12 2 9 5-9 5-9-5z"/><path d="m3 12 9 5 9-5"/><path d="m3 17 9 5 9-5"/>`,
  alert: `<path d="M12 9v4"/><path d="M12 17h.01"/><circle cx="12" cy="12" r="10"/>`,
  bolt: `<path d="M13 2 4.1 12.9a1 1 0 0 0 .8 1.6H11l-1 7.5 8.9-10.9a1 1 0 0 0-.8-1.6H12z"/>`,

  // --- playback
  play: `<path d="M6 3.5v17l14-8.5z"/>`,
  pause: `<path d="M8 4v16"/><path d="M16 4v16"/>`,
  rewind: `<path d="M3 12a9 9 0 1 0 3-6.7L3 8"/><path d="M3 3v5h5"/>`,
  gauge: `<path d="M12 14 8 8"/><circle cx="12" cy="14" r="8"/><path d="M12 6V3"/>`,
  motion: `<path d="M3 12h4l3-8 4 16 3-8h4"/>`,
};

/**
 * Render one mark. `cls` lands on the <svg> so CSS resolves the role to a
 * token; nothing here ever names a colour.
 */
export function icon(name, cls = "") {
  const body = BODY[name];
  if (!body) throw new Error(`icon: no such mark ${JSON.stringify(name)}`);
  return (
    `<svg viewBox="${VIEWBOX}" class="${cls}" fill="none" stroke="currentColor"` +
    ` stroke-width="2" stroke-linecap="round" stroke-linejoin="round"` +
    ` aria-hidden="true" focusable="false">${body}</svg>`
  );
}

/**
 * Which family a tool belongs to, and which mark reports it. The families are
 * the app's own icon roles (src/icons.ts ROLE_TOKEN): workspace, source,
 * content, vcs, fleet. A tool orrerix has never seen falls back to `box` in
 * plain ink rather than being given a hue it has not earned.
 */
const TOOL_MARKS = {
  Read: ["file", "content"],
  Write: ["pencil", "content"],
  Edit: ["pencil", "content"],
  MultiEdit: ["pencil", "content"],
  NotebookEdit: ["pencil", "content"],
  Glob: ["search", "workspace"],
  Grep: ["search", "workspace"],
  LS: ["search", "workspace"],
  Bash: ["terminal", "source"],
  BashOutput: ["terminal", "source"],
  WebFetch: ["globe", "source"],
  WebSearch: ["globe", "source"],
  Task: ["users", "fleet"],
  Agent: ["users", "fleet"],
  TodoWrite: ["fold", "fleet"],
  git: ["branch", "vcs"],
};

export function toolMark(name) {
  const hit = TOOL_MARKS[name] || (name.startsWith("mcp__") ? ["bolt", "fleet"] : null);
  return hit ? { mark: hit[0], family: hit[1] } : { mark: "box", family: "" };
}
