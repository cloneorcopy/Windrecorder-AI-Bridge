import { useCallback, useEffect, useMemo, useState } from "react";
import { ChevronLeft, ChevronRight, ExternalLink, FileSearch } from "lucide-react";
import { errorMessage, search } from "../services/api";
import type { RowCard, SearchOutcome, SearchParams } from "../types";
import { Button, Empty, ErrorBanner, Field, Panel, Pill, SectionTitle, cn, inputClass } from "../components/ui";
import { FrameViewer, Thumb, ZoomHint } from "../components/FrameViewer";
import { marked } from "../components/DetailPanel";
import { FrameRail, cardSubject, locateRow, type Say } from "../components/FrameRail";

type Tr = (key: string, args?: Record<string, string | number>) => string;

/** Today as the recorder stores it: a naive local date, not an instant. */
function today(): { y: number; m: number; d: number } {
  const now = new Date();
  return { y: now.getFullYear(), m: now.getMonth() + 1, d: now.getDate() };
}

function iso(y: number, m: number, d: number) {
  return `${y}-${String(m).padStart(2, "0")}-${String(d).padStart(2, "0")}`;
}

function stamp(date: string): SearchParams["from"] {
  const [y, m, d] = date.split("-").map(Number);
  return { year: y ?? 1970, month: m ?? 1, day: d ?? 1, hour: 0, minute: 0, second: 0 };
}

const DEFAULTS = () => {
  const { y, m, d } = today();
  return { keywords: "", exclude: "", from: iso(y, m, d), to: iso(y, m, d), pageSize: 20 };
};

export function SearchScreen({ tr, say }: { tr: Tr; say: Say }) {
  /* Every writer below is a functional update — `setForm((now) => ...)` rather than a spread of the `form`
   * this render closed over. A person cannot tell the difference: two inputs, two clicks, two renders. A
   * script can, and the verification harness drives both date boxes in one tick, where the older spelling
   * quietly dropped the first box and the gate searched the wrong week while reporting the day it was asked
   * for. Any future door that fills the form from outside — a deep link, a "same search, yesterday" — hits
   * the same hole, so the safer writer is the one that ships. */
  const [form, setForm] = useState(DEFAULTS);
  const [page, setPage] = useState(1);
  const [outcome, setOutcome] = useState<SearchOutcome | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [selected, setSelected] = useState<RowCard | null>(null);
  /** The card whose original frame is being looked at, in the whole-window viewer. */
  const [framed, setFramed] = useState<RowCard | null>(null);
  /** Whether that viewer opened on the still or on the segment moving. Its own state rather than a field of
   *  `framed`, because the viewer's walk replaces `framed` on every step and must not drop the mode the user
   *  chose while it does. */
  const [framedPlays, setFramedPlays] = useState(false);
  const run = useCallback(
    async (which: number) => {
      setBusy(true);
      setError(null);
      try {
        const params: SearchParams = {
          keywords: form.keywords,
          exclude: form.exclude,
          from: stamp(form.from),
          to: stamp(form.to),
          page: which,
          pageSize: form.pageSize,
        };
        setOutcome(await search(params));
      } catch (caught) {
        setError(errorMessage(caught));
        setOutcome(null);
      } finally {
        setBusy(false);
      }
    },
    [form],
  );

  useEffect(() => {
    void run(1);
    // The opening query is the window's first impression; re-running it on every keystroke is not the
    // same thing, so this depends on `run` only through the mount, and every later search is a click.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const terms = outcome?.terms ?? [];
  const total = outcome?.total ?? 0;
  const pages = outcome?.pages ?? 0;

  /** The page's rows, as viewer subjects: `←`/`→` walk the list the user was already looking at, so the
   *  viewer is never a dead end that has to be closed and reopened one row at a time. */
  const subjects = useMemo(() => (outcome?.cards ?? []).map((card) => cardSubject(card, tr)), [outcome, tr]);
  const framedAt = framed
    ? subjects.findIndex((one) => one.key.file === framed.key.file && one.key.rowid === framed.key.rowid)
    : -1;

  const open = useCallback((card: RowCard) => locateRow(card, tr, say), [say, tr]);

  return (
    <div className="space-y-5">
      <Panel className="p-4 sm:p-5">
        <SectionTitle
          title={tr("windui_web_search_title")}
          detail={tr("windui_web_search_detail")}
          right={
            outcome ? (
              <Pill tone={total > 0 ? "indigo" : "slate"}>
                <FileSearch className="h-3 w-3" />
                {tr("windui_web_results", { count: outcome.cards.length, total })}
                <span className="text-slate-500">· {tr("windui_web_elapsed_ms", { ms: outcome.elapsedMs })}</span>
              </Pill>
            ) : null
          }
        />
        <div className="mt-4 grid grid-cols-1 gap-3 sm:grid-cols-2 lg:grid-cols-4">
          <div className="sm:col-span-2">
            <Field label={tr("windui_web_keywords")} hint={tr("windui_web_keywords_hint")}>
              <input
                className={inputClass}
                value={form.keywords}
                placeholder="…"
                onChange={(event) => setForm((now) => ({ ...now, keywords: event.target.value }))}
                onKeyDown={(event) => {
                  if (event.key === "Enter") {
                    setPage(1);
                    void run(1);
                  }
                }}
              />
            </Field>
          </div>
          <Field label={tr("windui_web_from")}>
            <input type="date" className={inputClass} value={form.from} onChange={(event) => setForm((now) => ({ ...now, from: event.target.value }))} />
          </Field>
          <Field label={tr("windui_web_to")}>
            <input type="date" className={inputClass} value={form.to} onChange={(event) => setForm((now) => ({ ...now, to: event.target.value }))} />
          </Field>
          <Field label={tr("windui_web_exclude")}>
            <input className={inputClass} value={form.exclude} onChange={(event) => setForm((now) => ({ ...now, exclude: event.target.value }))} />
          </Field>
          <Field label={tr("windui_web_page_size")}>
            <select className={inputClass} value={form.pageSize} onChange={(event) => setForm((now) => ({ ...now, pageSize: Number(event.target.value) }))}>
              {[10, 20, 40, 80].map((n) => (
                <option key={n} value={n}>
                  {n}
                </option>
              ))}
            </select>
          </Field>
          <div className="flex items-end gap-2 sm:col-span-2">
            <Button variant="primary" busy={busy} onClick={() => { setPage(1); void run(1); }}>
              {busy ? tr("windui_web_running") : tr("windui_web_run")}
            </Button>
            {pages > 1 ? (
              <div className="ml-auto flex items-center gap-1.5">
                <Button
                  variant="ghost"
                  disabled={page <= 1}
                  onClick={() => {
                    const next = Math.max(1, page - 1);
                    setPage(next);
                    void run(next);
                  }}
                >
                  <ChevronLeft className="h-3.5 w-3.5" />
                </Button>
                <span className="px-1 text-xs text-slate-400">
                  {page} / {pages}
                </span>
                <Button
                  variant="ghost"
                  disabled={page >= pages}
                  onClick={() => {
                    const next = Math.min(pages, page + 1);
                    setPage(next);
                    void run(next);
                  }}
                >
                  <ChevronRight className="h-3.5 w-3.5" />
                </Button>
              </div>
            ) : null}
          </div>
        </div>
      </Panel>

      {error ? <ErrorBanner message={error} onRetry={() => void run(page)} tr={tr} /> : null}

      {!error && outcome && outcome.cards.length === 0 ? (
        total === 0 && outcome.params.keywords.length === 0 ? (
          <Empty title={tr("windui_web_no_rows_title")} detail={tr("windui_web_no_rows_detail")} />
        ) : (
          <Empty title={tr("windui_web_no_rows_title")} detail={tr("windui_web_no_rows_detail")} />
        )
      ) : null}

      {outcome && outcome.cards.length > 0 ? (
        <div className="grid grid-cols-1 gap-3 sm:grid-cols-2 lg:grid-cols-3 2xl:grid-cols-4">
          {outcome.cards.map((card) => (
            <ResultCard
              key={`${card.key.file}:${card.key.rowid}`}
              card={card}
              terms={terms}
              tr={tr}
              selected={selected?.key.rowid === card.key.rowid && selected?.key.file === card.key.file}
              onSelect={() => setSelected(card)}
              onLocate={() => open(card)}
            />
          ))}
        </div>
      ) : null}

      {/* The rail's Escape handling is inside `DetailPanel` now. `framed` is the rail's own second step, and
       *  the viewer listens for the key too, so while the picture is up the rail keeps its hands off it:
       *  one press would otherwise close the picture and the column behind it together and drop the user
       *  back on the grid. */}
      {selected ? (
        <FrameRail
          card={selected}
          terms={terms}
          tr={tr}
          pauseEscape={!!framed}
          onClose={() => setSelected(null)}
          onLocate={(card) => open(card)}
          onPlay={(card) => {
            setFramedPlays(true);
            setFramed(card);
          }}
          onOpenOriginal={(card) => {
            setFramedPlays(false);
            setFramed(card);
          }}
        />
      ) : null}
      {framed ? (
        <FrameViewer
          subject={cardSubject(framed, tr)}
          tr={tr}
          startPlaying={framedPlays}
          onClose={() => setFramed(null)}
          // The walk is the page the row came from. A row that is no longer on this page — a search that
          // answered while the viewer was up — gets the picture without the arrows rather than arrows that
          // would move it to a row it was never beside.
          list={framedAt >= 0 ? subjects : undefined}
          index={framedAt >= 0 ? framedAt : undefined}
          onIndex={(next) => setFramed(outcome?.cards[next] ?? null)}
        />
      ) : null}
    </div>
  );
}

/** Split `text` on every term the query matched now lives with the rail that highlights it, in
 *  `components/DetailPanel.tsx`; the card below and the rail read the same list, so it is written once. */

function ResultCard({
  card,
  terms,
  tr,
  selected,
  onSelect,
  onLocate,
}: {
  card: RowCard;
  terms: string[];
  tr: Tr;
  selected: boolean;
  onSelect: () => void;
  onLocate: () => void;
}) {
  return (
    <article
      onClick={onSelect}
      className={cn(
        "animate-rise-in group cursor-pointer overflow-hidden rounded-2xl border bg-slate-900/60 shadow-xl shadow-black/30 backdrop-blur-xl transition-all duration-200",
        selected ? "border-indigo-500/60 ring-2 ring-indigo-500/25" : "border-slate-800/80 hover:border-slate-700",
      )}
    >
      <div
        className="relative cursor-pointer"
        // The picture opens the row, and the row's drawer is where the picture lives big. It used to reach
        // straight past that for the frame on disk, which made the most common click in the product the one
        // that reads a video: the drawer and the whole-window original came open off the same press, and
        // the wait was paid before the user had seen anything worth waiting for.
        title={tr("windui_card_preview_hint")}
        onClick={(event) => {
          event.stopPropagation();
          onSelect();
        }}
      >
        <Thumb base64={card.thumbnail} alt={card.clock} className="h-36 w-full" />
        <ZoomHint />
        <span className="absolute left-2 top-2 rounded-md bg-slate-950/80 px-1.5 py-0.5 text-[11px] font-medium tabular-nums text-slate-200 backdrop-blur">
          {card.clock}
        </span>
        {card.offset !== null ? (
          <span className="absolute right-2 top-2 rounded-md bg-slate-950/80 px-1.5 py-0.5 text-[10px] tabular-nums text-slate-400 backdrop-blur">
            +{Math.round(card.offset)}s
          </span>
        ) : null}
      </div>
      <div className="space-y-1.5 p-3">
        <p className="truncate text-xs font-medium text-slate-200">{card.title ?? tr("windui_web_title_none")}</p>
        <p className="selectable line-clamp-3 text-[11px] leading-relaxed text-slate-400">{marked(card.body, terms)}</p>
        <div className="flex items-center justify-between gap-2 pt-1">
          <span className="truncate text-[10px] text-slate-600">{card.day}</span>
          <button
            onClick={(event) => {
              event.stopPropagation();
              onLocate();
            }}
            className={cn(
              "inline-flex items-center gap-1 rounded-lg px-2 py-1 text-[10px] font-medium transition-colors",
              card.segmentPath
                ? "bg-slate-800/70 text-slate-300 hover:bg-indigo-500/20 hover:text-indigo-200"
                : "cursor-not-allowed bg-slate-800/40 text-slate-600",
            )}
          >
            <ExternalLink className="h-3 w-3" />
            {tr("windui_web_locate")}
          </button>
        </div>
      </div>
    </article>
  );
}

/* The rail a row opens is `components/FrameRail.tsx` now, and the day screen opens the same column on a
 *  stored paragraph: one row and one summary cannot answer the same click two different ways. */
