/* The primitives every screen is built from, so the design language is written once.
 *
 * These mirror the ServicePulse dashboard's vocabulary rather than inventing one: a translucent slate
 * panel with a hairline border and a heavy black shadow, pills that carry state by tint, one gradient
 * reserved for the thing you are meant to press, and blur behind it all.
 */

import { clsx, type ClassValue } from "clsx";
import { twMerge } from "tailwind-merge";
import { AlertTriangle, Inbox, Loader2 } from "lucide-react";
import type { ReactNode } from "react";

export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs));
}

export function Panel({ className, children }: { className?: string; children: ReactNode }) {
  return (
    <section
      className={cn(
        "relative rounded-2xl border border-slate-800/80 bg-slate-900/60 shadow-2xl shadow-black/40 backdrop-blur-xl",
        className,
      )}
    >
      {children}
    </section>
  );
}

/** The one accent in the product. Used for the brand tile and the primary action, and nothing else. */
export function GradientTile({ className, children }: { className?: string; children: ReactNode }) {
  return (
    <span
      className={cn(
        "relative flex items-center justify-center rounded-xl bg-gradient-to-br from-indigo-500 via-purple-600 to-pink-500 p-0.5 shadow-lg shadow-indigo-500/25",
        className,
      )}
    >
      <span className="flex h-full w-full items-center justify-center rounded-[11px] bg-slate-950/90">
        {children}
      </span>
    </span>
  );
}

export type Tone = "slate" | "indigo" | "emerald" | "amber" | "rose" | "purple";

/** How a screen asks for its words. Every screen already holds one; the primitives that carry copy of
 *  their own take it as a prop rather than reaching for a locale they cannot see. */
type Tr = (key: string, args?: Record<string, string | number>) => string;

const TONES: Record<Tone, string> = {
  slate: "bg-slate-500/10 text-slate-300 border-slate-500/25",
  indigo: "bg-indigo-500/10 text-indigo-300 border-indigo-500/30",
  emerald: "bg-emerald-500/10 text-emerald-300 border-emerald-500/30",
  amber: "bg-amber-500/10 text-amber-300 border-amber-500/30",
  rose: "bg-rose-500/10 text-rose-300 border-rose-500/30",
  purple: "bg-purple-500/10 text-purple-300 border-purple-500/30",
};

export function Pill({
  tone = "slate",
  className,
  children,
}: {
  tone?: Tone;
  className?: string;
  children: ReactNode;
}) {
  return (
    <span
      className={cn(
        "inline-flex items-center gap-1.5 rounded-full border px-2.5 py-1 text-xs font-medium",
        TONES[tone],
        className,
      )}
    >
      {children}
    </span>
  );
}

export function Button({
  variant = "ghost",
  busy = false,
  className,
  children,
  ...rest
}: React.ButtonHTMLAttributes<HTMLButtonElement> & {
  variant?: "primary" | "ghost" | "danger";
  busy?: boolean;
}) {
  const styles = {
    primary:
      "text-white bg-gradient-to-r from-indigo-500 via-purple-600 to-pink-500 hover:opacity-90 shadow-lg shadow-indigo-500/20",
    ghost:
      "text-slate-300 bg-slate-800/60 hover:bg-slate-700/70 border border-slate-700/70 hover:text-white",
    danger:
      "text-rose-200 bg-rose-500/10 hover:bg-rose-500/20 border border-rose-500/30",
  }[variant];
  return (
    <button
      {...rest}
      disabled={rest.disabled || busy}
      className={cn(
        "inline-flex items-center justify-center gap-2 rounded-xl px-3.5 py-2 text-sm font-medium transition-all duration-200 disabled:cursor-not-allowed disabled:opacity-50",
        styles,
        className,
      )}
    >
      {busy ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : null}
      {children}
    </button>
  );
}

export function Field({ label, hint, children }: { label: string; hint?: string; children: ReactNode }) {
  return (
    <label className="block space-y-1.5">
      <span className="block text-xs font-medium uppercase tracking-wide text-slate-400">{label}</span>
      {children}
      {hint ? <span className="block text-[11px] leading-relaxed text-slate-500">{hint}</span> : null}
    </label>
  );
}

export const inputClass =
  "w-full rounded-xl border border-slate-700/70 bg-slate-950/60 px-3 py-2 text-sm text-slate-100 outline-none transition-colors placeholder:text-slate-600 focus:border-indigo-500/60 focus:ring-2 focus:ring-indigo-500/20";

/**
 * The empty state names what the install actually holds. "Nothing recorded" over a library of eleven
 * months is a different claim than "nothing in this range", and the window has no way to know which it
 * is unless the caller says so — so the caller says so.
 */
export function Empty({ title, detail, action }: { title: string; detail: string; action?: ReactNode }) {
  return (
    <Panel className="flex flex-col items-center gap-3 px-6 py-14 text-center animate-fade-in">
      <span className="flex h-12 w-12 items-center justify-center rounded-2xl border border-slate-700/70 bg-slate-800/50 text-slate-400">
        <Inbox className="h-5 w-5" />
      </span>
      <p className="text-sm font-medium text-slate-200">{title}</p>
      <p className="max-w-md text-xs leading-relaxed text-slate-500">{detail}</p>
      {action}
    </Panel>
  );
}

/** A failure is never folded into an empty state.
 *
 *  `tr` is required rather than defaulted: this is the one primitive that speaks two sentences of its
 *  own — what failed to read, and the door that tries again — and a banner that silently kept them in
 *  English is exactly the bug the rest of this file is written against. */
export function ErrorBanner({ message, onRetry, tr }: { message: string; onRetry?: () => void; tr: Tr }) {
  return (
    <Panel className="flex items-start gap-3 border-rose-500/30 bg-rose-500/5 px-4 py-3 animate-rise-in">
      <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0 text-rose-400" />
      <div className="min-w-0 flex-1 space-y-0.5">
        <p className="text-sm font-medium text-rose-200">{tr("windui_web_error_title")}</p>
        <p className="selectable break-words text-xs leading-relaxed text-rose-200/70">{message}</p>
      </div>
      {onRetry ? (
        <Button variant="ghost" onClick={onRetry}>
          {tr("windui_web_retry")}
        </Button>
      ) : null}
    </Panel>
  );
}

export function SectionTitle({ title, detail, right }: { title: string; detail?: string; right?: ReactNode }) {
  return (
    <div className="flex flex-wrap items-end justify-between gap-3">
      <div>
        <h2 className="text-base font-semibold text-white">{title}</h2>
        {detail ? <p className="mt-0.5 text-xs text-slate-500">{detail}</p> : null}
      </div>
      {right}
    </div>
  );
}
