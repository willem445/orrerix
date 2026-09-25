/** Parse a user-entered compaction percentage. Zero disables the setting;
 *  every other accepted value is an integer percentage from 1 through 100. */
export function parseCompactionPercent(value: string): number | null {
  const trimmed = value.trim();
  if (!/^\d+$/.test(trimmed)) return null;
  const parsed = Number(trimmed);
  return Number.isSafeInteger(parsed) && parsed <= 100 ? parsed : null;
}

/** Parse the lull duration in minutes. Zero disables nudging. */
export function parseCompactionMinutes(value: string): number | null {
  const trimmed = value.trim();
  if (!/^\d+$/.test(trimmed)) return null;
  const parsed = Number(trimmed);
  return Number.isSafeInteger(parsed) && parsed <= 1440 ? parsed : null;
}
