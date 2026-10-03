/* The detail rail: the column every row of this window opens on the right.

   It was the Search screen's drawer, drawn once there. A summary row wants the same column — the same
   entry motion, the same three ways out (the backdrop, the X, Escape) and the same weight on screen —
   and what it does not want is a second implementation that drifts: two rails disagree about nothing
   more visibly than about whether Escape closes one thing or two.

   So the shell lives here and knows nothing about what a row *is*. It takes its payload as
   `rows: [label, value][]`, its paragraph as `body`, and the two things that genuinely differ between
   a frame and a summary — the picture and the buttons — as slots. What is Search-specific stays in
   Search: the row's own five lines and the terms the query matched. `marked` comes along because the
   rail is what highlights, and the card beside it reads the same list.
 */

import { useEffect, type ReactNode } from "react";
import { X } from "lucide-react";
import { cn } from "./ui";

/** One label and its value on a line. The rail's payload block and the day screen's summary block both
 *  draw it; a third copy of the classes is how the two start disagreeing about what a value does when it
 *  does not fit, which is why `title` carries the whole string here rather than at each call site. */
export function PanelRow({ label, value, className }: { label: string; value: string; className?: string }) {
  return (
    <div className={cn("flex items-baseline justify-between gap-3 text-[11px]", className)}>
      <span className="shrink-0 text-slate-500">{label}</span>
      <span className="selectable truncate text-slate-300" title={value}>
        {value}
      </span>
    </div>
  );
}

/** The rail's payload block — a hairline box of `PanelRow`s. The day screen's summary panel draws the same
 *  block for the same reason: a paragraph and its provenance belong attached, in both places. */
export function PanelRowList({ rows, className }: { rows: [string, string][]; className?: string }) {
  if (rows.length === 0) return null;
  return (
    <div className={cn("space-y-1.5 rounded-xl border border-slate-800/80 bg-slate-900/50 p-3", className)}>
      {rows.map(([label, value]) => (
        <PanelRow key={label} label={label} value={value} />
      ))}
    </div>
  );
}

/**
 * Split `text` on every term the query actually matched, longest first so a shorter variant cannot
 * eat a longer one's characters. The terms come from the side that built the query, because a
 * similar-glyph variant is only known to the code that generated it.
 */
export function marked(text: string, terms: string[]): (string | ReactNode)[] {
  const alive = terms.filter((term) => term.length > 0).sort((a, b) => b.length - a.length);
  if (alive.length === 0) return [text];
  const haystack = text.toLowerCase();
  const out: (string | ReactNode)[] = [];
  let at = 0;
  let key = 0;
  while (at < text.length) {
    let hit: { index: number; term: string } | null = null;
    for (const term of alive) {
      const found = haystack.indexOf(term.toLowerCase(), at);
      if (found >= 0 && (!hit || found < hit.index)) hit = { index: found, term };
    }
    if (!hit) {
      out.push(text.slice(at));
      break;
    }
    if (hit.index > at) out.push(text.slice(at, hit.index));
    out.push(
      <mark key={`m${key++}`} className="rounded bg-indigo-500/25 px-0.5 text-indigo-100">
        {text.slice(hit.index, hit.index + hit.term.length)}
      </mark>,
    );
    at = hit.index + hit.term.length;
  }
  return out;
}

export type DetailPanelProps = {
  /** A closed rail renders nothing, so a screen can keep one mounted and still say it is shut. */
  open: boolean;
  onClose: () => void;
  /** Held by whoever owns Escape while a full-window viewer is up over this rail.
   *
   *  Both handlers sit on `window`, so one press would close the picture and the column behind it
   *  together and drop the user back on the grid. */
  pauseEscape?: boolean;
  title: string;
  subtitle?: string;
  /** The payload, attached to the row it describes: which stretch, what window, when written, who wrote it. */
  rows: [string, string][];
  /** The paragraph this row is about, highlighted against `terms` when the caller has terms. */
  body?: string;
  terms?: string[];
  bodyLabel?: string;
  thumbnail?: ReactNode;
  footer?: ReactNode;
  /** Extra sections — the rail's own AI summary of the moment is one. */
  children?: ReactNode;
  /** Rendered last, pinned to the bottom of the column the way the frame row's buttons are. */
  actions?: ReactNode;
};

export function DetailPanel({
  open,
  onClose,
  pauseEscape,
  title,
  subtitle,
  rows,
  body,
  terms,
  bodyLabel,
  thumbnail,
  footer,
  children,
  actions,
}: DetailPanelProps) {
  useEffect(() => {
    if (!open || pauseEscape) return;
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [open, pauseEscape, onClose]);

  if (!open) return null;

  return (
    <>
      <div className="fixed inset-0 z-40 bg-slate-950/60 backdrop-blur-sm animate-fade-in" onClick={onClose} />
      <aside className="animate-slide-left fixed inset-y-0 right-0 z-50 flex w-full max-w-[520px] flex-col gap-4 overflow-y-auto border-l border-slate-800 bg-slate-950/95 p-5 shadow-2xl shadow-black/60 backdrop-blur-2xl">
        <div className="flex items-start justify-between gap-4">
          <div className="min-w-0">
            <p className="truncate text-sm font-semibold text-white">{title}</p>
            {subtitle ? <p className="truncate text-xs text-slate-500">{subtitle}</p> : null}
          </div>
          <button onClick={onClose} className="rounded-lg p-1.5 text-slate-400 transition-colors hover:bg-slate-800 hover:text-white">
            <X className="h-4 w-4" />
          </button>
        </div>

        {thumbnail}

        {body !== undefined ? (
          <div className="space-y-1">
            {bodyLabel ? <p className="text-[11px] uppercase tracking-wide text-slate-500">{bodyLabel}</p> : null}
            <p className="selectable text-sm leading-relaxed text-slate-200">{marked(body, terms ?? [])}</p>
          </div>
        ) : null}

        {rows.length > 0 ? <PanelRowList rows={rows} /> : null}

        {children}

        {footer}

        {actions ? <div className="mt-auto flex items-center gap-2">{actions}</div> : null}
      </aside>
    </>
  );
}
