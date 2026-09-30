/* The picture behind one row, at the resolution it was recorded.
 *
 * Three screens draw a preview from the index — the result card, the day's strip, the month's lightbox —
 * and all three now open the same viewer, so this file holds it once. The reason it is shared rather than
 * copied is the reason the window keeps repeating: a second implementation is a second answer, and the
 * three doors disagree about nothing more visibly than about what a click on a picture does.
 *
 * Two clicks, in this order, is what a picture asks for now. The cheap one is the index's own preview,
 * which is already in the row the screen holds and paints without a disk; the expensive one is the frame
 * behind it, which is a JPEG off a screenshot slice or a seek into a video, and is worth waiting for only
 * once the user has said they want that row. A result card therefore opens its drawer, and the drawer's
 * picture opens this viewer. The strip and the lightbox have no drawer to open, so their picture is the
 * one click they have.
 *
 * What a click asks for is a row *key*, never a path. `frame` on the Rust side re-reads the row from the
 * index and opens the screenshot or the video the index says is behind it, which is both the honest
 * answer and the one a webview cannot steer somewhere else.
 */

import { useCallback, useEffect, useRef, useState } from "react";
import { ChevronLeft, ChevronRight, Maximize2, Pause, Play, RotateCcw, X } from "lucide-react";
import { errorMessage, loadFrame, playPrepare, playSource, type Frame } from "../services/api";
import type { PlaySource, RowKey } from "../types";
import { cn } from "./ui";

type Tr = (key: string, args?: Record<string, string | number>) => string;

/** A row the viewer can be asked to enlarge: its identity, the words to put over the picture, and the
 *  small preview the index already carries for it — which is what paints while the original is read. */
export type FrameSubject = { key: RowKey; caption: string; preview: string | null };

export const frameSubject = (key: RowKey, caption: string, preview: string | null = null): FrameSubject => ({
  key,
  caption,
  preview,
});

/** A frame is read once per row, so a row is its own cache key. */
const frameCacheKey = (key: RowKey) => `${key.file}:${key.rowid}`;

/**
 * The frames this window has already read, oldest first.
 *
 * Walking the results with ← and then back with → must not ask the disk for the same row twice — the
 * whole cost of a click is the read, and a second read of a row the user has just left is the complaint
 * this cache exists to answer. It is capped because a page is up to eighty 1080p JPEGs: the last few
 * rows are what a person steps between, and everything else is memory that buys nothing.
 */
const READ_FRAMES = new Map<string, Frame | null>();
const READ_FRAMES_MAX = 6;

function rememberFrame(key: RowKey, frame: Frame | null) {
  const id = frameCacheKey(key);
  READ_FRAMES.delete(id);
  READ_FRAMES.set(id, frame);
  while (READ_FRAMES.size > READ_FRAMES_MAX) {
    const oldest = READ_FRAMES.keys().next().value;
    if (oldest === undefined) break;
    READ_FRAMES.delete(oldest);
  }
}

/**
 * Which second of its segment a returned picture really is, when the door says.
 *
 * A row's caption is its `videofile_time`, and for a frame cut out of a segment that is a claim about
 * the instant rather than the instant: the stored timestamp was computed by dividing the row's frame
 * number by the configured recording rate, while the segment on disk runs at one frame per second, so
 * the two can sit minutes apart (`wind_ui::backend::Frame::second`). Showing that frame under this
 * caption without saying which one it is would be the same bug one layer up, so the viewer reads the
 * number the seek used and puts it on the line.
 *
 * Optional here rather than assumed: the field is the Rust door's to hand over, and a viewer that
 * invented a second would be worse than one that says nothing. The door answers `null` when no seek was
 * needed — the picture is the row's own stored file — and then this line stays the door's name.
 */
const shownSecond = (frame: Frame): number | null => frame.shownOffset ?? null;

/** The second, spelled the way the player's own caption spells it — digits and a unit, no new phrase. */
const atSecond = (at: number) => `+${at}s`;

/** The index's own preview: a base64 JPEG that travels with the row, so a grid paints without a fetch. */
export function Thumb({ base64, className, alt }: { base64: string | null; className?: string; alt: string }) {
  if (!base64) {
    return (
      <div className={cn("flex items-center justify-center bg-slate-800/60 text-[10px] uppercase tracking-wide text-slate-600", className)}>
        —
      </div>
    );
  }
  return <img src={`data:image/jpeg;base64,${base64}`} alt={alt} loading="lazy" className={cn("object-cover", className)} />;
}

/** The small corner glyph that says "this picture is a door", shown on hover so a grid of them stays quiet. */
export function ZoomHint() {
  return <Maximize2 className="pointer-events-none absolute bottom-2 right-2 h-3.5 w-3.5 text-slate-200/70 opacity-0 transition-opacity group-hover:opacity-100" />;
}

/** The index's own preview, drawn no larger than it is.
 *
 *  `max-h/max-w` only ever shrink a picture, so a 512 px preview cannot be stretched across a 1080p box and
 *  passed off as the frame behind it. It is here to be looked at while the real one is read, and to be the
 *  last thing on screen when there is no real one left — which is what `windui_frame_missing` says out loud.
 */
function StoredPreview({ base64, alt, note }: { base64: string | null; alt: string; note: string }) {
  return (
    <div className="flex min-h-0 flex-1 flex-col items-center justify-center gap-2">
      <p className="shrink-0 text-[11px] text-slate-400">{note}</p>
      {base64 ? (
        <div className="flex min-h-0 flex-1 items-center justify-center">
          <img src={`data:image/jpeg;base64,${base64}`} alt={alt} className="max-h-full max-w-full rounded-xl border border-slate-800/60 object-contain" />
        </div>
      ) : null}
    </div>
  );
}

/**
 * The original picture of one row, read from disk when this asks for it.
 *
 * The stored preview is sized for a card, which is fine in a card and a lie at 46 vh — so the whole-window
 * viewer comes through here, and the three answers are three states: reading, absent, and shown with the
 * door it came through. `object-contain` is load-bearing: a crop would hide the edges of the screen the
 * user is trying to read.
 *
 * `subject.preview` is the row's own stored picture, which every caller already holds. It is never
 * labelled as the original: it paints while the read is in flight, and it stays painted when the read
 * answers "nothing is left on disk" — because the honest sentence about a lost frame is easier to believe
 * next to the small picture that survived.
 */
export function OriginalFrame({ subject, tr, className }: { subject: FrameSubject; tr: Tr; className?: string }) {
  /** The read, tagged with the row it belongs to.
   *
   *  A step of `→` changes the subject one render before its effect runs, and a state that only holds a
   *  picture would spend that render painting the previous row under the new row's caption — a viewer that
   *  shows the wrong picture for one frame is worse than one that shows nothing. Tagging the answer with
   *  the key it was read for makes a stale answer simply not an answer yet.
   */
  const [read, setRead] = useState<{ for: string; frame: Frame | null; failed: string | null } | null>(null);
  const id = frameCacheKey(subject.key);
  const preview = subject.preview;
  const settled = read && read.for === id ? read : null;
  const frame = settled?.frame;

  /** The frame the viewer was showing before this one.
   *
   *  A step of `→` used to collapse a 1080p picture back to the 512 px preview for the half second the new
   *  read takes, which looks like the window losing what it had. Holding the last frame — dimmed, with the
   *  same waiting line over it — is what every photo viewer does, and it is honest: the caption above has
   *  already moved to the next row, so nothing here claims the old picture is the new one.
   */
  const held = useRef<{ for: string; frame: Frame } | null>(null);

  useEffect(() => {
    let alive = true;
    const already = READ_FRAMES.get(id);
    if (already !== undefined) {
      // A row walked back to with ← is answered from the cache, not from the disk a second time. Touching
      // the entry keeps the handful the walk is likely to revisit, and drops the ones it has left.
      READ_FRAMES.delete(id);
      READ_FRAMES.set(id, already);
      if (already) held.current = { for: id, frame: already };
      setRead({ for: id, frame: already, failed: null });
      return;
    }
    setRead(null);
    loadFrame(subject.key)
      .then((found) => {
        rememberFrame(subject.key, found);
        if (found) held.current = { for: id, frame: found };
        if (alive) setRead({ for: id, frame: found, failed: null });
      })
      .catch((caught) => {
        // A failure to ask is not "there is nothing there", and the two must not share a sentence.
        if (alive) setRead({ for: id, frame: null, failed: errorMessage(caught) });
      });
    return () => {
      alive = false;
    };
    // `id` is the row's identity spelled as one string (`file:rowid`), so this is the row's key and nothing
    // else: re-fetching on every render would re-read the disk on every keystroke of a walk.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [id]);

  if (!settled) {
    // The frame this viewer was showing a moment ago, held while the next one is read.
    const last = held.current && held.current.for !== id ? held.current.frame : null;
    if (last) {
      return (
        <div className={cn("flex min-h-0 flex-col items-center gap-2", className)}>
          <p className="shrink-0 text-[11px] text-slate-400">{tr("windui_frame_loading")}</p>
          <div className="flex min-h-0 flex-1 items-center justify-center">
            <img
              src={`data:image/jpeg;base64,${last.base64}`}
              alt=""
              className="max-h-full max-w-full rounded-xl border border-slate-800/60 object-contain opacity-60"
            />
          </div>
        </div>
      );
    }
    return (
      <div className={cn("flex min-h-0 flex-col items-center justify-center bg-slate-800/60 text-[11px] text-slate-400", className)}>
        {preview ? (
          <StoredPreview base64={preview} alt={subject.caption} note={tr("windui_frame_loading")} />
        ) : (
          <p>{tr("windui_frame_loading")}</p>
        )}
      </div>
    );
  }
  if (settled?.failed) {
    return <div className={cn("p-3 text-[11px] leading-relaxed text-amber-300/90", className)}>{settled.failed}</div>;
  }
  if (!frame) {
    return (
      <div className={cn("flex min-h-0 flex-col items-center gap-2 bg-slate-800/40 p-3 text-center", className)}>
        <p className="shrink-0 text-[11px] leading-relaxed text-amber-300/90">{tr("windui_frame_missing")}</p>
        {preview ? <StoredPreview base64={preview} alt={subject.caption} note={tr("windui_frame_stored_only")} /> : null}
      </div>
    );
  }
  return (
    <figure className={cn("flex min-h-0 flex-col space-y-1", className)}>
      <img
        src={`data:image/jpeg;base64,${frame.base64}`}
        alt={subject.caption}
        className="min-h-0 w-full flex-1 rounded-xl border border-slate-800 object-contain"
      />
      <figcaption className="shrink-0 text-[10px] text-slate-500">
        {/* The door's own sentence, and when it reports which second it cut, that second beside it: the
            caption above is the row's claimed instant and this picture is not always at it. */}
        {shownSecond(frame) === null ? tr(frame.sourceKey) : `${tr(frame.sourceKey)} · ${atSecond(shownSecond(frame)!)}`}
      </figcaption>
    </figure>
  );
}

/**
 * The segment this row sits inside, playing in the window.
 *
 * This is the door the viewer used to lack. The still says what was on the screen at one instant, and
 * nothing here could answer "what was happening around it" — the only way out was `locate`, a hand-off to
 * some other program that does not know which moment the user clicked for.
 *
 * Three facts about how this product makes its footage are what let the player be a plain `<video>` rather
 * than a decoder written here, and `src-tauri/src/video.rs` carries the fourth — why the bytes reach the
 * element at all:
 *
 *   * `windmaint` encodes at one frame per second, in and out, so second S *is* frame S. That is the same
 *     property every `videofile_time` → offset lookup in this product already rests on;
 *   * it writes `-an`, so there is no audio track to drift out of step and nothing to unmute;
 *   * it writes `-movflags +faststart`, so the index sits at the head of the file and a seek to minute four
 *     does not read the first four minutes to get there.
 *
 * The row's own moment is where playback starts: `play_source` answers with the offset the index computed
 * for this row, and the element seeks to it as soon as it knows the file's length, because "play this" was
 * asked about this row and not about the beginning of its segment. The scrub bar, the jump buttons and the
 * rate menu are the browser's, and they are the point — a box that could only run forward from second zero
 * would be a worse player than the hand-off it replaces.
 *
 * Stepping to another row with `←`/`→` keeps the player open and moves it, which is why `key={source.url}`
 * is on the element: a new segment gets a new play head rather than inheriting the old one.
 */
/** Ask this webview whether it can decode a codec, in the only terms that are not a guess.
 *
 *  `canPlayType` answers `""` / `"maybe"` / `"probably"`; anything but an empty string counts as playable,
 *  because refusing to try a file the element might well play digs the same hole as promising one it
 *  cannot. One probe element for the window: this is a question about the machine, not about a row, and it
 *  does not change while the window is open.
 */
let probe: HTMLVideoElement | null = null;
function canPlay(mime: string): boolean {
  probe ??= document.createElement("video");
  return probe.canPlayType(mime) !== "";
}

function SegmentPlayer({ subject, tr, className }: { subject: FrameSubject; tr: Tr; className?: string }) {
  /** The answer, tagged with the row it was asked for — the rule `OriginalFrame` paints by, so a step never
   *  plays the previous row's segment under the new row's caption. */
  const [asked, setAsked] = useState<{ for: string; source: PlaySource | null; failed: string | null } | null>(null);
  /** Whether the element itself refused the file. `source: null` is the door answering "this row has no
   *  footage"; this is the other failure — a stream the machine cannot decode — and the two need different
   *  sentences, because only one of them is something the user can fix. */
  const [refused, setRefused] = useState(false);
  /** True while the Rust side is asking `ffmpeg` for a copy this webview can decode. A distinct state
   *  rather than `null`: "still working on it" and "there is nothing here" have to read differently. */
  const [preparing, setPreparing] = useState(false);
  const id = frameCacheKey(subject.key);
  const settled = asked && asked.for === id ? asked : null;
  const element = useRef<HTMLVideoElement | null>(null);
  /** The row this element has already been placed on, so a later `loadedmetadata` cannot yank the play head
   *  back from wherever the user dragged it. */
  const placed = useRef<string | null>(null);

  useEffect(() => {
    let alive = true;
    setAsked(null);
    setRefused(false);
    setPreparing(false);
    placed.current = null;
    playSource(subject.key)
      .then(async (found) => {
        if (!alive) return;
        // The machine, not this window, decides whether HEVC plays: the optional store extension may or
        // may not be installed, and `canPlayType` is the only honest way to ask. When it says no, the file
        // is handed to the ffmpeg this product already uses for every other piece of footage work, and the
        // row plays the copy — instead of being reported to the user as something they have to go fix.
        if (found && found.mime && !canPlay(found.mime)) {
          // The original is shown as the answer first, so the sentence below can name the codec it is
          // working around rather than printing a placeholder for a fact the window already has.
          setAsked({ for: id, source: found, failed: null });
          setPreparing(true);
          try {
            const prepared = await playPrepare(subject.key);
            if (alive) setAsked({ for: id, source: prepared ?? found, failed: null });
          } catch (caught) {
            // The copy failed. The original is still what there is, and the sentence says which step broke.
            if (alive) setAsked({ for: id, source: found, failed: errorMessage(caught) });
          } finally {
            if (alive) setPreparing(false);
          }
          return;
        }
        setAsked({ for: id, source: found, failed: null });
      })
      .catch((caught) => {
        // A failure to ask is not "there is nothing to play", and the two must not share a sentence.
        if (alive) setAsked({ for: id, source: null, failed: errorMessage(caught) });
      });
    return () => {
      alive = false;
      // The window hides behind the tray rather than exiting, so an unmount is the one moment this element is
      // certain to be leaving. A media pipeline left running behind a closed viewer is background work for a
      // program that spends most of its life idle.
      element.current?.pause();
    };
    // `id` is the row's identity spelled as one string, which is exactly what a step changes.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [id]);

  /** Put the element on the row's own second, once per segment.
   *
   *  There is no duration to seek inside before the metadata arrives, and after a manual scrub the user's
   *  own position wins — which is why this runs from `onLoadedMetadata` rather than from an effect.
   */
  const placeOnMoment = () => {
    const video = element.current;
    const source = settled?.source;
    if (!video || !source || placed.current === id) return;
    const at = source.offset;
    if (at !== null) video.currentTime = at;
    placed.current = id;
  };

  if (preparing) {
    return (
      <div className={cn("flex min-h-0 flex-col items-center justify-center gap-3 bg-slate-800/60 p-4 text-center text-[11px] leading-relaxed text-slate-400", className)}>
        <p className="max-w-[46ch]">{tr("windui_web_play_transcoding", { codec: settled?.source?.codec ?? "?" })}</p>
        {subject.preview ? <StoredPreview base64={subject.preview} alt={subject.caption} note={tr("windui_frame_stored_only")} /> : null}
      </div>
    );
  }
  if (!settled) {
    return (
      <div className={cn("flex min-h-0 flex-col items-center justify-center bg-slate-800/60 text-[11px] text-slate-400", className)}>
        {subject.preview ? <StoredPreview base64={subject.preview} alt={subject.caption} note={tr("windui_web_play_loading")} /> : <p>{tr("windui_web_play_loading")}</p>}
      </div>
    );
  }
  if (settled.failed || refused) {
    return <div className={cn("p-3 text-[11px] leading-relaxed text-amber-300/90", className)}>{settled.failed ?? tr("windui_web_play_undecodable")}</div>;
  }
  const source = settled.source;
  if (!source) {
    // Nothing to play, said plainly, with the small preview still on screen to show what the row does hold.
    return (
      <div className={cn("flex min-h-0 flex-col items-center gap-2 bg-slate-800/40 p-3 text-center", className)}>
        <p className="shrink-0 text-[11px] leading-relaxed text-amber-300/90">{tr("windui_web_play_none")}</p>
        {subject.preview ? <StoredPreview base64={subject.preview} alt={subject.caption} note={tr("windui_frame_stored_only")} /> : null}
      </div>
    );
  }
  const at = source.offset;
  return (
    <figure className={cn("flex min-h-0 flex-col space-y-1", className)}>
      {/* `muted` is not a guess about the file: these segments carry no audio track at all, and a browser
         will not start a video that has one on its own. */}
      <video
        key={source.url}
        ref={element}
        src={source.url}
        controls
        autoPlay
        muted
        playsInline
        onLoadedMetadata={placeOnMoment}
        onError={() => setRefused(true)}
        className="min-h-0 w-full flex-1 rounded-xl border border-slate-800 bg-black object-contain"
      />
      <figcaption className="flex shrink-0 items-center justify-between gap-3 text-[10px] text-slate-500">
        <span className="truncate">{tr("windui_web_playing", { at: at === null ? "—" : atSecond(at), name: source.name })}</span>
        {at !== null ? (
          <button
            // After a scrub the row's own second is still the moment the user asked about, and this is the
            // one control that returns to it without closing and reopening the viewer.
            onClick={() => {
              const video = element.current;
              if (video) video.currentTime = at;
            }}
            title={tr("windui_web_play_back_to_moment")}
            className="inline-flex shrink-0 items-center gap-1 rounded-lg px-2 py-1 transition-colors hover:bg-slate-800 hover:text-white"
          >
            <RotateCcw className="h-3 w-3" />
            {tr("windui_web_play_back_to_moment")}
          </button>
        ) : null}
      </figcaption>
    </figure>
  );
}

/**
 * The whole-window viewer: what the picture inside a row's drawer opens.
 *
 * It walks. A row is never alone — a result page, a day's strip and a month's lightbox are all lists the
 * user was already looking at before the click — and a viewer that shows one picture of a list they can
 * see, and cannot leave, turns every next row into a close, a click and a wait. So `←`/`→` and the two
 * arrows move through `list`, one row at a time, and each row's frame is read on the step that asks for
 * it. Nothing here loads a page of pictures: the read is the expensive half of a click, and the point of
 * the arrows is to pay it for the rows the user actually opens.
 */
export function FrameViewer({
  subject,
  tr,
  onClose,
  list,
  index,
  onIndex,
  startPlaying,
}: {
  subject: FrameSubject;
  tr: Tr;
  onClose: () => void;
  /** The rows this picture sits among, in the order the screen shows them. */
  list?: FrameSubject[];
  index?: number;
  onIndex?: (next: number) => void;
  /** Open straight into the player, which is what the drawer's own play button asks for: one press from the
   *  row to the moving picture, with no read-only step in between. */
  startPlaying?: boolean;
}) {
  /** Still or moving, for the whole viewer rather than per row.
   *
   *  A step of `→` while playing keeps playing and changes what plays: walking a list with the player open
   *  is how a person reads a stretch of an afternoon, and closing the player on every step would ask them to
   *  press the button again for each row they look at.
   */
  const [playing, setPlaying] = useState(!!startPlaying);
  const walking = !!list && list.length > 1 && !!onIndex;
  const at = index ?? -1;
  const step = useCallback(
    (by: number) => {
      if (!walking || !list || !onIndex) return;
      const next = at + by;
      if (next >= 0 && next < list.length) onIndex(next);
    },
    [at, list, onIndex, walking],
  );

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") onClose();
      else if (event.key === "ArrowLeft") step(-1);
      else if (event.key === "ArrowRight") step(1);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose, step]);

  return (
    <div className="fixed inset-0 z-[60] flex flex-col bg-slate-950/95 p-4 backdrop-blur-xl animate-fade-in" onClick={onClose}>
      <div className="mb-3 flex shrink-0 items-center justify-between gap-4 text-xs text-slate-400">
        <p className="truncate">
          <span className="font-medium tabular-nums text-slate-200">{subject.caption}</span>
        </p>
        <div className="flex shrink-0 items-center gap-1.5">
          <button
            // The one control that stops the window being a picture viewer with a video inside it: the
            // toggle stays lit while the segment is moving, and pressing it again returns to the still the
            // row was indexed from — which is the cheaper answer, and the one worth keeping.
            onClick={(event) => {
              event.stopPropagation();
              setPlaying((now) => !now);
            }}
            title={playing ? tr("windui_web_play_stop_hint") : tr("windui_web_play_hint")}
            className={cn(
              "inline-flex items-center gap-1 rounded-lg px-2 py-1 text-[11px] font-medium transition-colors",
              playing ? "bg-indigo-500/25 text-indigo-100 hover:bg-indigo-500/35" : "text-slate-400 hover:bg-slate-800 hover:text-white",
            )}
          >
            {playing ? <Pause className="h-3.5 w-3.5" /> : <Play className="h-3.5 w-3.5" />}
            {playing ? tr("windui_web_play_stop") : tr("windui_web_play")}
          </button>
          {walking ? (
            <>
              <button
                onClick={(event) => {
                  event.stopPropagation();
                  step(-1);
                }}
                disabled={at <= 0}
                className="rounded-lg p-1.5 text-slate-400 transition-colors hover:bg-slate-800 hover:text-white disabled:cursor-not-allowed disabled:opacity-30"
                title={tr("windui_frame_prev")}
              >
                <ChevronLeft className="h-4 w-4" />
              </button>
              <span className="tabular-nums text-slate-500">
                {at + 1} / {list?.length}
              </span>
              <button
                onClick={(event) => {
                  event.stopPropagation();
                  step(1);
                }}
                disabled={at >= (list?.length ?? 0) - 1}
                className="rounded-lg p-1.5 text-slate-400 transition-colors hover:bg-slate-800 hover:text-white disabled:cursor-not-allowed disabled:opacity-30"
                title={tr("windui_frame_next")}
              >
                <ChevronRight className="h-4 w-4" />
              </button>
            </>
          ) : null}
          <button
            onClick={onClose}
            className="rounded-lg p-1.5 text-slate-400 transition-colors hover:bg-slate-800 hover:text-white"
            title={tr("windui_web_close")}
          >
            <X className="h-4 w-4" />
          </button>
        </div>
      </div>
      {playing ? <SegmentPlayer subject={subject} tr={tr} className="min-h-0 flex-1" /> : <OriginalFrame subject={subject} tr={tr} className="min-h-0 flex-1" />}
    </div>
  );
}
