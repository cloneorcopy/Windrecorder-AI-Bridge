import { useCallback, useEffect, useMemo, useState } from "react";
import { CalendarDays, ChevronLeft, ChevronRight } from "lucide-react";
import { daySummaries, errorMessage, loadDay } from "../services/api";
import type { DayOutcome, DaySummariesDto, PeriodSummaryDto, RowCard, RowKey, StripCell } from "../types";
import { Button, Empty, ErrorBanner, Panel, Pill, SectionTitle, cn } from "../components/ui";
import { FrameViewer, ZoomHint, frameSubject, type FrameSubject } from "../components/FrameViewer";
import { DetailPanel, PanelRowList } from "../components/DetailPanel";
import { PeriodSummaryEntry, summaryRows } from "../components/AiSummary";
import { FrameRail, cardSubject, locateRow, type Say } from "../components/FrameRail";
import { duration } from "../clock";

type Tr = (key: string, args?: Record<string, string | number>) => string;

function shift(date: string, days: number) {
  const [y, m, d] = date.split("-").map(Number);
  const base = new Date(y ?? 1970, (m ?? 1) - 1, d ?? 1);
  base.setDate(base.getDate() + days);
  return `${base.getFullYear()}-${String(base.getMonth() + 1).padStart(2, "0")}-${String(base.getDate()).padStart(2, "0")}`;
}

/** The strip's own words for one cell: when it happened, and in what window if the day's rows say. The
 *  cell's stored preview travels with it, because that is what the viewer paints while the frame is read. */
function stripSubject(key: RowKey, cell: StripCell, card: RowCard | undefined, tr: Tr, date: string): FrameSubject {
  return frameSubject(key, `${date} ${cell.clock ?? "—"} · ${card?.title ?? tr("windui_web_title_none")}`, cell.thumbnail);
}

/** Where the day's three coverage numbers came from, in words, because which of the two it is changes what
 *  the numbers are allowed to mean: the day's own paragraph, a count taken from the index just now, or
 *  neither — in which case the zeroes on screen are not a count of anything. */
const COVERAGE_KEYS: Record<DaySummariesDto["coverageFrom"], string> = {
  stored: "day.summary.coverage.stored",
  index: "day.summary.coverage.index",
  unknown: "day.summary.coverage.unknown",
};

/** One day's paragraph and what it claims about the day it covers.
 *
 *  Rows are added only for facts the door actually answered: no paragraph yet gets no `written` line, and
 *  a coverage count this install could not take is reported as that instead of as `0 / 0`. */
function dailyRows(day: DaySummariesDto, tr: Tr): [string, string][] {
  const rows: [string, string][] = [];
  if (day.daily.exists) {
    rows.push([tr("day.summary.written_at"), day.daily.writtenAt ?? tr("day.summary.written_at_missing")]);
    rows.push([tr("day.summary.written_by"), day.daily.writtenBy ?? tr("rail.ai_summary.written_by_unknown")]);
  }
  if (day.coverageFrom !== "unknown") {
    rows.push([tr("day.summary.coverage"), `${day.daily.segmentsSummarised} / ${day.daily.segmentsTotal}`]);
  }
  rows.push([tr("day.summary.coverage_from"), tr(COVERAGE_KEYS[day.coverageFrom])]);
  if (day.daily.missing.length > 0) {
    rows.push([tr("day.summary.gaps"), String(day.daily.missing.length)]);
  }
  return rows;
}

export function OneDayScreen({ tr, say }: { tr: Tr; say: Say }) {
  const [date, setDate] = useState(() => {
    const now = new Date();
    return `${now.getFullYear()}-${String(now.getMonth() + 1).padStart(2, "0")}-${String(now.getDate()).padStart(2, "0")}`;
  });
  const [outcome, setOutcome] = useState<DayOutcome | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [picked, setPicked] = useState<RowCard | null>(null);
  /** Which strip cell is being looked at in the whole-window viewer, by position in `doors`. */
  const [framed, setFramed] = useState<number | null>(null);

  /* The day's own AI summaries. Asked for beside the day's rows, never through them: a summary folder that
     will not open is that panel's failure, and the strip and the density bars keep answering. */
  const [summaries, setSummaries] = useState<DaySummariesDto | null>(null);
  const [summariesBusy, setSummariesBusy] = useState(true);
  const [summariesFailed, setSummariesFailed] = useState<string | null>(null);
  /** Only read when the day named has no paragraph of its own, so the newest one there is can be printed
   *  under the date it was written for rather than under the date being asked about. */
  const [fallback, setFallback] = useState<DaySummariesDto | null>(null);
  const [fallbackFailed, setFallbackFailed] = useState<string | null>(null);
  /** The rail: one stored paragraph, and the row the index holds for its moment when this day holds one. */
  const [rail, setRail] = useState<{ period: PeriodSummaryDto; moment: RowCard | null } | null>(null);
  /** The whole-window viewer opened out of the rail, on a row the day's strip does not sample. */
  const [viewer, setViewer] = useState<{ subject: FrameSubject; playing: boolean } | null>(null);

  const loadSummaries = useCallback(async (which: string) => {
    setSummariesBusy(true);
    setSummariesFailed(null);
    try {
      const [y, m, d] = which.split("-").map(Number);
      setSummaries(await daySummaries(y ?? 1970, m ?? 1, d ?? 1));
    } catch (caught) {
      setSummariesFailed(errorMessage(caught));
      setSummaries(null);
    } finally {
      setSummariesBusy(false);
    }
  }, []);

  useEffect(() => {
    void loadSummaries(date);
    // A rail left open over the previous day's paragraph would keep naming a moment the page no longer
    // shows, and the viewer with it.
    setRail(null);
    setViewer(null);
  }, [date, loadSummaries]);

  const fallbackDate = summaries && !summaries.daily.exists ? summaries.fallbackDate : null;
  useEffect(() => {
    if (!fallbackDate) {
      setFallback(null);
      setFallbackFailed(null);
      return;
    }
    let alive = true;
    const [y, m, d] = fallbackDate.split("-").map(Number);
    daySummaries(y ?? 0, m ?? 0, d ?? 0)
      .then((dto) => {
        if (!alive) return;
        setFallback(dto);
        setFallbackFailed(null);
      })
      .catch((caught) => {
        // Refused is not the same as absent, and the panel says which.
        if (!alive) return;
        setFallback(null);
        setFallbackFailed(errorMessage(caught));
      });
    return () => {
      alive = false;
    };
  }, [fallbackDate]);

  /** Open the rail on the moment a paragraph was written about.
   *
   *  The paragraph names a window, not a row, so the row is the index's: the earliest of this day's rows
   *  that falls inside it. When the day holds none — a stretch whose rows have aged out, a window the
   *  index never carried — the rail still opens, on the paragraph alone, and says there is no frame. */
  const openPeriod = (period: PeriodSummaryDto) => {
    const inside = (outcome?.cards ?? []).filter((card) => card.time >= period.start && card.time <= period.end);
    const moment = inside.length > 0 ? inside.reduce((earliest, one) => (one.time < earliest.time ? one : earliest)) : null;
    setRail({ period, moment });
  };

  const load = useCallback(async (which: string) => {
    setBusy(true);
    setError(null);
    try {
      const [y, m, d] = which.split("-").map(Number);
      setOutcome(await loadDay(y ?? 1970, m ?? 1, d ?? 1));
    } catch (caught) {
      setError(errorMessage(caught));
      setOutcome(null);
    } finally {
      setBusy(false);
    }
  }, []);

  useEffect(() => {
    void load(date);
  }, [date, load]);

  const busiest = useMemo(() => [...(outcome?.titles ?? [])].sort((a, b) => b[1] - a[1]).slice(0, 10), [outcome]);
  const maxBucket = Math.max(1, ...(outcome?.buckets.map((one) => one.count) ?? [1]));

  /** The hour axis, in the order the day runs. `label` arrives already phrased as `HH:MM` from the
   *  store's own bucket builder — instants are phrased by the side that read the row — so the hour is
   *  simply its first two characters, and consecutive buckets that share an hour collapse into one
   *  label cell weighted by that hour's column count. */
  const hours = useMemo(() => {
    const runs: { hour: string; weight: number }[] = [];
    for (const bucket of outcome?.buckets ?? []) {
      const hour = bucket.label.slice(0, 2);
      const last = runs[runs.length - 1];
      if (last && last.hour === hour) {
        last.weight += 1;
      } else {
        runs.push({ hour, weight: 1 });
      }
    }
    return runs;
  }, [outcome]);

  /** The strip's cells that stand for a row, in strip order, and where each one sits in that list.
   *
   *  The viewer walks this, so the day's frames step in the order the day was sampled. A cell that samples
   *  no row is in neither the list nor the map: it has nothing to open, and the grid says so by being
   *  disabled rather than by answering with its neighbour's picture.
   */
  const doors = useMemo(() => {
    const list: FrameSubject[] = [];
    const at = new Map<number, number>();
    (outcome?.strip ?? []).forEach((cell, index) => {
      const key = cell.key;
      if (!key) return;
      const card = outcome?.cards.find((one) => one.key.rowid === key.rowid && one.key.file === key.file);
      list.push(stripSubject(key, cell, card, tr, date));
      at.set(index, list.length - 1);
    });
    return { list, at };
  }, [outcome, date, tr]);

  return (
    <div className="space-y-5">
      <Panel className="flex flex-wrap items-center justify-between gap-4 p-4">
        <SectionTitle title={tr("windui_web_oneday_title")} detail={tr("windui_web_oneday_detail")} />
        <div className="flex items-center gap-1.5">
          <Button variant="ghost" onClick={() => setDate(shift(date, -1))}>
            <ChevronLeft className="h-3.5 w-3.5" />
          </Button>
          <input
            type="date"
            value={date}
            onChange={(event) => setDate(event.target.value)}
            className="rounded-xl border border-slate-700/70 bg-slate-950/60 px-3 py-2 text-sm text-slate-100 outline-none focus:border-indigo-500/60"
          />
          <Button variant="ghost" onClick={() => setDate(shift(date, 1))}>
            <ChevronRight className="h-3.5 w-3.5" />
          </Button>
        </div>
      </Panel>

      {error ? <ErrorBanner message={error} onRetry={() => void load(date)} tr={tr} /> : null}

      {outcome ? (
        <>
          <div className="flex flex-wrap gap-2">
            <Pill tone="indigo">
              <CalendarDays className="h-3 w-3" />
              {tr("windui_web_rows", { count: outcome.cards.length })}
            </Pill>
            <Pill tone="purple">{tr("windui_web_active_hours")} {outcome.activeHours.toFixed(2)}</Pill>
            {outcome.unindexedVideo ? <Pill tone="amber">{tr("windui_web_unindexed")}</Pill> : null}
            {outcome.warnings.map((one) => (
              <Pill key={one} tone="rose">
                {one}
              </Pill>
            ))}
          </div>

          <Panel className="p-4">
            <p className="mb-3 text-[11px] uppercase tracking-wide text-slate-500">{tr("windui_web_strip")}</p>
            <div className="flex gap-1 overflow-x-auto pb-2">
              {outcome.strip.map((cell, index) => {
                const card = cell.key
                  ? outcome.cards.find((one) => one.key.rowid === cell.key?.rowid && one.key.file === cell.key?.file)
                  : undefined;
                const position = doors.at.get(index);
                return (
                  <button
                    key={`${cell.from}-${index}`}
                    // The picture is a door here exactly as it is on a result card: one click asks the index
                    // for the frame this row was made of. A cell that samples no row has nothing to open, so
                    // it is disabled rather than answering with the wrong picture.
                    onClick={() => {
                      if (position === undefined) return;
                      setPicked(card ?? null);
                      setFramed(position);
                    }}
                    disabled={position === undefined}
                    className={cn(
                      "group relative h-20 shrink-0 overflow-hidden rounded-md border transition-all",
                      position === undefined ? "cursor-default opacity-60" : "cursor-zoom-in",
                      picked?.key.rowid === cell.key?.rowid ? "border-indigo-500/70" : "border-slate-800 hover:border-slate-600",
                    )}
                    // The clock the index stored, spelled by the side that reads it. This used to format
                    // `cell.time` here, which turned the recorder's naive-local seconds into an instant and
                    // then added this machine's offset to it: every cell in the strip was labelled eight
                    // hours away from the same row's own card.
                    title={cell.clock ?? "—"}
                  >
                    {cell.thumbnail ? (
                      <img src={`data:image/jpeg;base64,${cell.thumbnail}`} alt="" loading="lazy" className="h-full w-28 object-cover" />
                    ) : (
                      <span className="block h-full w-28 bg-slate-800/50" />
                    )}
                    {position !== undefined ? <ZoomHint /> : null}
                  </button>
                );
              })}
            </div>
          </Panel>

          <div className="grid gap-5 lg:grid-cols-[1.4fr_1fr]">
            <Panel className="p-4">
              <p className="mb-3 text-[11px] uppercase tracking-wide text-slate-500">{tr("windui_web_day_activity")}</p>
              {/* The row has a definite height and every column is stretched to it, because a
                  percentage-height bar inside an auto-height box is treated as `auto` by CSS and the
                  whole chart painted nothing: 240 columns measured 0 px tall in Chrome. The bars also
                  carry no gap now — `gap-1` is 4 px, so 240 of them overflowed a 700 px panel by
                  266 px and squeezed each column to 0 px wide. */}
              <div className="flex h-28">
                {outcome.buckets.map((bucket) => (
                  <div
                    key={bucket.start}
                    className="group relative h-full min-w-0 flex-1 rounded-t bg-slate-800/40"
                    title={`${bucket.label} · ${bucket.count}`}
                  >
                    <div
                      className="absolute bottom-0 left-0 w-full rounded-t bg-gradient-to-t from-indigo-600/60 to-purple-500/70 transition-all duration-300 group-hover:from-indigo-500 group-hover:to-pink-500"
                      style={{ height: `${(bucket.count / maxBucket) * 100}%` }}
                    />
                  </div>
                ))}
              </div>
              {/* One label per hour, weighted by how many buckets that hour actually holds, so the
                  label boundaries fall exactly on the column boundaries the row above drew — both sides
                  divide the same width by the same 240 units. The day can start at an odd minute, and a
                  short first hour then gets a short label cell rather than a wrong one. */}
              <div className="mt-1 flex">
                {hours.map((hour, position) => (
                  <div
                    key={`${hour.hour}-${position}`}
                    className="min-w-0 truncate text-center text-[9px] text-slate-600"
                    style={{ flex: `${hour.weight} 1 0%` }}
                  >
                    {hour.hour}
                  </div>
                ))}
              </div>
            </Panel>

            <Panel className="p-4">
              <p className="mb-3 text-[11px] uppercase tracking-wide text-slate-500">{tr("windui_web_titles")}</p>
              {busiest.length === 0 ? (
                <p className="text-xs text-slate-600">—</p>
              ) : (
                <ul className="space-y-1.5">
                  {busiest.map(([title, seconds]) => (
                    <li key={title} className="flex items-baseline gap-2 text-xs">
                      <span className="min-w-0 flex-1 truncate text-slate-300" title={title}>
                        {title}
                      </span>
                      <span className="shrink-0 tabular-nums text-slate-500">{duration(seconds)}</span>
                    </li>
                  ))}
                </ul>
              )}
            </Panel>
          </div>

          {/* What the AI said about this day — a panel of its own, beside the two-column row above and never
              inside it. That row's hour axis and its bar columns divide the same width by the same 240
              buckets, and anything inserted between them is a third column in a two-number division; the
              layout was fixed on 2026-09-27 for exactly that reason. */}
          <Panel className="space-y-3 p-4">
            <div className="flex flex-wrap items-center justify-between gap-2">
              <p className="text-[11px] uppercase tracking-wide text-slate-500">{tr("day.summary.title")}</p>
              {summaries ? <Pill tone="slate">{summaries.date}</Pill> : null}
            </div>

            {summariesBusy ? <p className="text-xs text-slate-500">{tr("windui_web_loading")}…</p> : null}

            {!summariesBusy && summariesFailed ? (
              <p className="selectable text-xs leading-relaxed text-rose-200/80">
                {tr("day.summary.failed")} {summariesFailed}
              </p>
            ) : null}

            {!summariesBusy && !summariesFailed && summaries ? (
              <>
                {summaries.daily.exists ? (
                  summaries.daily.readable ? (
                    summaries.daily.text.length > 0 ? (
                      <p className="selectable text-sm leading-relaxed text-slate-200">{summaries.daily.text}</p>
                    ) : (
                      <p className="text-xs leading-relaxed text-slate-500">{tr("day.summary.blank", { date: summaries.date })}</p>
                    )
                  ) : (
                    <p className="selectable text-xs leading-relaxed text-rose-200/80">
                      {summaries.daily.note ?? tr("day.summary.unreadable", { date: summaries.date })}
                    </p>
                  )
                ) : (
                  <p className="text-xs leading-relaxed text-slate-500">{tr("day.summary.none", { date: summaries.date })}</p>
                )}

                {summaries.daily.exists && summaries.daily.readable && (summaries.daily.partial || summaries.daily.stale) ? (
                  <div className="flex flex-wrap gap-2">
                    {summaries.daily.partial ? <Pill tone="amber">{tr("day.summary.partial")}</Pill> : null}
                    {summaries.daily.stale ? <Pill tone="rose">{tr("day.summary.stale")}</Pill> : null}
                  </div>
                ) : null}

                <PanelRowList rows={dailyRows(summaries, tr)} />

                {summaries.notes.length > 0 ? (
                  <ul className="space-y-1">
                    {summaries.notes.map((note) => (
                      <li key={note} className="selectable text-[11px] leading-relaxed text-amber-200/80">
                        {note}
                      </li>
                    ))}
                  </ul>
                ) : null}

                {summaries.daily.missing.length > 0 ? (
                  <p className="selectable break-words text-[11px] leading-relaxed text-slate-500">
                    {tr("day.summary.gaps_list")}{" "}
                    {summaries.daily.missing.slice(0, 12).join(" · ")}
                    {summaries.daily.missing.length > 12 ? ` · +${summaries.daily.missing.length - 12}` : ""}
                  </p>
                ) : null}

                {/* The newest paragraph there is, when this day has none of its own — named by the date it
                    was written for, because yesterday's summary printed under today's is the one mistake
                    this panel is not allowed to make. */}
                {summaries.fallbackDate && !summaries.daily.exists ? (
                  <div className="space-y-2 rounded-xl border border-amber-500/25 bg-amber-500/[0.06] p-3">
                    <p className="text-[11px] leading-relaxed text-amber-200/90">
                      {tr("day.summary.fallback", { date: summaries.fallbackDate, asked: summaries.date })}
                    </p>
                    {fallbackFailed ? (
                      <p className="selectable text-[11px] leading-relaxed text-rose-200/80">{tr("day.summary.failed")} {fallbackFailed}</p>
                    ) : fallback ? (
                      <>
                        {fallback.daily.text.length > 0 ? (
                          <p className="selectable text-sm leading-relaxed text-slate-200">{fallback.daily.text}</p>
                        ) : (
                          <p className="text-xs leading-relaxed text-slate-500">{tr("day.summary.blank", { date: fallback.date })}</p>
                        )}
                        <PanelRowList rows={dailyRows(fallback, tr)} />
                      </>
                    ) : (
                      <p className="text-[11px] leading-relaxed text-slate-500">{tr("day.summary.fallback_missing", { date: summaries.fallbackDate })}</p>
                    )}
                  </div>
                ) : null}
              </>
            ) : null}

            {summaries && !summariesBusy && !summariesFailed ? (
              <div className="space-y-1.5">
                <p className="text-[11px] uppercase tracking-wide text-slate-500">
                  {tr("day.summary.periods")} · {summaries.periods.length}
                </p>
                {summaries.periods.length === 0 ? (
                  <p className="max-w-prose text-xs leading-relaxed text-slate-600">{tr("day.summary.periods_none", { date: summaries.date })}</p>
                ) : (
                  <ul className="grid gap-2 sm:grid-cols-2">
                    {summaries.periods.map((period) => (
                      <li key={`${period.segment}-${period.start}`}>
                        <PeriodSummaryEntry period={period} tr={tr} clamp onPick={() => openPeriod(period)} />
                      </li>
                    ))}
                  </ul>
                )}
              </div>
            ) : null}
          </Panel>

          <Panel className="p-4">
            <p className="mb-3 text-[11px] uppercase tracking-wide text-slate-500">
              {tr("windui_web_flags")} · {outcome.flags.length}
            </p>
            {outcome.flags.length === 0 ? (
              // A dash here reads as "this panel failed to fill", which is a different claim from the
              // truth: marks are written by the tray's flag command into `userdata\flag_mark_note.csv`,
              // and a person who has never used it has an empty panel, not a broken one. So the panel
              // says where its rows come from, in the catalog's own sentence.
              <p className="max-w-prose text-xs leading-relaxed text-slate-600">{tr("oneday_text_flag_mark_help")}</p>
            ) : (
              <ul className="grid gap-2 sm:grid-cols-2 lg:grid-cols-3">
                {outcome.flags.map((flag) => (
                  <li key={`${flag.when}-${flag.index}`} className="flex items-center gap-2.5 rounded-xl border border-slate-800/80 bg-slate-900/50 p-2">
                    <span className="text-[10px] tabular-nums text-slate-500">{flag.when.slice(11)}</span>
                    <span className="min-w-0 flex-1 truncate text-xs text-slate-300">{flag.note}</span>
                    {flag.hasThumbnail ? <span className="h-1.5 w-1.5 shrink-0 rounded-full bg-indigo-400/70" /> : null}
                  </li>
                ))}
              </ul>
            )}
          </Panel>

          {framed !== null && doors.list[framed] ? (
            <FrameViewer
              subject={doors.list[framed]}
              tr={tr}
              onClose={() => setFramed(null)}
              list={doors.list}
              index={framed}
              onIndex={setFramed}
            />
          ) : null}

          {/* The rail a stored paragraph opens. It is the search screen's column, drawn from the same
              component: same slide in, same backdrop, same X, same Escape, and the same weight — a summary
              is a row like any other and gets the same room to be read in.

              When this day's index holds a row inside the paragraph's window, the rail opens on that moment,
              picture and all; when it holds none, the rail says so and shows the paragraph by itself rather
              than drawing a box for a frame that is not there. */}
          {rail ? (
            rail.moment ? (
              <FrameRail
                card={rail.moment}
                terms={[]}
                tr={tr}
                pauseEscape={framed !== null || viewer !== null}
                onClose={() => setRail(null)}
                onLocate={(card) => locateRow(card, tr, say)}
                onPlay={(card) => setViewer({ subject: cardSubject(card, tr), playing: true })}
                onOpenOriginal={(card) => setViewer({ subject: cardSubject(card, tr), playing: false })}
              />
            ) : (
              <DetailPanel
                open
                onClose={() => setRail(null)}
                pauseEscape={framed !== null || viewer !== null}
                title={rail.period.span}
                subtitle={rail.period.day}
                rows={summaryRows(rail.period, tr)}
                body={rail.period.text}
                bodyLabel={tr("rail.ai_summary.title")}
                footer={<p className="text-[11px] leading-relaxed text-amber-300/70">{tr("rail.ai_summary.no_frame")}</p>}
              />
            )
          ) : null}

          {viewer ? (
            <FrameViewer
              subject={viewer.subject}
              tr={tr}
              startPlaying={viewer.playing}
              onClose={() => setViewer(null)}
            />
          ) : null}
        </>
      ) : busy ? (
        <Panel className="px-6 py-14 text-center text-xs text-slate-500">{tr("windui_web_loading")}…</Panel>
      ) : null}

      {outcome && outcome.cards.length === 0 && !busy ? (
        <Empty title={tr("windui_web_no_rows_title")} detail={tr("windui_web_no_rows_detail")} />
      ) : null}
    </div>
  );
}
