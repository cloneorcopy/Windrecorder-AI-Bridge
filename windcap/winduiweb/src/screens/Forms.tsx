import { useCallback, useEffect, useMemo, useState } from "react";
import { KeyRound, RefreshCw, RotateCcw, Save, Zap } from "lucide-react";
import {
  aiRead,
  aiSave,
  aiTest,
  errorMessage,
  maintenanceBacklog,
  maintenanceProgress,
  maintenanceStart,
  maintenanceStop,
  promptRestore,
  promptsRead,
  promptSave,
  promptTrial,
  recordingRead,
  recordingSave,
  settingsRead,
  settingsSave,
  type AiDraftInput,
  type FormPage,
} from "../services/api";
import type { Backlog, BridgeStatus, FieldDto, MaintenanceLeg, MaintenanceProgress, PromptOrigin, PromptOutcomeDto, PromptRowDto, PromptTrialDto, SaveResult } from "../types";
import { Button, Empty, ErrorBanner, Field, Panel, Pill, SectionTitle, cn, inputClass, type Tone } from "../components/ui";

type Tr = (key: string, args?: Record<string, string | number>) => string;
type Say = (text: string, tone?: "ok" | "bad") => void;
type Page = FormPage | "ai";

/** What the three form pages have in common, plus the two answers only the AI page carries.
 *  The read functions return different types; this is the one the renderer speaks. */
type FormView = { fields: FieldDto[]; keyState?: string; ready?: boolean; verdict?: string; bridge?: BridgeStatus };

/** The stored value as the text box the draft expects.
 *
 *  Every form on the Rust side holds what was *typed*, not what it parses to — "7" with the last digit
 *  deleted is not 7 — so the boundary carries strings for every kind, including the booleans and the
 *  line lists. This is the one place that turns a parsed value back into that text.
 */
function asText(value: FieldDto["value"]): string {
  if (value === null || value === undefined) return "";
  if (Array.isArray(value)) return value.join("\n");
  if (typeof value === "boolean") return value ? "true" : "false";
  return String(value);
}

export function FormScreen({ page, tr, say, relabel }: { page: Page; tr: Tr; say: Say; relabel?: () => Promise<void> }) {
  const [form, setForm] = useState<FormView | null>(null);
  const [values, setValues] = useState<Record<string, string>>({});
  const [clearKey, setClearKey] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [testing, setTesting] = useState(false);
  const [result, setResult] = useState<SaveResult | null>(null);

  /** What the deferred pass has published about itself, and why the two are kept apart.
   *
   *  `progressError` is not the same fact as `!progress.known`: the first is this window failing to
   *  read, the second is the install having nothing to read. Collapsing them would let a broken IPC call
   *  render as "nothing has organised this library yet", which is the exact shape this product has been
   *  burned by — a front end quietly healthy over a backend that failed. */
  const [progress, setProgress] = useState<MaintenanceProgress | null>(null);
  const [progressError, setProgressError] = useState<string | null>(null);

  /** Ask the recorder to start or stop the deferred pass.
   *
   *  The answer is the request file, not "done" — and saying so is still the point, because the recorder
   *  honours it on its next tick and a running pass finishes the file it has open before it stops. What
   *  the rail below adds is not a spinner for that handoff but the pass's own report once it is under
   *  way: which of its nine steps is open, how much of it is done, and how the last one ended. */
  const askMaintenance = async (which: "start" | "stop") => {
    try {
      const path = which === "start" ? await maintenanceStart() : await maintenanceStop();
      say(`${which === "start" ? "queued" : "stop requested"} — ${path}`, "ok");
    } catch (caught) {
      say(errorMessage(caught), "bad");
    }
  };

  /** Read the pass's published state while this page is up, and stop reading when it is not.
   *
   *  Polled rather than pushed because the pass is a child of the recorder, not of this window: there is
   *  no channel to it, and inventing one would be a second thing to keep alive. Two seconds is the
   *  writer's own publish floor (`wind_base::maintain::PUBLISH_EVERY`), so a refresh never shows a
   *  number staler than the last one it asked for. */
  useEffect(() => {
    if (page !== "settings") return;
    let alive = true;
    const ask = () =>
      maintenanceProgress()
        .then((answer) => {
          if (!alive) return;
          setProgress(answer);
          setProgressError(null);
        })
        .catch((caught) => {
          if (!alive) return;
          setProgressError(errorMessage(caught));
        });
    ask();
    const timer = setInterval(ask, 2000);
    return () => {
      alive = false;
      clearInterval(timer);
    };
  }, [page]);

  const load = useCallback(async () => {
    setError(null);
    try {
      const read: FormView = page === "settings" ? await settingsRead() : page === "recording" ? await recordingRead() : await aiRead();
      setForm(read);
      setValues(Object.fromEntries(read.fields.map((field) => [field.key, asText(field.value)])));
    } catch (caught) {
      setError(errorMessage(caught));
    }
  }, [page]);

  useEffect(() => {
    void load();
  }, [load]);

  const grouped = useMemo(() => {
    const buckets = new Map<string, FieldDto[]>();
    for (const field of form?.fields ?? []) {
      const key = field.group ?? "";
      buckets.set(key, [...(buckets.get(key) ?? []), field]);
    }
    return [...buckets.entries()];
  }, [form]);

  const save = useCallback(async () => {
    setBusy(true);
    setResult(null);
    try {
      const outcome = page === "settings" ? await settingsSave(values) : page === "recording" ? await recordingSave(values) : await aiSave(values, clearKey);
      setResult(outcome);
      if (outcome.ok) {
        // The Settings page owns `lang`, the one key that changes what this window *says*. Re-reading the
        // copy is what makes the choice land in the click that made it rather than at the next launch;
        // `load()` then re-renders the rows, whose labels the server already resolved in the new locale.
        if (page === "settings" && relabel) await relabel();
        say(tr("windui_web_saved", { path: outcome.written ?? "" }));
        void load();
        // The bridge row is a fact about a process the tray starts on its own timer, so the re-read
        // immediately above is taken a moment too early: switch the service on and the page would
        // answer "not running" about a port that opens a second later. One more look, after the tray
        // has had its turn.
        if (page === "ai") window.setTimeout(() => void load(), 1600);
      } else {
        say(tr("windui_web_save_blocked", { count: outcome.problems.length }), "bad");
      }
    } catch (caught) {
      say(errorMessage(caught), "bad");
    } finally {
      setBusy(false);
    }
  }, [page, values, clearKey, say, tr, load, relabel]);

  const test = useCallback(async () => {
    setTesting(true);
    try {
      const report = await aiTest(values, clearKey);
      say(report.message, report.ok ? "ok" : "bad");
    } catch (caught) {
      say(errorMessage(caught), "bad");
    } finally {
      setTesting(false);
    }
  }, [values, clearKey, say]);

  const titleKey = page === "settings" ? "windui_web_settings_title" : page === "recording" ? "windui_web_recording_title" : "windui_web_ai_title";
  const detailKey = page === "settings" ? "windui_web_settings_detail" : page === "recording" ? "windui_web_recording_detail" : "windui_web_ai_detail";

  /** The AI page's own draft, handed to a prompt trial so a key typed but not saved is the one the trial
   *  uses — the same fold `ai_test` runs, so the two buttons cannot disagree about the endpoint. */
  const aiDraft = useMemo<AiDraftInput>(() => ({ values, clearKey }), [values, clearKey]);

  if (error) return <ErrorBanner message={error} onRetry={() => void load()} tr={tr} />;
  if (!form) return <Panel className="px-6 py-14 text-center text-xs text-slate-500">{tr("windui_web_loading")}…</Panel>;

  return (
    <div className="space-y-5">
      <Panel className="flex flex-wrap items-center justify-between gap-4 p-4">
        <SectionTitle title={tr(titleKey)} detail={tr(detailKey)} />
        <div className="flex items-center gap-2">
          {page === "ai" ? (
            <>
              <Pill tone={form.ready ? "emerald" : "amber"}>
                <KeyRound className="h-3 w-3" />
                {form.keyState}
              </Pill>
              <Button variant="ghost" busy={testing} onClick={() => void test()}>
                <Zap className="h-3.5 w-3.5" />
                {testing ? tr("windui_web_ai_testing") : tr("windui_web_ai_test")}
              </Button>
            </>
          ) : null}
          <Button variant="primary" busy={busy} onClick={() => void save()}>
            <Save className="h-3.5 w-3.5" />
            {busy ? tr("windui_web_saving") : tr("windui_web_save")}
          </Button>
        </div>
      </Panel>

      {page === "ai" && !form.ready ? (
        <Panel className="border-amber-500/25 bg-amber-500/[0.06] px-4 py-3 text-xs leading-relaxed text-amber-200/80">{form.verdict}</Panel>
      ) : null}

      {/* The bridge's own answer, on the page that edits its five keys. Which port it listens on is the
          one setting a wrong value makes invisible — an assistant just gets nothing back — so the row
          names the address, says whether anything is answering it this second, and carries the URL to
          paste. `state` and `refused` are the service's words, resolved server-side like every label. */}
      {page === "ai" && form.bridge ? (
        <Panel className="space-y-1 px-4 py-3 text-[11px] leading-relaxed">
          <div className="flex flex-wrap items-center gap-x-3 gap-y-1">
            <span className="font-medium text-slate-300">{tr("windui_web_bridge_title")}</span>
            <span className={cn("font-mono", form.bridge.listening ? "text-emerald-300" : "text-amber-300")}>
              {form.bridge.state}
            </span>
            {form.bridge.enabled ? (
              <>
                <span className="selectable font-mono text-slate-400">{form.bridge.authority}</span>
                <span className="selectable break-all font-mono text-slate-500" title={tr("windui_web_bridge_url")}>
                  {form.bridge.url}
                </span>
              </>
            ) : null}
          </div>
          {form.bridge.refused ? <p className="selectable text-rose-200/80">{form.bridge.refused}</p> : null}
        </Panel>
      ) : null}

      {result && !result.ok ? (
        <Panel className="border-rose-500/30 bg-rose-500/[0.06] p-4">
          <p className="mb-2 text-xs font-medium text-rose-200">{tr("windui_web_save_blocked", { count: result.problems.length })}</p>
          <ul className="selectable list-disc space-y-1 pl-5 text-[11px] leading-relaxed text-rose-200/70">
            {result.problems.map((one) => (
              <li key={one}>{one}</li>
            ))}
          </ul>
        </Panel>
      ) : null}

      {/* What the save did that the config file cannot hold — the sign-in entry. Said after a successful
          write, and said even when it failed, because a checkbox the machine did not obey has to be
          visible rather than assumed. */}
      {result?.ok && result.notices.length > 0 ? (
        <Panel className="border-sky-500/25 bg-sky-500/[0.06] px-4 py-3 text-[11px] leading-relaxed text-sky-100/80">
          {result.notices.map((one) => (
            <p key={one} className="selectable">
              {one}
            </p>
          ))}
        </Panel>
      ) : null}

      <div className="space-y-5">
        {grouped.map(([group, fields]) => (
          <Panel key={group || "main"} className="p-4">
            {group ? <p className="mb-3 text-[11px] uppercase tracking-wide text-slate-500">{group}</p> : null}
            <div className="grid grid-cols-1 gap-4 md:grid-cols-2">
              {fields.map((field) => (
                <FieldRow
                  key={field.key}
                  field={field}
                  value={values[field.key] ?? ""}
                  tr={tr}
                  onChange={(next) => setValues((all) => ({ ...all, [field.key]: next }))}
                />
              ))}
            </div>
          </Panel>
        ))}

        {/* The two buttons that do not need a saved setting to work: one starts the backlog now, the
            other calls it off. They ask the recorder rather than doing the work here, because only the
            process holding the record lock knows whether a pass is already running. */}
        {page === "settings" ? (
          <Panel className="space-y-3 p-4">
            <div className="flex flex-wrap items-center gap-3">
              <Button variant="primary" onClick={() => void askMaintenance("start")}>
                {tr("windui_web_maintain_now")}
              </Button>
              <Button onClick={() => void askMaintenance("stop")}>
                {tr("windui_web_maintain_stop")}
              </Button>
              <span className="min-w-0 flex-1 text-[11px] leading-relaxed text-slate-500">
                {tr("windui_web_maintain_help")}
              </span>
            </div>
            <MaintenanceRail tr={tr} progress={progress} error={progressError} />
            <BacklogPanel tr={tr} />
          </Panel>
        ) : null}

        {page === "ai" ? (
          <Panel className="p-4">
            <label className="flex items-start gap-3">
              <input
                type="checkbox"
                checked={clearKey}
                onChange={(event) => setClearKey(event.target.checked)}
                className="mt-0.5 h-4 w-4 shrink-0 accent-rose-500"
              />
              <span className="text-xs leading-relaxed text-slate-400">
                {tr("windui_web_ai_clear_key")}
                <span className="mt-0.5 block text-[11px] text-slate-600">{tr("windui_web_ai_key_kept")}</span>
              </span>
            </label>
          </Panel>
        ) : null}
      </div>

      {/* The seven templates, on the page that already holds the endpoint they are sent to. */}
      {page === "ai" ? <PromptsPanel tr={tr} input={aiDraft} /> : null}
    </div>
  );
}

/** How much there is to organise, on request.
 *
 *  A button rather than an automatic read, because the census walks the library — seconds on a big one —
 *  and the settings page is opened for other reasons than deciding whether to press 立刻整理. The numbers
 *  are each step's own dry-run answer (`windmaint backlog`), so they are what that step would pick up if
 *  it ran now; the window does arithmetic of its own about the library, and could not be trusted to.
 *
 *  Zero is shown as zero. A census that hides the empty lines reads like a feature that quietly stopped
 *  reporting, and "nothing to compress" is a fact worth seeing next to "1063 rows to redraw".
 */
function BacklogPanel({ tr }: { tr: Tr }) {
  const [data, setData] = useState<Backlog | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const count = async () => {
    setBusy(true);
    setError(null);
    try {
      setData(await maintenanceBacklog());
    } catch (caught) {
      setError(errorMessage(caught));
    } finally {
      setBusy(false);
    }
  };

  const lines = () => {
    if (!data) return [];
    return [
      tr("windui_web_backlog_text", { n: data.text.waiting }),
      ...(data.text.noMaskedCopy > 0 ? [tr("windui_web_backlog_nomask", { n: data.text.noMaskedCopy })] : []),
      tr("windui_web_backlog_convert", { n: data.convert.slices, f: data.convert.frames }),
      tr("windui_web_backlog_reindex", { n: data.reindex.videos }),
      tr("windui_web_backlog_expire", { n: data.expire.toCompress, d: data.expire.toDelete }),
      tr("windui_web_backlog_previews", { n: data.previews.rows }),
      tr("windui_web_backlog_summaries", { n: data.summaries.stretches }),
    ];
  };

  return (
    <div className="space-y-2 border-t border-slate-800 pt-3">
      <div className="flex flex-wrap items-center gap-3">
        <Button onClick={() => void count()} disabled={busy}>
          {busy ? tr("windui_web_backlog_counting") : tr("windui_web_backlog_run")}
        </Button>
        <span className="min-w-0 flex-1 text-[11px] leading-relaxed text-slate-500">{tr("windui_web_backlog_note")}</span>
      </div>
      {error ? <p className="selectable text-[11px] leading-relaxed text-rose-200/80">{tr("windui_web_backlog_failed", { error })}</p> : null}
      {data && !busy ? (
        <ul className="selectable space-y-1 text-[11px] leading-relaxed text-slate-400">
          {lines().map((line) => (
            <li key={line}>{line}</li>
          ))}
        </ul>
      ) : null}
    </div>
  );
}

/** The four legs the pass counts itself in, in the order `wind_base::maintain::Leg::ALL` writes them —
 *  which is the order `docs/adr/2026-09-30-the-organise-pass-runs-on-four-legs.md` §一 draws them.
 *
 *  The nine steps are not here on purpose: they are still what `windmaint` runs, and still what the
 *  command line answers with, but the ADR took them away as *the shape a person is shown*. A row is a
 *  leg now, and a leg is something the owner can say what it is waiting for. */
const PASS_LEGS = ["text", "convert", "ai", "other"];

/** 文字识别 / 视频合成 / AI 总结 / 其他整理 — plain verb phrases, one per leg. A leg a newer `windmaint`
 *  invented has no copy yet, so it is named by the word the file itself uses rather than painted with
 *  the catalog's not-found marker: a bar whose label is a bug report is worse than a bar labelled `markdown`. */
function legLabel(tr: Tr, name: string): string {
  return PASS_LEGS.includes(name) ? tr(`windui_web_leg_${name}`) : name;
}

/** What one item of this leg *is*: a picture, a segment, a stretch, an item. A numerator over a
 *  denominator is only a sentence when the unit is named, and the four units are the four the census
 *  counts — `text` rows, slices handed to ffmpeg, stretches with no summary, and everything else — so a
 *  row can never promise work in one unit and deliver it in another.
 */
function legUnit(tr: Tr, name: string): string {
  return PASS_LEGS.includes(name) ? tr(`windui_web_leg_${name}_unit`) : "";
}

/** How full a bar is, out of the two counts the pass published.
 *
 *  Items over items — never seconds, and never an estimate of what is left. Clamped because a pass may
 *  handle more than its census counted (work the recorder added after the count belongs to the next
 *  pass, and a bar that recedes is the lie the ADR names), and `total` of zero is a leg with no queue,
 *  which is drawn full only when it really did the work. */
function barShare(done: number, total: number): number {
  if (total <= 0) return done > 0 ? 100 : 0;
  return Math.min(100, Math.round((done / total) * 100));
}

/** One item counter: a thin bar, `已干完 3700 / 4200 张画面`, and the one-line reason if this leg has one.
 *
 *  Colour is the only other thing this row is allowed to say, and it says it about itself: red is local
 *  trouble (this machine could not do its own work), amber is an endpoint that has not answered, and an
 *  amber row is not a failed pass — the ADR is explicit that 网络类失败不算整理失败, and `wind_base`'s
 *  `LegStatus::fails_the_pass` is where that was written down. The pass's own state is drawn nowhere
 *  here, so a quiet AI leg cannot redden the total bar above it.
 *
 *  `live` is the one thing the row borrows from the pass above it, and it governs the pulse only. A leg is
 *  left `running` in the file when a pass is called off or dies mid-request — the publisher stops writing,
 *  so nothing will ever say the polite end of that sentence — and a bar that keeps breathing under
 *  "上一轮在第 8 步后收手" is a window claiming work that stopped hours ago. `running` in the file is not
 *  enough: this is the same word the total bar above already reads, answered against the process table, so
 *  a corpse cannot pulse either. */
function LegRow({ tr, leg, live }: { tr: Tr; leg: MaintenanceLeg; live: boolean }) {
  const failed = leg.state === "failed";
  const offline = leg.state === "offline";
  const left = leg.total > leg.done ? leg.total - leg.done : 0;
  const tail =
    failed || offline
      ? leg.note
      : leg.state === "waiting"
        ? tr("windui_web_maintain_waiting")
        : leg.state === "done"
          ? tr("windui_web_maintain_leg_done")
          : left > 0
            ? tr("windui_web_maintain_left", { n: left })
            : "";

  return (
    <div className="flex flex-wrap items-baseline gap-x-2 gap-y-0.5 text-[11px] leading-relaxed text-slate-400">
      <span className="w-[104px] shrink-0 truncate text-slate-300" title={legLabel(tr, leg.name)}>
        {legLabel(tr, leg.name)}
      </span>
      <span
        role="progressbar"
        aria-label={legLabel(tr, leg.name)}
        aria-valuemin={0}
        aria-valuemax={leg.total}
        aria-valuenow={leg.done}
        className="h-1.5 w-28 shrink-0 overflow-hidden rounded-full bg-slate-700"
      >
        <span
          className={cn(
            "block h-full rounded-full transition-[width] duration-500",
            failed ? "bg-rose-500/80" : offline ? "bg-amber-400/70" : leg.state === "done" ? "bg-emerald-500/70" : "bg-sky-400",
            leg.state === "running" && live ? "animate-pulse" : "",
          )}
          style={{ width: `${barShare(leg.done, leg.total)}%` }}
        />
      </span>
      <span className="selectable shrink-0 tabular-nums">
        {tr("windui_web_maintain_leg_count", { done: leg.done, total: leg.total, unit: legUnit(tr, leg.name) })}
      </span>
      <span className={cn("min-w-0 flex-1 break-words", failed ? "text-rose-200/80" : offline ? "text-amber-200/70" : "text-slate-500")}>{tail}</span>
    </div>
  );
}

/** What the deferred pass has said about itself, painted as one total bar, one bar per leg, and one
 *  sentence about how the pass itself ended.
 *
 *  Counts of items and nothing else: no step numbers, and no time — the owner refused both an estimate
 *  and any percentage derived from seconds, and the denominators the pass fixed when it started are the
 *  only denominator this draws. Four endings are still four different sentences (`complete`, `stopped`,
 *  `failed`, and a pass that vanished without writing one), because they are four things a person needs
 *  to know they have. The pass's own closing note is shown as it was written: it is the product's
 *  sentence about its own run, not this window's copy. */
function MaintenanceRail({ tr, progress, error }: { tr: Tr; progress: MaintenanceProgress | null; error: string | null }) {
  if (error) {
    return <p className="selectable text-[11px] leading-relaxed text-rose-200/80">{error}</p>;
  }
  if (!progress || !progress.known) {
    return <p className="text-[11px] leading-relaxed text-slate-500">{tr("windui_web_maintain_none")}</p>;
  }

  const crashed = !progress.running && progress.state === "running";

  const sentence = progress.running
    ? tr(progress.kind === "manual" ? "windui_web_maintain_manual" : "windui_web_maintain_scheduled")
    : progress.state === "complete"
      ? tr("windui_web_maintain_complete", { total: progress.itemsTotal })
      : progress.state === "stopped"
        ? tr("windui_web_maintain_stopped", { left: progress.itemsLeft })
        : progress.state === "failed"
          ? tr("windui_web_maintain_failed")
          : tr("windui_web_maintain_crashed");

  return (
    <div className="space-y-1.5">
      {/* 总量: the pass's own bar, which is the sum of the legs and nothing else — so it cannot disagree
          with the rows under it, and so an amber leg leaves it the colour of a pass that is working. */}
      <div className={cn("flex flex-wrap items-baseline gap-x-2 gap-y-0.5 text-[11px] leading-relaxed", progress.running ? "" : "opacity-90")}>
        <span className="w-[104px] shrink-0 truncate text-slate-200">{tr("windui_web_maintain_total_label")}</span>
        <span
          role="progressbar"
          aria-label={tr("windui_web_maintain_total_label")}
          aria-valuemin={0}
          aria-valuemax={progress.itemsTotal}
          aria-valuenow={Math.min(progress.itemsDone, progress.itemsTotal)}
          className="h-2 w-28 shrink-0 overflow-hidden rounded-full bg-slate-700"
        >
          <span
            className={cn(
              "block h-full rounded-full transition-[width] duration-500",
              // Red only for the pass's own local failure; a pass that stopped or finished is not red.
              progress.state === "failed" ? "bg-rose-500/80" : crashed ? "bg-amber-400/70" : "bg-sky-500",
              progress.running ? "animate-pulse" : "",
            )}
            style={{ width: `${barShare(progress.itemsDone, progress.itemsTotal)}%` }}
          />
        </span>
        <span className="selectable shrink-0 tabular-nums text-slate-300">
          {tr("windui_web_maintain_total", { done: progress.itemsDone, total: progress.itemsTotal })}
        </span>
        <span className={cn("min-w-0 flex-1 break-words", progress.state === "failed" ? "text-rose-200/80" : "text-slate-500")}>{sentence}</span>
      </div>

      {/* The leg rows. The backend already dropped a leg the census counted at nothing, so nothing here
          is a bar promising a queue nobody counted — the rule is stated once, in `leg_rows`. */}
      {progress.legs.map((leg) => (
        <LegRow key={leg.name} tr={tr} leg={leg} live={progress.running} />
      ))}

      {progress.note && !progress.running ? <p className="selectable break-all font-mono text-[11px] text-slate-500">{progress.note}</p> : null}
    </div>
  );
}

/** The four edges of a crop-mask group, in the order the config stores one group's four numbers.
 *  `settings_read` normally sends these names already resolved in `field.edges`; this list is the same
 *  four catalog rows for the row that reports none, so both doors say the same thing in the same
 *  language rather than one of them falling back to English. */
const EDGE_KEYS = ["set_text_top_padding", "set_text_right_padding", "set_text_bottom_padding", "set_text_left_padding"];

/** One described field. The renderer branches on `kind` and nothing else — a control that appears here
 *  is a key the engine reads, because the list it renders is the same list `validate` walks.
 *
 *  `tr` is a prop rather than a captured module map because the crop-mask control below names four
 *  directions on screen: which edge of which panel a slider hides is a sentence, and a Chinese install
 *  that reads "Left" has no way to know whether it is being told about the left or the right. */
function FieldRow({ field, value, tr, onChange }: { field: FieldDto; value: string; tr: Tr; onChange: (next: string) => void }) {
  const bound =
    field.min !== null && field.max !== null
      ? `${field.min} – ${field.max}`
      : field.max !== null
        ? `≤ ${field.max}`
        : null;

  const control = () => {
    switch (field.kind) {
      case "bool":
        return (
          <button
            type="button"
            onClick={() => onChange(value === "true" ? "false" : "true")}
            className={cn(
              // `shrink-0` because a flex row squeezes this button to 17 px on a narrow window: it has no
              // in-flow content, so its min-content is nothing and the label's `min-w-0` alone does not
              // protect it. The absolutely positioned knob then sits outside the pill it belongs to.
              "relative h-6 w-11 shrink-0 rounded-full border transition-colors duration-200",
              value === "true" ? "border-indigo-500/50 bg-indigo-500/30" : "border-slate-700 bg-slate-800/70",
            )}
          >
            <span
              className={cn(
                "absolute top-0.5 h-4.5 w-4.5 rounded-full bg-white shadow transition-all duration-200",
                value === "true" ? "left-[22px]" : "left-0.5",
              )}
            />
          </button>
        );
      case "lines":
        return (
          <textarea
            rows={4}
            className={cn(inputClass, "selectable font-mono text-[11px] leading-relaxed")}
            value={value}
            onChange={(event) => onChange(event.target.value)}
          />
        );
      case "urbl": {
        // The value is the same comma-separated list the native window's draft holds and the config
        // stores, so one save path serves both doors. One row per screen the machine reports: a mask
        // that covers only the first panel on a four-panel desk is a privacy setting that lies.
        const numbers = value.split(/[\s,]+/).filter((token) => token !== "").map(Number);
        const groups = Math.max(field.panels.length, 1);
        const edges = field.edges.length === 4 ? field.edges : EDGE_KEYS.map((key) => tr(key));
        const ceiling = field.max ?? 40;
        const at = (index: number) => (Number.isFinite(numbers[index]) ? numbers[index] : index % 4 === 3 ? 3 : 6);
        const write = (index: number, next: number) => {
          const copy = Array.from({ length: groups * 4 }, (_, position) => at(position));
          copy[index] = Math.min(Math.max(0, Number.isFinite(next) ? next : 0), ceiling);
          onChange(copy.join(", "));
        };
        return (
          <div className="space-y-2">
            {Array.from({ length: groups }, (_, panel) => (
              <div key={panel} className="flex flex-wrap items-center gap-x-3 gap-y-1">
                <span className="selectable w-[124px] shrink-0 font-mono text-[10px] text-slate-500">
                  {field.panels[panel] ?? `#${panel + 1}`}
                </span>
                {edges.map((edge, slot) => (
                  <label key={edge} className="flex items-center gap-1.5 text-[11px] text-slate-500">
                    {edge}
                    <input
                      type="number"
                      min={0}
                      max={ceiling}
                      className={cn(inputClass, "w-[70px]")}
                      value={at(panel * 4 + slot)}
                      onChange={(event) => write(panel * 4 + slot, Number(event.target.value))}
                    />
                  </label>
                ))}
              </div>
            ))}
          </div>
        );
      }
      case "choice":
        return (
          <select className={inputClass} value={value} onChange={(event) => onChange(event.target.value)}>
            {!field.options.includes(value) && value !== "" ? <option value={value}>{value}</option> : null}
            {field.options.map((option, index) => (
              <option key={option} value={option}>
                {field.optionLabels?.[index] ?? option}
              </option>
            ))}
          </select>
        );
      case "secret":
        return (
          <input
            type="password"
            autoComplete="off"
            placeholder="••••••••"
            className={inputClass}
            value={value}
            onChange={(event) => onChange(event.target.value)}
          />
        );
      case "int":
      case "fraction":
        return (
          <input
            type="number"
            step={field.kind === "fraction" ? "0.05" : "1"}
            className={inputClass}
            value={value}
            onChange={(event) => onChange(event.target.value)}
          />
        );
      default:
        return <input className={inputClass} value={value} onChange={(event) => onChange(event.target.value)} />;
    }
  };

  return (
    <div className={cn("space-y-1.5", field.kind === "bool" && "flex items-center justify-between gap-4")}>
      {field.kind === "bool" ? (
        <>
          <div className="min-w-0">
            <p className="truncate text-xs font-medium text-slate-200">{field.label}</p>
            <p className="text-[11px] leading-relaxed text-slate-500">{field.help}</p>
          </div>
          {control()}
        </>
      ) : (
        <>
          <Field label={field.label} hint={`${field.help}${bound ? ` · ${bound}` : ""}`}>
            {control()}
          </Field>
          <p className="selectable truncate text-[10px] text-slate-600" title={field.key}>
            {field.key}
          </p>
        </>
      )}
      {/* What this install refused, in its own words: an engine that used to need Python, or a locale the
          catalog has no table for. Both are named by the server, so neither can be a silently absent option. */}
      {field.note ? <p className="text-[11px] leading-relaxed text-amber-300/90">{field.note}</p> : null}
    </div>
  );
}

/* -----------------------------------------------------------------------------------------------
 * Prompts — the seven templates the product sends, edited where they run
 *
 * Two rules carry this section, and both are the page's reason for existing. The box holds the
 * effective text rather than a copy of it, because a settings screen that shows a *copy* lets a person
 * edit something the engine would never send; and every write answers with all seven rows as they
 * stand afterwards, so the page paints from the answer it was given instead of asking again — the
 * posture `save` above takes with `result`, taken further, since a prompt's answer carries the rows
 * themselves and not just a verdict. There is deliberately no "custom prompt on/off" switch and no
 * template picker: `wind_base::prompts` decides that a file at `userdata/ai_prompts/<name>.txt` wins
 * by existing, Save writes it, Restore deletes it, and the only difference shown here is between the
 * box and the file.
 * ----------------------------------------------------------------------------------------------- */

/** What the last write to one template answered. `error` is the validator's own sentence, kept exactly
 *  as it arrived: it names the offending `{token}` and the one they meant, and paraphrasing it would
 *  put the window's words in front of the product's. */
type PromptReply = { error: string | null; note: string | null; savedPath: string | null };

/** The rows as the last answer left them, plus the box each one is being edited in. Held together, so a
 *  draft can never outlive the row it was typed against. */
type PromptView = { rows: PromptRowDto[]; drafts: Record<string, string> };

type PromptAction = "save" | "restore" | "trial";

/** Which copy is in force, and how loud it should be. `embedded` is the one that must not look normal:
 *  an install whose `config_src` went missing answers from the copy compiled into the binary. */
const ORIGINS: Record<PromptOrigin, { key: string; tone: Tone }> = {
  user: { key: "ai.prompts.origin.user", tone: "indigo" },
  shipped: { key: "ai.prompts.origin.shipped", tone: "slate" },
  embedded: { key: "ai.prompts.origin.embedded", tone: "rose" },
};

/** Forget what one row said, because the row has moved since it said it. */
function drop<T>(all: Record<string, T>, name: string): Record<string, T> {
  if (!(name in all)) return all;
  const rest = { ...all };
  delete rest[name];
  return rest;
}

function PromptsPanel({ tr, input }: { tr: Tr; input: AiDraftInput }) {
  const [view, setView] = useState<PromptView | null>(null);
  const [replies, setReplies] = useState<Record<string, PromptReply>>({});
  const [trials, setTrials] = useState<Record<string, PromptTrialDto>>({});
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState<{ name: string; kind: PromptAction } | null>(null);

  /** Take the seven rows an answer carried without eating an edit that is still in progress.
   *
   *  The template just written comes back holding exactly what was typed into it, so it settles into
   *  "saved" on its own. A box that still differs from the text *its own row* carried when this answer
   *  arrived is typing on some other template, and a save on this one has no business replacing it — it
   *  stays, and the row's own `unsaved` mark says it is not on disk yet. */
  const paint = useCallback((next: PromptRowDto[]) => {
    setView((current) => ({
      rows: next,
      drafts: Object.fromEntries(
        next.map((row) => {
          const box = current?.drafts[row.name];
          const inForce = current?.rows.find((one) => one.name === row.name)?.text;
          return [row.name, box !== undefined && inForce !== undefined && box !== inForce ? box : row.text];
        }),
      ),
    }));
  }, []);

  const load = useCallback(async () => {
    setError(null);
    try {
      paint((await promptsRead()).prompts);
    } catch (caught) {
      setError(errorMessage(caught));
    }
  }, [paint]);

  useEffect(() => {
    void load();
  }, [load]);

  /** A keystroke. Both lines a write leaves behind stop being true the moment the box moves: one
   *  describes the file as it was saved, the other answers text that is no longer on the screen. */
  const edit = useCallback((name: string, text: string) => {
    setView((current) => (current ? { ...current, drafts: { ...current.drafts, [name]: text } } : current));
    setReplies((all) => drop(all, name));
    setTrials((all) => drop(all, name));
  }, []);

  /** The one shape a write takes: send it, then paint from the rows it carries back. A refusal is an
   *  ordinary answer rather than a rejection — the rows are unchanged and the sentence is the product's
   *  — so the row keeps the text, and the reason it is not on disk. */
  const write = useCallback(
    async (name: string, kind: "save" | "restore", call: () => Promise<PromptOutcomeDto>) => {
      setBusy({ name, kind });
      try {
        const outcome = await call();
        paint(outcome.prompts);
        setReplies((all) => ({ ...all, [name]: { error: outcome.error, note: outcome.note, savedPath: outcome.savedPath } }));
      } catch (caught) {
        setReplies((all) => ({ ...all, [name]: { error: errorMessage(caught), note: null, savedPath: null } }));
      } finally {
        setBusy(null);
      }
    },
    [paint],
  );

  /** One real request, built from this box and not from the file, against the endpoint this page's own
   *  draft holds. Nothing here is stored, which is the whole reason the button can sit beside a box of
   *  unsaved words. */
  const runTrial = useCallback(
    async (name: string, text: string) => {
      setBusy({ name, kind: "trial" });
      try {
        const report = await promptTrial(name, text, input);
        setTrials((all) => ({ ...all, [name]: report }));
      } catch (caught) {
        setTrials((all) => ({ ...all, [name]: { ok: false, segment: "", chars: 0, message: errorMessage(caught) } }));
      } finally {
        setBusy(null);
      }
    },
    [input],
  );

  if (error) return <ErrorBanner message={error} onRetry={() => void load()} tr={tr} />;
  if (!view) return <Panel className="px-6 py-14 text-center text-xs text-slate-500">{tr("windui_web_loading")}…</Panel>;
  if (view.rows.length === 0) return <Empty title={tr("ai.prompts.none_title")} detail={tr("ai.prompts.none_detail")} />;

  return (
    <div className="space-y-5">
      <Panel className="flex flex-wrap items-center justify-between gap-4 p-4">
        <SectionTitle title={tr("ai.prompts.title")} detail={tr("ai.prompts.detail")} />
        {/* Worth one button, because these are files: `windai prompts`, the bridge and this window all
            write them, and the page would otherwise show the state the window happened to open on. */}
        <Button variant="ghost" disabled={busy !== null} onClick={() => void load()}>
          <RefreshCw className="h-3.5 w-3.5" />
          {tr("windui_footer_refresh")}
        </Button>
      </Panel>

      {/* Said once for the page rather than seven times down it, because on an install that lost its
          `config_src` all seven are true at once. */}
      {view.rows.some((row) => row.origin === "embedded") ? (
        <Panel className="border-rose-500/30 bg-rose-500/[0.06] px-4 py-3 text-[11px] leading-relaxed text-rose-200/80">
          {tr("ai.prompts.embedded_note")}
        </Panel>
      ) : null}

      {view.rows.map((row) => (
        <PromptEditor
          key={row.name}
          row={row}
          draft={view.drafts[row.name] ?? row.text}
          reply={replies[row.name] ?? null}
          trial={trials[row.name] ?? null}
          action={busy?.name === row.name ? busy.kind : null}
          asking={busy?.kind === "trial"}
          tr={tr}
          onEdit={(text) => edit(row.name, text)}
          onSave={(text) => void write(row.name, "save", () => promptSave(row.name, text))}
          onRestore={() => void write(row.name, "restore", () => promptRestore(row.name))}
          onTrial={(text) => void runTrial(row.name, text)}
        />
      ))}
    </div>
  );
}

/** One template: its own box, its own Try, and its own answer. Everything that belongs to a prompt is
 *  on the row it belongs to, including the long strings — the path, the placeholder list, the
 *  validator's refusal — so nothing has to be collected into a legend somewhere else. */
function PromptEditor({
  row,
  draft,
  reply,
  trial,
  action,
  asking,
  tr,
  onEdit,
  onSave,
  onRestore,
  onTrial,
}: {
  row: PromptRowDto;
  draft: string;
  reply: PromptReply | null;
  trial: PromptTrialDto | null;
  action: PromptAction | null;
  /** Whether a request is already in flight somewhere on this page. Only the trial buttons listen: a
   *  second hosted call the user did not ask for costs a second set of tokens, while writing a file
   *  costs nothing and should stay available under a slow answer. */
  asking: boolean;
  tr: Tr;
  onEdit: (text: string) => void;
  onSave: (text: string) => void;
  onRestore: () => void;
  onTrial: (text: string) => void;
}) {
  const dirty = draft !== row.text;
  // `origin` is a `String` on the Rust side, so a label this mirror has never met deserves a colour
  // rather than a blank settings page.
  const origin = ORIGINS[row.origin] ?? ORIGINS.shipped;
  // Sized to the file rather than to the typing, so the box does not jump under the cursor mid-word.
  const tall = row.text.split("\n").length > 12;
  const written = reply?.savedPath ? tr("windui_web_saved", { path: reply.savedPath }) : reply?.note;

  return (
    <Panel className="space-y-3 p-4">
      <div className="flex flex-wrap items-center gap-x-3 gap-y-1.5">
        <span className="selectable font-mono text-xs font-medium text-slate-200">{row.name}</span>
        <Pill tone={origin.tone}>{tr(origin.key)}</Pill>
        {dirty ? <Pill tone="amber">{tr("ai.prompts.unsaved")}</Pill> : null}
      </div>

      <div className={cn("grid grid-cols-1 gap-4", row.changed && "lg:grid-cols-2")}>
        <Field label={tr("ai.prompts.box")} hint={tr("ai.prompts.box_hint")}>
          <textarea
            rows={tall ? 14 : 6}
            className={cn(inputClass, "selectable font-mono text-[11px] leading-relaxed")}
            value={draft}
            onChange={(event) => onEdit(event.target.value)}
          />
        </Field>
        {/* Beside it, not behind a click: an override that has drifted from a newer default has to be
            readable in the same screen that edits it. Only this box runs; this block never does. */}
        {row.changed ? (
          <Field label={tr("ai.prompts.shipped")} hint={tr("ai.prompts.shipped_hint")}>
            <pre className="selectable max-h-[430px] overflow-y-auto whitespace-pre-wrap break-words rounded-xl border border-slate-700/50 bg-slate-950/40 px-3 py-2 font-mono text-[11px] leading-relaxed text-slate-400">
              {row.shipped}
            </pre>
          </Field>
        ) : null}
      </div>

      {/* The slots this template understands, on the row that owns them. The highlighted ones are the
          subset whose absence would make the request carry no screen text at all, which is exactly what
          a save refuses to write. */}
      <div className="flex flex-wrap items-baseline gap-x-1.5 gap-y-1 text-[10px] leading-relaxed">
        <span className="uppercase tracking-wide text-slate-500">{tr("ai.prompts.slots")}</span>
        {row.placeholders.map((token) => (
          <code
            key={token}
            className={cn("selectable rounded px-1 font-mono", row.required.includes(token) ? "bg-emerald-500/15 text-emerald-300" : "text-slate-400")}
          >
            {token}
          </code>
        ))}
        {row.required.length > 0 ? <span className="text-slate-500">{tr("ai.prompts.required_hint")}</span> : null}
      </div>

      <p className="selectable break-all font-mono text-[10px] text-slate-600" title={tr("ai.prompts.file")}>
        {row.path}
      </p>

      <div className="flex flex-wrap items-center gap-2">
        <Button variant="primary" busy={action === "save"} disabled={!dirty || action !== null} onClick={() => onSave(draft)}>
          <Save className="h-3.5 w-3.5" />
          {action === "save" ? tr("windui_web_saving") : tr("windui_web_save")}
        </Button>
        <Button variant="ghost" busy={action === "restore"} disabled={!row.overridden || action !== null} title={tr("ai.prompts.restore_hint")} onClick={onRestore}>
          <RotateCcw className="h-3.5 w-3.5" />
          {tr("ai.prompts.restore")}
        </Button>
        <Button variant="ghost" busy={action === "trial"} disabled={asking || action !== null} title={tr("ai.prompts.try_hint")} onClick={() => onTrial(draft)}>
          <Zap className="h-3.5 w-3.5" />
          {action === "trial" ? tr("ai.prompts.trying") : tr("ai.prompts.try")}
        </Button>
      </div>

      {reply?.error ? <p className="selectable text-[11px] leading-relaxed text-rose-200/90">{reply.error}</p> : null}
      {written ? <p className="selectable break-all text-[11px] leading-relaxed text-sky-100/80">{written}</p> : null}

      {trial ? (
        <div className="space-y-1.5 rounded-xl border border-slate-700/60 bg-slate-950/40 px-3 py-2.5">
          <div className="flex flex-wrap items-baseline gap-x-3 gap-y-1 text-[10px] text-slate-500">
            {/* Which stretch it asked about travels with the answer, so a paragraph about yesterday's
                meeting cannot be read as a claim about every one this machine has seen. */}
            {trial.segment ? <span className="selectable font-mono text-slate-400">{trial.segment}</span> : null}
            <span>{tr("ai.prompts.trial_chars", { chars: trial.chars })}</span>
          </div>
          <p
            className={cn(
              "selectable whitespace-pre-wrap break-words font-mono text-[11px] leading-relaxed",
              trial.ok ? "text-emerald-200/90" : "text-rose-200/90",
            )}
          >
            {trial.message}
          </p>
        </div>
      ) : null}
    </Panel>
  );
}
