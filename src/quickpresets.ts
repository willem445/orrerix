// Saved instruction presets for the quick task (#3679) — a pure model plus the
// one store that reads and writes them. DOM-free.
//
// A preset is a NAME and three texts: what the human wants the plan, work and
// review steps told, beyond each role's own instructions. Presets are the
// user's, not a repository's: one file in the app's own state directory
// (`quickpresets.json`, beside `sshprofiles.json`), reusable in every repo, and
// never read by an agent. Nothing secret belongs in one — it is prompt text.
//
// Design note: docs/design/quick-orchestration.md.

/** The three per-step instruction texts, which is all a preset carries. */
export interface QuickInstructions {
  plan: string;
  work: string;
  review: string;
}

export interface QuickPreset extends QuickInstructions {
  name: string;
}

/** The file's schema version. A file naming another is one this build must not
 *  rewrite: the fields it recognises may not mean what it thinks. */
export const QUICK_PRESETS_VERSION = 1;

/** Longest preset name, in characters. */
export const QUICK_PRESET_NAME_MAX = 60;
/** Longest single instruction text a preset keeps. */
export const QUICK_PRESET_TEXT_MAX = 8_000;
/** Most presets one file holds. A cap, so a save past it is refused by name
 *  rather than silently evicting somebody's oldest preset. */
export const QUICK_PRESETS_MAX = 50;

/** What a decoded file holds: the presets, and every top-level field this
 *  build does not know, kept verbatim so a newer build's additions survive a
 *  save made by this one. */
export interface QuickPresetFile {
  presets: QuickPreset[];
  extra: Record<string, unknown>;
}

/** A preset name as it is stored: trimmed, inner whitespace collapsed, capped. */
export function normalizePresetName(name: string): string {
  return name.split(/\s+/).filter(Boolean).join(" ").slice(0, QUICK_PRESET_NAME_MAX);
}

const sameName = (a: string, b: string): boolean =>
  normalizePresetName(a).toLowerCase() === normalizePresetName(b).toLowerCase();

const text = (v: unknown): string => (typeof v === "string" ? v.slice(0, QUICK_PRESET_TEXT_MAX) : "");

/** Decode the store's file.
 *
 *  `null` in — first run, or a corrupt file the backend quarantined — is an
 *  EMPTY store, which is true: there are no presets. A file that parses and is
 *  not this build's shape answers `null` out, which is a different thing:
 *  "this build cannot read it", and a caller must not write over it. Rows that
 *  are not presets are dropped; a duplicate name keeps its first row. */
export function decodeQuickPresets(raw: string | null): QuickPresetFile | null {
  if (raw === null) return { presets: [], extra: {} };
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return null;
  }
  if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) return null;
  const { version, presets, ...extra } = parsed as Record<string, unknown>;
  if (version !== QUICK_PRESETS_VERSION || !Array.isArray(presets)) return null;
  const out: QuickPreset[] = [];
  for (const row of presets) {
    if (typeof row !== "object" || row === null) continue;
    const r = row as Record<string, unknown>;
    const name = normalizePresetName(typeof r.name === "string" ? r.name : "");
    if (!name || out.some((p) => sameName(p.name, name))) continue;
    out.push({ name, plan: text(r.plan), work: text(r.work), review: text(r.review) });
  }
  return { presets: out, extra };
}

/** Encode a file for `save_quick_presets`. */
export function encodeQuickPresets(file: QuickPresetFile): string {
  return JSON.stringify({ ...file.extra, version: QUICK_PRESETS_VERSION, presets: file.presets });
}

/** Whether two sets of instructions are the same — reading EVERY field a
 *  preset has, which is the point: this one predicate decides "does the form
 *  still match the preset it was filled from", and a field it did not read
 *  would be a field whose edits nobody notices. Leading and trailing blank
 *  space does not count as a difference. */
export function sameInstructions(a: QuickInstructions, b: QuickInstructions): boolean {
  return a.plan.trim() === b.plan.trim() && a.work.trim() === b.work.trim() && a.review.trim() === b.review.trim();
}

/** Whether nothing has been typed in any of the three. */
export function instructionsEmpty(a: QuickInstructions): boolean {
  return sameInstructions(a, { plan: "", work: "", review: "" });
}

/** The preset whose three texts are exactly what the form holds, if any — what
 *  the picker shows as selected. */
export function presetMatching(presets: readonly QuickPreset[], texts: QuickInstructions): QuickPreset | null {
  return presets.find((p) => sameInstructions(p, texts)) ?? null;
}

/** A preset by name, case-insensitively. */
export function presetNamed(presets: readonly QuickPreset[], name: string): QuickPreset | null {
  return presets.find((p) => sameName(p.name, name)) ?? null;
}

export type PresetEdit = { ok: true; presets: QuickPreset[] } | { ok: false; error: string };

/** Add `preset`, or replace the one that already has its name. */
export function upsertPreset(presets: readonly QuickPreset[], preset: QuickPreset): PresetEdit {
  const name = normalizePresetName(preset.name);
  if (!name) return { ok: false, error: "A preset needs a name." };
  if (instructionsEmpty(preset)) {
    return { ok: false, error: "There is nothing to save — all three instruction boxes are empty." };
  }
  const taken: QuickPreset = {
    name,
    plan: preset.plan.trim().slice(0, QUICK_PRESET_TEXT_MAX),
    work: preset.work.trim().slice(0, QUICK_PRESET_TEXT_MAX),
    review: preset.review.trim().slice(0, QUICK_PRESET_TEXT_MAX),
  };
  if (presets.some((p) => sameName(p.name, name))) {
    return { ok: true, presets: presets.map((p) => (sameName(p.name, name) ? taken : p)) };
  }
  if (presets.length >= QUICK_PRESETS_MAX) {
    return { ok: false, error: `There are already ${QUICK_PRESETS_MAX} presets — delete one first.` };
  }
  return { ok: true, presets: [...presets, taken] };
}

/** Remove the preset with this name. Removing one that is not there is not an
 *  error: the outcome the human asked for already holds. */
export function removePreset(presets: readonly QuickPreset[], name: string): QuickPreset[] {
  return presets.filter((p) => !sameName(p.name, name));
}

export type PresetWriteOutcome =
  /** The file was read, edited and written. */
  | { outcome: "saved"; presets: QuickPreset[] }
  /** The edit itself was refused — no name, nothing to save, the cap. */
  | { outcome: "refused"; error: string }
  /** The file could not be read, or is not this build's shape, so nothing was
   *  written over it. */
  | { outcome: "unreadable" }
  /** The write failed. */
  | { outcome: "save-failed"; error: string };

/** The one reader and writer of `quickpresets.json`.
 *
 *  **Every write re-reads the file first and edits what it just read**, never a
 *  list held from an earlier paint. The file is one whole blob that several
 *  launcher forms — and several windows — can each hold a copy of, so a save
 *  built from a held copy would erase whatever another one saved since. And a
 *  read that FAILED declines the write rather than treating the store as
 *  empty: "I could not look" is not "there was nothing there". The failure is
 *  not latched — the next gesture reads again. */
export class QuickPresetsStore {
  private readonly io: { load(): Promise<string | null>; save(contents: string): Promise<void> };

  constructor(io: { load(): Promise<string | null>; save(contents: string): Promise<void> }) {
    this.io = io;
  }

  /** The presets on disk, or `null` when they could not be read. */
  async read(): Promise<QuickPreset[] | null> {
    return (await this.readFile())?.presets ?? null;
  }

  private async readFile(): Promise<QuickPresetFile | null> {
    try {
      return decodeQuickPresets(await this.io.load());
    } catch {
      return null;
    }
  }

  private async write(edit: (presets: QuickPreset[]) => PresetEdit): Promise<PresetWriteOutcome> {
    const file = await this.readFile();
    if (!file) return { outcome: "unreadable" };
    const edited = edit(file.presets);
    if (!edited.ok) return { outcome: "refused", error: edited.error };
    try {
      await this.io.save(encodeQuickPresets({ presets: edited.presets, extra: file.extra }));
    } catch (err) {
      return { outcome: "save-failed", error: String(err) };
    }
    return { outcome: "saved", presets: edited.presets };
  }

  /** Save `preset`, replacing the one that has its name. */
  save(preset: QuickPreset): Promise<PresetWriteOutcome> {
    return this.write((presets) => upsertPreset(presets, preset));
  }

  /** Delete the preset with this name. */
  remove(name: string): Promise<PresetWriteOutcome> {
    return this.write((presets) => ({ ok: true, presets: removePreset(presets, name) }));
  }
}
