/* What the AI said about a moment, in the rail.

   Two files answer here — `userdata/result_ai_period_summary/<day>.json` per stretch, and the day's own
   paragraph — and they have two producers: this machine's `windai` and an outside AI writing over the
   bridge. Nothing in this window can tell which of the two it is reading, so the door hands the name in
   `writtenBy` and this file prints it beside the paragraph it belongs to, rather than in a legend the
   reader has to hold in mind.

   The one thing a summary must never do is read as an absence. "Nothing was written about this minute",
   "the paragraph no longer stands against the index", "this install holds no such stretch so nobody can
   check it" and "the ask itself failed" are four different facts and get four different sentences. */

import { useEffect, useState } from "react";
import { errorMessage, summaryForKey } from "../services/api";
import type { PeriodState, PeriodSummaryDto, RowKey } from "../types";
import { Pill, cn, type Tone } from "./ui";

type Tr = (key: string, args?: Record<string, string | number>) => string;

/** How each state the door can report is put on screen.
 *
 *  `content_changed` and `prompt_changed` are stale: the words are still there, and what they were written
 *  from is not. Showing either as current would be the window claiming coverage it cannot check. */
const PERIOD_STATES: Record<PeriodState, { key: string; tone: Tone }> = {
  current: { key: "rail.ai_summary.state.current", tone: "emerald" },
  content_changed: { key: "rail.ai_summary.state.content_changed", tone: "amber" },
  prompt_changed: { key: "rail.ai_summary.state.prompt_changed", tone: "amber" },
  unindexed: { key: "rail.ai_summary.state.unindexed", tone: "rose" },
  unknown: { key: "rail.ai_summary.state.unknown", tone: "slate" },
};

export function stateBadge(state: string, tr: Tr): { label: string; tone: Tone } {
  const known = PERIOD_STATES[state as PeriodState];
  // A state this file has never met is printed as what the door said, not folded into "current".
  return known ? { label: tr(known.key), tone: known.tone } : { label: tr("rail.ai_summary.state.other", { state }), tone: "slate" };
}

/** The payload of one stretch's paragraph, as the rail's row list: which stretch, what window, when it
 *  was written, who wrote it, and whether it still stands. */
export function summaryRows(period: PeriodSummaryDto, tr: Tr): [string, string][] {
  return [
    [tr("rail.ai_summary.segment"), period.segment],
    [tr("rail.ai_summary.window"), period.span],
    [tr("rail.ai_summary.frames"), String(period.frames)],
    [tr("rail.ai_summary.chars"), String(period.textChars)],
    [tr("rail.ai_summary.written_at"), period.writtenAt],
    [tr("rail.ai_summary.written_by"), period.writtenBy ?? tr("rail.ai_summary.written_by_unknown")],
    [tr("rail.ai_summary.day"), period.day],
    [tr("rail.ai_summary.state"), stateBadge(period.state, tr).label],
  ];
}

/** One paragraph and the facts that license it.
 *
 *  `onPick` turns the whole entry into the door the day screen's list uses; without it the entry is the
 *  reading state, with the text in full. `clamp` is only ever set on the list, where the rail is what
 *  opens the whole thing. */
export function PeriodSummaryEntry({
  period,
  tr,
  onPick,
  clamp,
}: {
  period: PeriodSummaryDto;
  tr: Tr;
  onPick?: () => void;
  clamp?: boolean;
}) {
  const badge = stateBadge(period.state, tr);
  const head = (
    <>
      <span className="flex min-w-0 flex-1 items-baseline gap-2">
        <span className="truncate text-xs font-medium tabular-nums text-slate-200">{period.span}</span>
        <span className="shrink-0 text-[10px] tabular-nums text-slate-500">{tr("rail.ai_summary.frames_count", { count: period.frames })}</span>
      </span>
      <Pill tone={badge.tone} className="shrink-0">
        {badge.label}
      </Pill>
    </>
  );

  const inner = (
    <>
      <div className="flex items-center justify-between gap-3">{head}</div>
      <p className="selectable truncate text-[10px] tabular-nums text-slate-500" title={`${period.writtenBy ?? tr("rail.ai_summary.written_by_unknown")} · ${period.writtenAt}`}>
        {period.writtenBy ?? tr("rail.ai_summary.written_by_unknown")} · {period.writtenAt} · {tr("rail.ai_summary.day_value", { date: period.day })}
      </p>
      <p className={cn("selectable text-xs leading-relaxed text-slate-300", clamp && "line-clamp-2")}>{period.text}</p>
    </>
  );

  if (!onPick) {
    return <div className="space-y-1.5 rounded-xl border border-slate-800/80 bg-slate-900/50 p-2.5">{inner}</div>;
  }
  return (
    <button
      onClick={onPick}
      className="block w-full space-y-1.5 rounded-xl border border-slate-800/80 bg-slate-900/50 p-2.5 text-left transition-colors hover:border-indigo-500/60"
    >
      {inner}
    </button>
  );
}

/**
 * The rail's own AI section: what the stored paragraphs say about this row's minute.
 *
 * Asked by key, never by a path the window assembled — the index decides which stretch a row sits in, and
 * the door looks one day back as well, because a stretch that began at 02:50 yesterday runs into the
 * minute being asked about. An empty answer is rendered as an empty answer, and a refused one as its own
 * sentence; the two do not share a line.
 */
export function AiSummarySection({ row, tr }: { row: { key: RowKey; time: number }; tr: Tr }) {
  const [read, setRead] = useState<{ busy: true } | { busy: false; entries: PeriodSummaryDto[]; failed: string | null }>({ busy: true });
  const { file, rowid } = row.key;
  const at = row.time;

  useEffect(() => {
    let alive = true;
    setRead({ busy: true });
    summaryForKey(rowid, file, at)
      .then((entries) => {
        if (alive) setRead({ busy: false, entries, failed: null });
      })
      .catch((caught) => {
        // A failure to ask is not "nothing was written", and the rail must not blur the two.
        if (alive) setRead({ busy: false, entries: [], failed: errorMessage(caught) });
      });
    return () => {
      alive = false;
    };
  }, [file, rowid, at]);

  return (
    <div className="space-y-2">
      <p className="text-[11px] uppercase tracking-wide text-slate-500">{tr("rail.ai_summary.title")}</p>
      {read.busy ? (
        <p className="text-[11px] text-slate-500">{tr("windui_web_loading")}…</p>
      ) : !read.failed ? (
        read.entries.length === 0 ? (
          <p className="max-w-prose text-[11px] leading-relaxed text-slate-500">{tr("rail.ai_summary.none")}</p>
        ) : (
          <ul className="space-y-2">
            {read.entries.map((one) => (
              <li key={`${one.day}-${one.segment}-${one.start}`}>
                <PeriodSummaryEntry period={one} tr={tr} />
              </li>
            ))}
          </ul>
        )
      ) : (
        <p className="selectable max-w-prose text-[11px] leading-relaxed text-rose-200/80">
          {tr("rail.ai_summary.failed")} {read.failed}
        </p>
      )}
    </div>
  );
}
