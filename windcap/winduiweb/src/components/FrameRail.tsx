/* The rail on a frame row: what the Search screen's drawer was, drawn once.

   It is `DetailPanel` with the five lines a row actually has, the picture the index holds for it, and the
   two doors on the same footage. The day screen opens the same rail when a stored paragraph names a moment
   the day holds a row for — which is the reason this is a component and not a block inside Search: a
   second copy of the picture-and-buttons is a second answer to "what does a row open", and this product
   has already been bitten by the two windows disagreeing.
 */

import { useMemo } from "react";
import { ExternalLink, Play } from "lucide-react";
import { errorMessage, locate } from "../services/api";
import type { RowCard } from "../types";
import { Button } from "./ui";
import { ZoomHint, frameSubject, type FrameSubject } from "./FrameViewer";
import { DetailPanel } from "./DetailPanel";
import { AiSummarySection } from "./AiSummary";

type Tr = (key: string, args?: Record<string, string | number>) => string;
export type Say = (text: string, tone?: "ok" | "bad") => void;

/** What a row of this screen is called when it is the one being looked at: when, in what window, and the
 *  small picture the index holds for it while the original is being read. */
export function cardSubject(card: RowCard, tr: Tr): FrameSubject {
  return frameSubject(card.key, `${card.day} ${card.clock} · ${card.title ?? tr("windui_web_title_none")}`, card.thumbnail);
}

/** Reveal a row's segment in Explorer — the one action in the rail that leaves the window.
 *
 *  Gated on the fact the index already answers, so the button cannot be pressed against footage that is
 *  gone: a row with no `segmentPath` gets the sentence, not a failed call. */
export function locateRow(card: RowCard, tr: Tr, say: Say) {
  if (!card.segmentPath) {
    say(tr("windui_web_locate") + ": " + card.segment + " — " + tr("windui_web_deep_link_absent"), "bad");
    return;
  }
  locate(card.segmentPath).catch((caught) => say(errorMessage(caught), "bad"));
}

export function FrameRail({
  card,
  terms,
  tr,
  onClose,
  pauseEscape,
  onPlay,
  onLocate,
  onOpenOriginal,
}: {
  card: RowCard;
  terms: string[];
  tr: Tr;
  onClose: () => void;
  /** Escape belongs to the full-frame viewer while it is up. */
  pauseEscape?: boolean;
  /** Open the viewer on the segment moving, from this row's own second. */
  onPlay: (card: RowCard) => void;
  onLocate: (card: RowCard) => void;
  onOpenOriginal: (card: RowCard) => void;
}) {
  const rows = useMemo(
    (): [string, string][] => [
      [tr("windui_web_from"), `${card.day} ${card.clock}`],
      [tr("rail.frame.segment"), card.segment],
      [tr("rail.frame.at_second"), card.offset === null ? "—" : `${Math.round(card.offset)} s`],
      [tr("rail.frame.file"), card.key.file],
      [tr("rail.frame.rowid"), String(card.key.rowid)],
    ],
    [card, tr],
  );

  return (
    <DetailPanel
      open
      onClose={onClose}
      pauseEscape={pauseEscape}
      title={card.clock}
      subtitle={card.day}
      rows={rows}
      body={card.body}
      terms={terms}
      bodyLabel={tr("windui_web_detail")}
      thumbnail={
        <>
          {/* The rail's own picture, and the second step of the click that opens this column.

              It is the preview the index stores, drawn no larger than it is — `max-h/max-w` only shrink, so
              nothing here is stretched and passed off as the frame. Reaching for the frame on disk used to be
              this panel's first act, which made the rail wait on a video seek before it showed anything; now
              the row answers at once, and the original is one deliberate click away, at full size, with the
              rows either side of it to walk to. */}
          <button
            onClick={() => onOpenOriginal(card)}
            title={tr("windui_frame_hint")}
            className="group relative shrink-0 self-center overflow-hidden rounded-xl border border-slate-800/80 bg-slate-900/60 transition-colors hover:border-indigo-500/60"
          >
            {card.thumbnail ? (
              <img
                src={`data:image/jpeg;base64,${card.thumbnail}`}
                alt={card.clock}
                loading="lazy"
                className="max-h-[46vh] w-auto max-w-full cursor-zoom-in object-contain"
              />
            ) : (
              <span className="flex h-36 w-64 cursor-zoom-in items-center justify-center text-[10px] uppercase tracking-wide text-slate-600">
                —
              </span>
            )}
            <ZoomHint />
          </button>
          <p className="-mt-2 shrink-0 text-center text-[11px] leading-relaxed text-slate-500">
            {tr("windui_frame_stored_only")} · {tr("windui_frame_hint")}
          </p>
        </>
      }
      footer={
        card.deepLink ? (
          <a href={card.deepLink} className="selectable break-all text-xs text-indigo-300 underline-offset-2 hover:underline">
            {card.deepLink}
          </a>
        ) : (
          <p className="text-[11px] leading-relaxed text-amber-300/70">{tr("windui_web_deep_link_absent")}</p>
        )
      }
      actions={
        /* Two doors on the same footage, and the moving one first.
         *
         *  `locate` was the only one this column had, which meant the product's answer to "let me see this
         *  moment" was to hand the file to another program that does not know which moment was meant. Both
         *  buttons are gated on the same fact the index already answers — is this row's segment still on
         *  disk — so neither can be pressed against footage that is gone.
         */
        <>
          <Button variant="primary" className="flex-1" onClick={() => onPlay(card)} disabled={!card.segmentPath} title={tr("windui_web_play_hint")}>
            <Play className="h-4 w-4" />
            {tr("windui_web_play")}
          </Button>
          <Button variant="ghost" onClick={() => onLocate(card)} disabled={!card.segmentPath}>
            <ExternalLink className="h-4 w-4" />
            {tr("windui_web_locate")}
          </Button>
        </>
      }
    >
      <AiSummarySection row={card} tr={tr} />
    </DetailPanel>
  );
}
