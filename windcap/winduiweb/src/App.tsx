import { useCallback, useEffect, useMemo, useState } from "react";
import {
  CalendarDays,
  ChartColumn,
  Clapperboard,
  Cpu,
  Octagon,
  Radio,
  RefreshCw,
  Search as SearchIcon,
  Settings2,
  Sparkles,
  X,
} from "lucide-react";
import { about, errorMessage, libraryStats, recorderState, startupTab, uiLocale, uiStrings } from "./services/api";
import { COPY_KEYS, FALLBACK, t, type Copy } from "./copy";
import type { About, Stats } from "./types";
import { Button, ErrorBanner, GradientTile, Panel, Pill } from "./components/ui";
import { cn } from "./components/ui";
import { SearchScreen } from "./screens/Search";
import { OneDayScreen } from "./screens/OneDay";
import { StatScreen } from "./screens/Stat";
import { FormScreen } from "./screens/Forms";

type Tab = "search" | "oneday" | "stat" | "recording" | "settings" | "ai";

const TABS: { id: Tab; key: string; icon: typeof SearchIcon }[] = [
  { id: "search", key: "windui_tab_search", icon: SearchIcon },
  { id: "oneday", key: "windui_tab_oneday", icon: CalendarDays },
  { id: "stat", key: "windui_tab_stat", icon: ChartColumn },
  { id: "recording", key: "windui_tab_recording", icon: Clapperboard },
  { id: "settings", key: "windui_tab_settings", icon: Settings2 },
  { id: "ai", key: "windui_tab_ai", icon: Sparkles },
];

type Toast = { id: number; text: string; tone: "ok" | "bad" };

export function App() {
  const [tab, setTab] = useState<Tab>("search");
  const [boot, setBoot] = useState<{ about?: About; error?: string; busy: boolean }>({ busy: true });
  const [copy, setCopy] = useState<Copy>(FALLBACK);
  const [stats, setStats] = useState<Stats | null>(null);
  const [recorder, setRecorder] = useState<{ running: boolean; pid: number | null } | null>(null);
  const [toasts, setToasts] = useState<Toast[]>([]);

  const say = useCallback((text: string, tone: "ok" | "bad" = "ok") => {
    const id = Date.now() + Math.random();
    setToasts((all) => [...all, { id, text, tone }]);
    window.setTimeout(() => setToasts((all) => all.filter((one) => one.id !== id)), 6000);
  }, []);

  const loadCopy = useCallback(async () => {
    // The locale first, then the strings: asking for both in one round trip would mean the Rust side
    // guessing which language the window is about to paint in.
    const locale = await uiLocale();
    const resolved = await uiStrings(COPY_KEYS);
    setCopy({ ...FALLBACK, ...resolved, __locale: locale });
  }, []);

  const bootOnce = useCallback(async () => {
    setBoot({ busy: true });
    try {
      const info = await about();
      setBoot({ about: info, busy: false });
    } catch (caught) {
      setBoot({ busy: false, error: errorMessage(caught) });
    }
  }, []);

  useEffect(() => {
    void bootOnce();
    // The hint is read once, before anything else can move the tab, so a `--tab settings` launch shows
    // Settings rather than flashing Search first.
    void startupTab()
      .then((hint) => {
        const name = (hint ?? "").toLowerCase();
        const found = TABS.find((one) => one.id === name);
        if (found) setTab(found.id);
      })
      .catch(() => {
        /* No hint, no change: the default screen is a real screen. */
      });
  }, [bootOnce]);

  useEffect(() => {
    if (!boot.about) return;
    void loadCopy().catch(() => {
      /* The fallback map renders; nothing else in the window depends on copy. */
    });
    // The row count reads every month file, so it is asked for after the window is up rather than
    // before it paints. A large library takes seconds, and the pill says "…" while it does.
    void libraryStats().then(setStats).catch(() => setStats(null));
    void refreshRecorder();
  }, [boot.about]);

  const refreshRecorder = useCallback(() => {
    void recorderState()
      .then(setRecorder)
      .catch(() => setRecorder(null));
  }, []);

  const locale = String(copy.__locale ?? "en");
  const tr = useCallback((key: string, args?: Record<string, string | number>) => t(copy, key, args), [copy]);

  const screen = useMemo(() => {
    switch (tab) {
      case "search":
        return <SearchScreen tr={tr} say={say} />;
      case "oneday":
        // `say` because the day's rail has the same two doors as the search rail, and one of them leaves
        // the window: a refused `locate` is a toast, not a silent button.
        return <OneDayScreen tr={tr} say={say} />;
      case "stat":
        return <StatScreen tr={tr} />;
      case "recording":
        return <FormScreen page="recording" tr={tr} say={say} />;
      case "settings":
        // `relabel` is the language row's door: saving `lang` re-reads the window's copy, so the page
        // speaks the new language in the same click that chose it.
        return <FormScreen page="settings" tr={tr} say={say} relabel={loadCopy} />;
      case "ai":
        return <FormScreen page="ai" tr={tr} say={say} />;
    }
  }, [tab, tr, say, loadCopy]);

  const active = TABS.find((one) => one.id === tab) ?? TABS[0];

  if (boot.error) {
    return (
      <div className="flex h-full items-center justify-center p-8">
        <div className="w-full max-w-xl space-y-4">
          <ErrorBanner message={boot.error} onRetry={() => void bootOnce()} tr={tr} />
          <p className="text-center text-xs leading-relaxed text-slate-500">{tr("windui_web_boot_error_note")}</p>
        </div>
      </div>
    );
  }

  return (
    <div className="flex h-full overflow-hidden">
      {/* Ambient light. One blurred indigo field behind the rail and one pink behind the content: enough
          to stop a near-black page looking flat, few enough that nothing glows twice. */}
      <div className="pointer-events-none fixed left-[-10%] top-[-20%] -z-10 h-[420px] w-[520px] rounded-full bg-indigo-500/10 blur-3xl" />
      <div className="pointer-events-none fixed bottom-[-25%] right-[-5%] -z-10 h-[380px] w-[480px] rounded-full bg-pink-500/[0.07] blur-3xl" />

      <aside className="flex w-[210px] shrink-0 flex-col gap-1 border-r border-slate-800/70 bg-slate-950/40 px-3 py-4 backdrop-blur-xl">
        <div className="flex items-center gap-3 px-1.5 pb-4">
          <GradientTile className="h-10 w-10">
            <Radio className="h-4.5 w-4.5 text-white" strokeWidth={2} />
          </GradientTile>
          <div className="min-w-0">
            <p className="truncate text-sm font-semibold text-white">{tr("windui_web_brand")}</p>
            <p className="truncate text-[11px] text-slate-500">{tr("windui_web_tagline")}</p>
          </div>
        </div>

        {TABS.map((item) => {
          const Icon = item.icon;
          const on = item.id === tab;
          return (
            <button
              key={item.id}
              onClick={() => setTab(item.id)}
              className={cn(
                "group relative flex items-center gap-3 rounded-xl px-3 py-2.5 text-left text-sm transition-all duration-200",
                on ? "bg-white/[0.06] text-white" : "text-slate-400 hover:bg-white/[0.03] hover:text-slate-200",
              )}
            >
              {on ? (
                <span className="absolute left-0 top-1/2 h-5 w-[3px] -translate-y-1/2 rounded-r bg-gradient-to-b from-indigo-400 to-pink-500" />
              ) : null}
              <Icon className={cn("h-4 w-4 shrink-0", on ? "text-indigo-300" : "text-slate-500 group-hover:text-slate-300")} />
              <span className="truncate">{tr(item.key)}</span>
            </button>
          );
        })}

        <div className="mt-auto space-y-1.5 px-1.5 pt-4 text-[10px] leading-relaxed text-slate-600">
          <p className="selectable truncate" title={boot.about?.root}>
            {boot.about?.root ?? "…"}
          </p>
          <p className="selectable">{boot.about?.version ?? ""}</p>
          <p>{tr("windui_web_locale_row", { locale })}</p>
        </div>
      </aside>

      <main className="flex min-w-0 flex-1 flex-col">
        <header className="px-6 pt-6">
          <Panel className="flex flex-wrap items-center justify-between gap-4 p-4">
            <div className="flex items-center gap-3">
              <h1 className="text-lg font-semibold tracking-tight text-white">{tr(active.key)}</h1>
              {stats ? (
                <Pill tone="slate">
                  <Cpu className="h-3 w-3" />
                  {stats.rows.toLocaleString()} · {stats.monthsTotal}
                </Pill>
              ) : (
                <Pill tone="slate">{tr("windui_web_loading")}…</Pill>
              )}
              {stats?.error ? <Pill tone="amber">{stats.error}</Pill> : null}
            </div>
            <div className="flex items-center gap-2">
              {recorder?.running ? (
                <Pill tone="emerald">
                  <span className="h-1.5 w-1.5 animate-pulse rounded-full bg-emerald-400" />
                  {tr("windui_web_recording_on", { pid: recorder.pid ?? "—" })}
                </Pill>
              ) : (
                <Pill tone="rose">
                  <Octagon className="h-3 w-3" />
                  {tr("windui_web_recording_off")}
                </Pill>
              )}
              <Button
                variant="ghost"
                onClick={() => {
                  void libraryStats().then(setStats).catch(() => setStats(null));
                  refreshRecorder();
                }}
              >
                <RefreshCw className={cn("h-3.5 w-3.5", boot.busy && "animate-spin")} />
                {tr("windui_footer_refresh")}
              </Button>
            </div>
          </Panel>
        </header>

        <div className="min-h-0 flex-1 overflow-y-auto px-6 pb-10 pt-5">{screen}</div>
      </main>

      <div className="pointer-events-none fixed bottom-5 right-5 z-50 flex w-[380px] flex-col gap-2">
        {toasts.map((one) => (
          <div
            key={one.id}
            className={cn(
              "animate-rise-in rounded-xl border px-4 py-3 text-xs shadow-2xl shadow-black/50 backdrop-blur-xl",
              one.tone === "ok"
                ? "border-emerald-500/30 bg-emerald-500/10 text-emerald-200"
                : "border-rose-500/30 bg-rose-500/10 text-rose-200",
            )}
          >
            <div className="flex items-start gap-2">
              <p className="selectable min-w-0 flex-1 leading-relaxed">{one.text}</p>
              <button
                className="pointer-events-auto text-current/60 hover:text-current"
                onClick={() => setToasts((all) => all.filter((x) => x.id !== one.id))}
              >
                <X className="h-3.5 w-3.5" />
              </button>
            </div>
          </div>
        ))}
      </div>
    </div>
  );
}
