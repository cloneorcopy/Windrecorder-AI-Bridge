/* How this window words a *length* of time.
 *
 * The day screen's "where did the time go" panel counts the seconds each window held the screen, and it
 * used to print `Math.round(seconds / 60)` with an "m" on the end. On an afternoon of short bursts every
 * one of the ten rows read `0m` — which is indistinguishable, on screen, from a panel that failed to fill
 * in. The egui window has always said `0:00:07` for the same number, through
 * `wind_base::clock::seconds_to_hhmmss`; this is that precision in the fewest characters a column can
 * carry, because these two windows are allowed to look different and are not allowed to disagree.
 *
 * Note what is *not* here: no wall clock. `videofile_time` is naive-local seconds, so any
 * `new Date(seconds * 1000)` in this window reads them as an instant and then adds the machine's offset on
 * top — the strip and the lightbox did exactly that, and labelled a 21:57 picture 05:57 the next morning
 * while the very same row's result card said 21:57. Those strings arrive formatted from the side that read
 * the row: `StripCell.clock`, `LightboxTile.stamp`, `RowCard.clock`, `FlagNote.when`.
 */

/** `h:mm:ss`, `m:ss` under an hour, `s` under a minute — never a rounded `0m` for a row that happened. */
export function duration(seconds: number): string {
  const total = Math.max(0, Math.round(seconds));
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = total % 60;
  if (h > 0) return `${h}:${String(m).padStart(2, "0")}:${String(s).padStart(2, "0")}`;
  if (m > 0) return `${m}:${String(s).padStart(2, "0")}`;
  return `${s}s`;
}
