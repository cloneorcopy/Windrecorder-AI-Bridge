/* The wire contract, mirrored from the Rust types this window calls.
 *
 * Every field here is camelCase because both sides of the boundary say so: the commands in
 * `src-tauri/src/commands.rs` carry `#[serde(rename_all = "camelCase")]`, and so do the shared model
 * types in `windui/src/model.rs` that are returned untouched. One casing on the wire is the point —
 * a contract with two conventions is how `pageSize` and `page_size` end up both accepted and one of
 * them silently ignored.
 *
 * These types are *descriptions*, not wrappers: nothing here re-decides a value. `null` means the
 * Rust side said `None`, and the renderer has to cope with that for the same reason the egui view
 * does — a row genuinely can have no window title, no thumbnail, and no segment on disk.
 */

/** `base::clock::LocalParts` — a wall-clock date as the recorder stored it, not an instant. */
export type LocalParts = {
  year: number;
  month: number;
  day: number;
  hour: number;
  minute: number;
  second: number;
};

/** `model::RowKey` — a row is its month file plus its rowid; two months both have a row 12. */
export type RowKey = { file: string; rowid: number };

/** `model::RowCard`, one indexed frame. */
export type RowCard = {
  key: RowKey;
  /** Raw `videofile_time` seconds, as the recorder wrote them. */
  time: number;
  /** `HH:MM:SS`, formatted where it was read. */
  clock: string;
  /** `YYYY-MM-DD`. */
  day: string;
  title: string | null;
  body: string;
  segment: string;
  /** Seconds into the segment; what a player would seek to, and what Locate reports. */
  offset: number | null;
  deepLink: string | null;
  /** Base64 JPEG as stored — no fetch, no decode round trip. */
  thumbnail: string | null;
  /** The segment on disk right now, if it is still there. */
  segmentPath: string | null;
  /** The screenshot this row was indexed from, if that JPEG is still on disk.
   *
   *  Informational here: `frame` is asked for by key, and the Rust side resolves this path itself from the
   *  index rather than reading it back out of what the window sent. A webview that could name the file to
   *  open is a webview that can name any file. */
  picturePath: string | null;
};

export type SearchParams = {
  keywords: string;
  exclude: string;
  from: LocalParts;
  to: LocalParts;
  page: number;
  pageSize: number;
};

export type SearchOutcome = {
  cards: RowCard[];
  total: number;
  pages: number;
  elapsedMs: number;
  params: SearchParams;
  /** The terms the highlighter matched, already expanded through the similar-glyph table. */
  terms: string[];
};

export type BucketCell = { start: number; count: number; label: string };

/** What is waiting for the deferred pass, counted by the steps that would do it
 *  (`windmaint backlog`, passed through by `maintenance_backlog`).
 *
 *  Every number is a dry-run answer from the step's own selector, so it is what that step would pick up
 *  if it ran now — not a guess from file counts. */
export type Backlog = {
  text: { waiting: number; fillable: number; noMaskedCopy: number };
  convert: { slices: number; frames: number; seconds: number };
  reindex: { videos: number };
  expire: { toCompress: number; toDelete: number; rowsToDrop: number; sliceDirsToSweep: number };
  previews: { rows: number; missingSource: number };
  summaries: { stretches: number };
};

/** What the deferred pass last published about itself, read off `cache\locks\LOCK_MAINTAIN\PROGRESS.MD`.
 *
 *  `known` false means the file is missing or unreadable — say "cannot tell", never "nothing is
 *  running". `running` is the pass's own state *and* the process table's answer, so a `windmaint`
 *  killed mid-step cannot leave this window spinning for a dead process. `items` is work finished in
 *  the open step, and zero for a step that does not count it: `reindex` walks the library in a child
 *  process, and no number is invented for it.
 *
 *  The shape a person reads is the four below it: one total bar (`itemsDone` of `itemsTotal`, with
 *  `itemsLeft` still owed) and one row per leg. Counts only — never a fraction of a second, and never
 *  an estimate of how long is left. `step`/`steps`/`stepName` stay because the command line still runs
 *  nine steps and an old pass still publishes them; nothing here paints them. */
export type MaintenanceProgress = {
  known: boolean;
  running: boolean;
  pid: number;
  kind: string;
  state: string;
  step: number;
  steps: number;
  stepName: string;
  items: number;
  elapsedSeconds: number;
  note: string;
  itemsTotal: number;
  itemsDone: number;
  itemsLeft: number;
  legs: MaintenanceLeg[];
};

/** One leg's counter, as the pass itself counted it.
 *
 *  `name` is the writer's word (`text` / `convert` / `ai` / `other`), which is the half of the label
 *  key this window looks up. `state` is that leg's own and not the pass's: `waiting`, `running`,
 *  `done`, `failed`, `offline`. `offline` is an endpoint that has not answered — the ADR's 网络类失败不
 *  算整理失败 — so it draws a muted row and must never redden the pass. `note` is the pass's own
 *  one-line reason, shown as it was written. A leg the census counted at nothing with nothing to say
 *  about itself is not in this list at all. */
export type MaintenanceLeg = {
  name: string;
  done: number;
  total: number;
  state: string;
  note: string;
};

export type StripCell = {
  from: number;
  to: number;
  time: number | null;
  key: RowKey | null;
  thumbnail: string | null;
  /** `HH:MM:SS`, phrased where the row was read.
   *
   *  Do not format `time` in this window to get that: `videofile_time` holds naive-local seconds, and a JS
   *  `Date` reads them as an instant, so the picture lands the zone offset away from the minute it was
   *  taken — which is how the strip came to label a 21:57 frame as 05:57 the next morning while the search
   *  card for the same row said 21:57. */
  clock: string | null;
};

export type FlagNote = {
  when: string;
  note: string;
  time: number | null;
  index: number;
  hasThumbnail: boolean;
};

export type DayOutcome = {
  cards: RowCard[];
  bounds: [number, number];
  buckets: BucketCell[];
  strip: StripCell[];
  stripSpan: [number, number];
  activeHours: number;
  titles: [string, number][];
  flags: FlagNote[];
  unindexedVideo: boolean;
  warnings: string[];
};

export type DayPoint = { day: number; rows: number; hours: number };
export type MonthDayPoint = { month: number; day: number; rows: number };

export type Totals = {
  month: { points: DayPoint[]; rows: number; warnings: string[] };
  year: { points: MonthDayPoint[]; rows: number; warnings: string[] };
};

/** One tile of the month's lightbox. `stamp` is the row's own `YYYY-MM-DD HH:MM:SS`, for the reason
 *  `StripCell.clock` and `FlagNote.when` are formatted strings and not numbers to be phrased here. */
export type LightboxTile = { key: RowKey; time: number; thumbnail: string | null; stamp: string };
export type CloudWord = { text: string; count: number };

/** `commands::PlaySource` — where a row's segment can be played from, asked by row key.
 *
 *  `url` is the only form of address the player is given, and it names no folder: the Rust side resolves
 *  the name inside this install's own videos directory and hands back the custom-scheme URL that resolves
 *  to it. `offset` is the row's seconds into that segment, measured from the index rather than from a card
 *  the window happens to be holding. */
export type PlaySource = {
  name: string;
  url: string;
  offset: number | null;
  /** `h264` | `hevc` | `av1` | `vp9` | `unknown`, read out of the file rather than out of the config that
   *  wrote it — a library holds footage from before whatever the encoder is set to now. */
  codec: string;
  /** The RFC 6381 string to ask `canPlayType` with, or null when no player names this codec. Whether the
   *  webview can decode HEVC is a fact about the machine, so the window asks rather than being told. */
  mime: string | null;
};

/** `commands::About`. */
export type About = { root: string; version: string; months: number };
/** `commands::Stats`. */
export type Stats = { monthsTotal: number; monthsScanned: number; rows: number; error: string | null };
export type DisplayInfo = { index: number; width: number; height: number; primary: boolean };

/** One row of a described form. `kind` decides which control is rendered and nothing else does. */
export type FieldKind = "int" | "fraction" | "bool" | "text" | "lines" | "urbl" | "choice" | "secret";

export type FieldDto = {
  key: string;
  label: string;
  help: string;
  kind: FieldKind;
  min: number | null;
  max: number | null;
  /** The values a picker writes. */
  options: string[];
  /** What each option is called, positionally — a locale is stored as `sc` and read as 简体中文. */
  optionLabels: string[];
  /** A dynamic sentence beside the static help, such as which engines this install refused. */
  note: string | null;
  group: string | null;
  /** number | boolean | string | string[], matching `kind`. `null` for a secret, which never travels. */
  value: number | boolean | string | string[] | null;
  /** One heading per screen the mask row covers — empty for every other kind. */
  panels: string[];
  /** The four edges a mask group edits, named in the row's own language — empty for every other kind. */
  edges: string[];
};

export type FormDto = { page: string; fields: FieldDto[] };

/** What the MCP bridge will do with the settings on disk, as the bridge itself says it: `state` is
 *  already in the window's language, and `refused` is the service's own sentence, word for word. */
export type BridgeStatus = {
  state: string;
  authority: string;
  url: string;
  enabled: boolean;
  listening: boolean;
  refused: string | null;
};

export type AiForm = FormDto & {
  keyState: string;
  ready: boolean;
  verdict: string;
  bridge: BridgeStatus;
};

export type SaveResult = {
  ok: boolean;
  problems: string[];
  written: string | null;
  /** What the save did outside the config file — the sign-in registry entry, above all. */
  notices: string[];
};
export type AiStatus = { ok: boolean; message: string };

/* ---------------------------------------------------------------------------------------------
 * Prompts — the seven templates the product sends, as files the user edits
 *
 * `name` is the handle for all four calls and the string the file is called: `period_summary_system`,
 * `period_summary_user`, `daily_summary_system`, `daily_summary_user`, `tags_system`, `tags_user`,
 * `search_system`. A name outside that set is refused by the Rust side, and the refusal lists them.
 * --------------------------------------------------------------------------------------------- */

/** Which of the two files — or neither — is answering. `embedded` means this install's `config_src`
 *  was moved or deleted and the copy compiled into the binary is what runs: worth saying out loud. */
export type PromptOrigin = "user" | "shipped" | "embedded";

/** `commands::PromptRowDto` — one template as it stands right now. */
export type PromptRowDto = {
  name: string;
  /** What the next request would send. */
  text: string;
  /** What this build ships, so a drifted override is visible rather than silent. */
  shipped: string;
  overridden: boolean;
  origin: PromptOrigin;
  /** The file `text` came from, or the one a save would write. */
  path: string;
  /** An override whose text actually differs from the default it replaced. */
  changed: boolean;
  /** Every `{token}` this template understands. */
  placeholders: string[];
  /** The subset without which the request would carry no material — the validator insists on these. */
  required: string[];
};

/** `commands::PromptFormDto` — `prompts_read`. */
export type PromptFormDto = { prompts: PromptRowDto[] };

/** `commands::PromptOutcomeDto` — `prompt_save` and `prompt_restore`.
 *
 *  Both carry all seven rows as they stand *after* the call, so a page repaints from the answer and
 *  never reloads. A refusal is an answer, not a rejection: `ok: false` with the validator's sentence
 *  and the unchanged rows. */
export type PromptOutcomeDto = {
  ok: boolean;
  /** Verbatim from `wind_base::prompts::validate` — the product's own words about its own
   *  placeholders. Render it as written; do not paraphrase or wrap it in a widget's phrasing. */
  error: string | null;
  /** The file written, on a save that wrote one. `null` for a restore, and for anything refused. */
  savedPath: string | null;
  /** The state `ok` alone cannot tell apart — above all the restore that had nothing to restore,
   *  which says so in its own sentence rather than reading like a restore that changed a file. */
  note: string | null;
  prompts: PromptRowDto[];
};

/** `commands::PromptTrialDto` — `prompt_trial`, one real request built from the text in the box.
 *  Nothing here is stored: not the prompt, not a summary, not the key. */
export type PromptTrialDto = {
  ok: boolean;
  /** Which stretch was asked about, so the line cannot be read as a general claim. */
  segment: string;
  /** Characters of prompt plus screen text that left the machine. */
  chars: number;
  /** The reply, already redacted and clipped by `wind-ai`, or the refusal in its own words.
   *  There is no key in any field of this DTO — and none ever arrives. */
  message: string;
};

/* ---------------------------------------------------------------------------------------------
 * Summaries — what the AI said about a day, and about one minute of it
 *
 * Read from `userdata/result_ai_daily_summary/<product-day>.json` and
 * `userdata/result_ai_period_summary/<product-day>.json`, which have two producers: this machine's
 * `windai`, and an outside AI writing over the bridge. That is why every paragraph carries
 * `writtenBy`, and why a missing file and an unreadable file are different fields.
 *
 * `start`/`end` are naive-local seconds, exactly as `RowCard.time` is. Do not hand them to a JS
 * `Date` to get a clock — it reads them as an instant and lands the zone offset away from the minute
 * they name. `span` is that string already, formatted on the Rust side.
 * --------------------------------------------------------------------------------------------- */

/** Whether a stored paragraph still stands against the index it was written from.
 *
 *  `unindexed` is the honest answer for a file this install holds no stretch for — a fixture, a
 *  segment past `vid_store_day`, a month that was re-indexed. `unknown` means the index could not be
 *  read at all, which is not the same claim. */
export type PeriodState = "current" | "content_changed" | "prompt_changed" | "unindexed" | "unknown";

/** `commands::PeriodSummaryDto` — one recorded stretch's paragraph. */
export type PeriodSummaryDto = {
  /** The segment key: its start stamp, `2026-09-27_15-47-17`. */
  segment: string;
  start: number;
  end: number;
  /** `15:47:17 → 15:50:07`. */
  span: string;
  frames: number;
  text: string;
  textChars: number;
  writtenAt: string;
  /** `windai`, or the label an outside AI sent. `null` means the producer did not say — which is a
   *  different fact from a name, and the screen is allowed to say "unknown producer". */
  writtenBy: string | null;
  state: PeriodState;
  /** Which product day's file this came out of. `summary_for_key` looks back one day, because a
   *  stretch that began at 02:50 yesterday runs into the minute being asked about. */
  day: string;
};

/** `commands::DailySummaryDto` — one day's own paragraph, and what it claims about its coverage. */
export type DailySummaryDto = {
  /** There is a file at this day's path. Distinct from `readable`, and from a file that holds
   *  nothing: three states a screen has to name differently. */
  exists: boolean;
  readable: boolean;
  text: string;
  /** Written over an incomplete day, by the writer's own admission. */
  partial: boolean;
  /** The retention pass flagged it, *or* the day's stretches have moved since it was written.
   *  Which of the two, in words, is in `DaySummariesDto.notes`. */
  stale: boolean;
  writtenBy: string | null;
  writtenAt: string | null;
  segmentsTotal: number;
  segmentsSummarised: number;
  /** The stretches of the day with no standing paragraph, oldest first. */
  missing: string[];
  /** Why it is missing or unreadable, in the reader's words. */
  note: string | null;
};

/** `commands::DaySummariesDto` — `day_summaries`. */
export type DaySummariesDto = {
  /** The product day asked about, `YYYY-MM-DD` — the day that begins at the install's own
   *  `day_begin_minutes`, not the calendar date an instant fell on. */
  date: string;
  daily: DailySummaryDto;
  periods: PeriodSummaryDto[];
  /** The newest day a daily paragraph exists for, when the day asked about has none. Exists so a
   *  screen can label the text as somebody's yesterday instead of passing it off as today's.
   *  `null` when the day asked about answered for itself. */
  fallbackDate: string | null;
  /** Where the three coverage numbers came from: the day's own paragraph (`stored`), a count taken
   *  now from the index (`index`), or neither (`unknown` — the zeroes are then not a count). */
  coverageFrom: "stored" | "index" | "unknown";
  /** Every caveat the numbers need: a month file that would not open, a paragraph whose premise has
   *  moved, a summary file that will not parse. Coverage printed without these is a floor read as a
   *  total, so a screen that has room for one line of them should show them. */
  notes: string[];
};
