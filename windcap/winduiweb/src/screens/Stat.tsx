import { useCallback, useEffect, useMemo, useState } from "react";
import { errorMessage, loadLightbox, loadTotals, loadWordCloud } from "../services/api";
import type { CloudWord, LightboxTile, Totals } from "../types";
import { Button, ErrorBanner, Panel, Pill, SectionTitle, cn } from "../components/ui";
import { FrameViewer, ZoomHint, frameSubject, type FrameSubject } from "../components/FrameViewer";
import { DetailPanel } from "../components/DetailPanel";
import { AiSummarySection } from "../components/AiSummary";

type Tr = (key: string, args?: Record<string, string | number>) => string;

function thisMonth() {
  const now = new Date();
  return { year: now.getFullYear(), month: now.getMonth() + 1 };
}

/** A lightbox tile is a key, a time, a preview and the row's own stamp — which is all the grid needs, and
 *  all the viewer needs too, because the frame is looked up by key when the tile is clicked. The stamp is
 *  spelled by the side that read the row: these seconds are naive-local, and formatting them here would
 *  move every picture in the month by the machine's offset from UTC. */
const tileSubject = (tile: LightboxTile): FrameSubject => frameSubject(tile.key, tile.stamp, tile.thumbnail);

export function StatScreen({ tr }: { tr: Tr }) {
  const [which, setWhich] = useState(thisMonth);
  const value = `${which.year}-${String(which.month).padStart(2, "0")}`;
  const [totals, setTotals] = useState<Totals | null>(null);
  const [tiles, setTiles] = useState<LightboxTile[]>([]);
  const [cloud, setCloud] = useState<CloudWord[]>([]);
  const [error, setError] = useState<string | null>(null);
  /** Which tile of the month's grid is being looked at in the whole-window viewer, by position in `subjects`. */
  const [framed, setFramed] = useState<number | null>(null);
  /** Which tile opened the rail. A tile is a key, a time and a preview, and the rail is where the rest of
   *  what this install knows about that moment lives — the stored paragraph included. The viewer stays the
   *  tile's own second step, so nothing here makes a picture harder to reach than it was. */
  const [rail, setRail] = useState<number | null>(null);

  const load = useCallback(async (year: number, month: number) => {
    setError(null);
    try {
      const [next, lightbox, words] = await Promise.all([
        loadTotals(year, month),
        loadLightbox(year, month),
        loadWordCloud(year, month),
      ]);
      setTotals(next);
      setTiles(lightbox);
      setCloud(words);
    } catch (caught) {
      setError(errorMessage(caught));
    }
  }, []);

  useEffect(() => {
    void load(which.year, which.month);
  }, [which, load]);

  const byDay = useMemo(() => new Map(totals?.month.points.map((point) => [point.day, point]) ?? []), [totals]);
  /** The month's tiles in grid order, which is the order `←`/`→` walks them in the viewer. */
  const subjects = useMemo(() => tiles.map(tileSubject), [tiles]);
  const days = useMemo(() => new Date(which.year, which.month, 0).getDate(), [which]);
  const busiest = Math.max(1, ...(totals?.month.points.map((point) => point.rows) ?? [1]));
  const maxWord = Math.max(1, ...cloud.map((word) => word.count));

  return (
    <div className="space-y-5">
      <Panel className="flex flex-wrap items-center justify-between gap-4 p-4">
        <SectionTitle
          title={tr("windui_web_stat_title")}
          detail={tr("windui_web_stat_detail")}
          right={totals ? <Pill tone="indigo">{tr("windui_web_rows", { count: totals.month.rows.toLocaleString() })}</Pill> : undefined}
        />
        <input
          type="month"
          value={value}
          onChange={(event) => {
            const [y, m] = event.target.value.split("-").map(Number);
            if (y && m) setWhich({ year: y, month: m });
          }}
          className="rounded-xl border border-slate-700/70 bg-slate-950/60 px-3 py-2 text-sm text-slate-100 outline-none focus:border-indigo-500/60"
        />
      </Panel>

      {error ? <ErrorBanner message={error} onRetry={() => void load(which.year, which.month)} tr={tr} /> : null}
      {totals?.month.warnings.length ? (
        <div className="flex flex-wrap gap-2">
          {totals.month.warnings.map((one) => (
            <Pill key={one} tone="amber">
              {one}
            </Pill>
          ))}
        </div>
      ) : null}

      <Panel className="p-4">
        <p className="mb-3 text-[11px] uppercase tracking-wide text-slate-500">{value}</p>
        <div className="grid grid-cols-[repeat(auto-fill,minmax(30px,1fr))] gap-1.5">
          {Array.from({ length: days }, (_, index) => index + 1).map((day) => {
            const point = byDay.get(day);
            const rows = point?.rows ?? 0;
            const shade = rows === 0 ? 0 : 0.15 + 0.75 * (rows / busiest);
            return (
              <div
                key={day}
                title={tr("windui_web_stat_day_tooltip", {
                  date: `${value}-${String(day).padStart(2, "0")}`,
                  count: rows,
                  hours: (point?.hours ?? 0).toFixed(2),
                })}
                className={cn(
                  "flex h-8 items-center justify-center rounded-md border text-[10px] tabular-nums transition-colors",
                  rows === 0 ? "border-slate-800/60 bg-slate-900/40 text-slate-700" : "border-indigo-500/25 text-indigo-100",
                )}
                style={rows === 0 ? undefined : { backgroundColor: `rgba(99, 102, 241, ${shade.toFixed(3)})` }}
              >
                {day}
              </div>
            );
          })}
        </div>
      </Panel>

      {cloud.length > 0 ? (
        <Panel className="p-4">
          <p className="mb-3 text-[11px] uppercase tracking-wide text-slate-500">{tr("windui_web_wordcloud")}</p>
          <p className="selectable flex flex-wrap items-baseline gap-x-3 gap-y-1">
            {cloud.map((word) => (
              <span
                key={word.text}
                className="leading-tight text-slate-200"
                style={{ fontSize: `${(0.7 + 1.5 * (word.count / maxWord)).toFixed(2)}rem`, opacity: 0.55 + 0.45 * (word.count / maxWord) }}
                title={`${word.count}`}
              >
                {word.text}
              </span>
            ))}
          </p>
        </Panel>
      ) : null}

      {tiles.length > 0 ? (
        <Panel className="p-4">
          <p className="mb-3 text-[11px] uppercase tracking-wide text-slate-500">
            {tr("windui_web_lightbox")} · {tiles.length}
          </p>
          <div className="grid grid-cols-[repeat(auto-fill,minmax(210px,1fr))] gap-2">
            {tiles.map((tile, position) => (
              // Every tile in this grid is one row of the month, and a row's picture is a door: the click
              // asks for the frame behind it, at the resolution it was recorded. The tile carries a key, a
              // stamp and its own small preview, and that is exactly what the door takes — the preview is
              // what the viewer paints while the frame is being read.
              <button
                key={`${tile.key.file}:${tile.key.rowid}`}
                onClick={() => setRail(position)}
                title={tr("stat.tile.hint")}
                className="group relative cursor-zoom-in overflow-hidden rounded-xl border border-slate-800/80 text-left transition-colors hover:border-indigo-500/60"
              >
                {tile.thumbnail ? (
                  <img src={`data:image/jpeg;base64,${tile.thumbnail}`} alt="" loading="lazy" className="h-30 w-full object-cover" />
                ) : (
                  <div className="h-30 w-full bg-slate-800/50" />
                )}
                <ZoomHint />
                <p className="px-2 py-1 text-[10px] tabular-nums text-slate-500">{tile.stamp}</p>
              </button>
            ))}
          </div>
        </Panel>
      ) : null}

      {rail !== null && tiles[rail] ? (
        <DetailPanel
          open
          pauseEscape={framed !== null}
          onClose={() => setRail(null)}
          title={tiles[rail].stamp}
          rows={[
            [tr("rail.frame.rowid"), String(tiles[rail].key.rowid)],
            [tr("rail.frame.file"), tiles[rail].key.file],
          ]}
          thumbnail={
            tiles[rail].thumbnail ? (
              <img
                src={`data:image/jpeg;base64,${tiles[rail].thumbnail}`}
                alt=""
                className="max-h-40 w-auto max-w-full self-center rounded-xl border border-slate-800/60 object-contain"
              />
            ) : null
          }
          footer={
            <Button variant="primary" onClick={() => setFramed(rail)}>
              {tr("stat.rail.open_picture")}
            </Button>
          }
        >
          {/* The paragraph is asked for by the row itself — key and time — which is the same pair the tile
              was built from, so what the rail shows is the text written about this picture, not about a
              row the page is holding from somewhere else. */}
          <AiSummarySection row={{ key: tiles[rail].key, time: tiles[rail].time }} tr={tr} />
        </DetailPanel>
      ) : null}

      {framed !== null && subjects[framed] ? (
        <FrameViewer
          subject={subjects[framed]}
          tr={tr}
          onClose={() => setFramed(null)}
          list={subjects}
          index={framed}
          onIndex={setFramed}
        />
      ) : null}
    </div>
  );
}
