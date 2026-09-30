/* Every call this window can make, in one file, typed against `types.ts`.
 *
 * There is no fallback path here and no mock: if `invoke` rejects, the caller shows the message. The
 * reason is the one this product keeps repeating — a front end that quietly renders a placeholder when
 * its backend failed is how a broken install looks healthy, and the last time that happened the window
 * showed an empty library for a day the recorder had filled.
 */

import { invoke } from "@tauri-apps/api/core";
import type {
  About,
  AiStatus,
  Backlog,
  CloudWord,
  DayOutcome,
  DaySummariesDto,
  DisplayInfo,
  FormDto,
  LightboxTile,
  MaintenanceProgress,
  PeriodSummaryDto,
  PlaySource,
  PromptFormDto,
  PromptOutcomeDto,
  PromptTrialDto,
  RowKey,
  SaveResult,
  SearchOutcome,
  SearchParams,
  Stats,
  Totals,
} from "../types";

export type FormPage = "settings" | "recording";

export type RecorderState = { running: boolean; pid: number | null };


/** The record lock, so the header pill says "recording" only when a live pid owns the lock. */
export const recorderState = () => invoke<RecorderState>("recorder_state");

/** Start the deferred pass now, whatever the clock says. Answers with the request file it wrote, not
 *  with "done": the recorder honours it on its next tick. */
export const maintenanceStart = () => invoke<string>("maintenance_start");

/** Stop the running pass at its next work item. */
export const maintenanceStop = () => invoke<string>("maintenance_stop");

/** What the pass has published about itself: which of the nine steps is open, how much of it is done,
 *  and how the last one ended.
 *
 *  Read rather than subscribed, on purpose. The pass is a child of the recorder, so the window has no
 *  channel into it and inventing one (a socket, a watcher) would be a second thing to keep alive. One
 *  small file, polled while the settings page is up, answers the question the button raises. */
export const maintenanceProgress = () => invoke<MaintenanceProgress>("maintenance_progress");

/** What is waiting to be organised, as the steps themselves count it.
 *
 *  `null` until asked: the census walks the library (seconds on a big one), so it is a button rather
 *  than something the page does every time it opens. The shape is `windmaint backlog`'s JSON, passed
 *  through unchanged — one implementation of "what is waiting", the window adds no arithmetic of its
 *  own. */
export const maintenanceBacklog = () => invoke<Backlog>("maintenance_backlog");

/** The screen this process was told to open, or null. `--tab`, for the gate and the tray. */
export const startupTab = () => invoke<string | null>("startup_tab");

export const uiLocale = () => invoke<string>("ui_locale");

/** Ask the shipped catalog for these keys; a missing one comes back marked as missing. */
export const uiStrings = (keys: string[]) =>
  invoke<Record<string, string>>("ui_strings", { keys });

export const about = () => invoke<About>("about");

export const libraryStats = () => invoke<Stats>("library_stats");

export const search = (params: SearchParams) =>
  invoke<SearchOutcome>("search", { params });

export const loadDay = (year: number, month: number, day: number) =>
  invoke<DayOutcome>("day", { year, month, day });

export const loadTotals = (year: number, month: number) =>
  invoke<Totals>("totals", { year, month });

export const loadLightbox = (year: number, month: number) =>
  invoke<LightboxTile[]>("lightbox", { year, month });

export const loadWordCloud = (year: number, month: number) =>
  invoke<CloudWord[]>("word_cloud", { year, month });

export const displays = () => invoke<DisplayInfo[]>("displays");

/** Reveal a segment in Explorer. The only action here that touches the desktop outside the window. */
export const locate = (path: string) => invoke<void>("locate", { path });

/** The frame behind a row, at the resolution it was recorded. */
export type Frame = { base64: string; sourceKey: string; shownOffset: number | null };

/**
 * Ask where one row's segment can be played from. A row *key* again, never a name or a path, for the same
 * reason `loadFrame` takes one: the index decides which file a row's footage is.
 *
 * `null` means there is nothing to play — the segment is past `vid_store_day`, or this machine never had an
 * ffmpeg to encode it — which the caller says out loud instead of opening a black box.
 */
export const playSource = (key: RowKey) => invoke<PlaySource | null>("play_source", { key });

/** Ask for a copy of the row's segment this machine's media element can actually decode.
 *
 *  Called only when the webview's own `canPlayType` refuses the codec the file carries — HEVC and AV1 are
 *  what the recorder writes when the user asked for them — and it blocks while `ffmpeg` makes the copy,
 *  which is why the caller shows a sentence rather than a spinner that promises immediacy. The copy is
 *  kept, so the wait is once per segment. */
export const playPrepare = (key: RowKey) => invoke<PlaySource | null>("play_prepare", { key });

/**
 * Ask for the original picture of one row. Only the key crosses the boundary: the Rust side re-reads the
 * row from the index, so which screenshot or which segment opens is the index's answer rather than
 * something this window can point somewhere else. It is also what lets a lightbox tile — a key, a time and
 * a preview, and deliberately nothing more — open the same picture a result card does.
 *
 * `null` means this install no longer holds it — the screenshot slice was swept and the video is gone —
 * which the caller says out loud rather than papering over with the stored preview the grid already shows.
 */
export const loadFrame = (key: RowKey) => invoke<Frame | null>("frame", { key });

export const settingsRead = () => invoke<FormDto>("settings_read");
export const settingsSave = (values: Record<string, string>) =>
  invoke<SaveResult>("settings_save", { input: { values } });

export const recordingRead = () => invoke<FormDto>("recording_read");
export const recordingSave = (values: Record<string, string>) =>
  invoke<SaveResult>("recording_save", { input: { values } });

export type AiForm = {
  fields: FormDto["fields"];
  keyState: string;
  ready: boolean;
  verdict: string;
};

export const aiRead = () => invoke<AiForm>("ai_read");

/** `clearKey` is its own flag because an empty token box means "leave the stored one alone". */
export const aiSave = (values: Record<string, string>, clearKey: boolean) =>
  invoke<SaveResult>("ai_save", { input: { values, clearKey } });

export const aiTest = (values: Record<string, string>, clearKey: boolean) =>
  invoke<AiStatus>("ai_test", { input: { values, clearKey } });

/* -----------------------------------------------------------------------------------------------
 * Prompts — the seven templates the product sends, as files the user edits from this window
 *
 * `name` is one of `period_summary_system`, `period_summary_user`, `daily_summary_system`,
 * `daily_summary_user`, `tags_system`, `tags_user`, `search_system`. Every call returns all seven
 * rows as they stand after it, so a page repaints from the answer instead of asking again — and a
 * refusal is an answer too (`ok: false` with the validator's sentence), never a rejected promise.
 * ----------------------------------------------------------------------------------------------- */

export const promptsRead = () => invoke<PromptFormDto>("prompts_read");

/** Write the user's own words for one template. `error` is `wind_base::prompts`' sentence, word for
 *  word — render it as written, since it is the product talking about its own placeholders. */
export const promptSave = (name: string, text: string) =>
  invoke<PromptOutcomeDto>("prompt_save", { name, text });

/** Delete one override so the shipped words answer again. `note` says whether anything changed. */
export const promptRestore = (name: string) =>
  invoke<PromptOutcomeDto>("prompt_restore", { name });

/** The AI page's draft, so a key typed but not saved is the one the trial uses. */
export type AiDraftInput = { values: Record<string, string>; clearKey?: boolean };

/** Send one real request built from `text` — the box's text, not the file's — and report what came
 *  back. It writes nothing: not the prompt, not a summary. Pass `null` for `input` to run the trial
 *  on the endpoint already stored in `config_user.json`. The key never appears in any field. */
export const promptTrial = (name: string, text: string, input: AiDraftInput | null = null) =>
  invoke<PromptTrialDto>("prompt_trial", { name, text, input });

/* -----------------------------------------------------------------------------------------------
 * Summaries — what the AI said about a day, and about one minute of it
 *
 * Read from the two `userdata/result_ai_*_summary/` folders, which have two producers (this
 * machine's `windai`, and an outside AI over the bridge), so every paragraph travels with
 * `writtenBy`. Days are the product's own — they roll at the install's `day_begin_minutes`, decided
 * on the Rust side — and an absent file is reported as absent rather than as an empty list.
 * ----------------------------------------------------------------------------------------------- */

/** One product day: its own paragraph, every stretch paragraph filed under it, and the gaps. */
export const daySummaries = (year: number, month: number, day: number) =>
  invoke<DaySummariesDto>("day_summaries", { year, month, day });

/**
 * What the AI said about one row's minute: every stored paragraph whose `[start, end]` window
 * contains it, oldest first. The row is named by its key — `tableKey` is the month file, exactly as
 * `RowKey.file` — because the index decides which stretch a row belongs to, and an empty array is
 * the honest answer for a minute nothing was written about.
 */
export const summaryForKey = (rowid: number, tableKey: string, time: number) =>
  invoke<PeriodSummaryDto[]>("summary_for_key", { rowid, tableKey, time });

/** Unwrap whatever `invoke` rejected into a line a person can act on. */
export function errorMessage(caught: unknown): string {
  if (typeof caught === "string") return caught;
  if (caught instanceof Error) return caught.message;
  try {
    return JSON.stringify(caught);
  } catch {
    return String(caught);
  }
}
