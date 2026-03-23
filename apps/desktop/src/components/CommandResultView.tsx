/**
 * CommandResultView — renders the result of a built-in /command invocation.
 *
 * Visually distinct from both user chat bubbles and pipeline run cards:
 * uses a muted monospace "terminal-style" panel with a small command badge.
 */

// ── Types ─────────────────────────────────────────────────────────────────────

export type CommandStatus = "ok" | "error" | "info";

export interface CommandLine {
  /** Visual weight of the line: normal body | dimmed secondary | highlighted key=value */
  variant?: "body" | "dim" | "kv" | "heading" | "divider";
  text: string;
}

export interface CommandResult {
  /** The raw slash command the user typed, e.g. "/cost" */
  command: string;
  status: CommandStatus;
  /** Short one-line title shown beside the badge */
  title: string;
  /** Optional structured body lines */
  lines?: CommandLine[];
}

// ── Helpers ───────────────────────────────────────────────────────────────────

function statusColor(status: CommandStatus) {
  switch (status) {
    case "ok":    return "text-emerald-400 bg-emerald-950/60 border-emerald-800/50";
    case "error": return "text-red-400 bg-red-950/60 border-red-800/50";
    case "info":  return "text-sky-400 bg-sky-950/60 border-sky-800/50";
  }
}

function statusIcon(status: CommandStatus) {
  switch (status) {
    case "ok":    return "✓";
    case "error": return "✕";
    case "info":  return "ℹ";
  }
}

// ── Component ─────────────────────────────────────────────────────────────────

interface Props {
  result: CommandResult;
}

export default function CommandResultView({ result }: Props) {
  const { command, status, title, lines = [] } = result;
  const colors = statusColor(status);

  return (
    <div
      className={`rounded-xl border px-4 py-3 font-mono text-xs leading-relaxed ${colors}`}
      role="log"
      aria-label={`Command result for ${command}`}
    >
      {/* Header row: badge + title */}
      <div className="mb-2 flex items-center gap-2">
        <span className="inline-flex h-4 w-4 flex-shrink-0 items-center justify-center rounded text-[10px] font-bold ring-1 ring-current">
          {statusIcon(status)}
        </span>
        <span className="font-semibold tracking-wide opacity-80">{command}</span>
        <span className="opacity-60">—</span>
        <span className="opacity-90">{title}</span>
      </div>

      {/* Body lines */}
      {lines.length > 0 && (
        <div className="mt-2 space-y-0.5 border-t border-current/20 pt-2">
          {lines.map((line, i) => {
            if (line.variant === "divider") {
              return <hr key={i} className="border-current/20 my-1" />;
            }
            if (line.variant === "heading") {
              return (
                <p key={i} className="mt-2 font-semibold uppercase tracking-widest opacity-60 text-[10px]">
                  {line.text}
                </p>
              );
            }
            if (line.variant === "kv") {
              const [k, ...rest] = line.text.split(":");
              return (
                <p key={i} className="flex gap-2">
                  <span className="w-36 flex-shrink-0 opacity-50">{k}</span>
                  <span className="opacity-90">{rest.join(":").trimStart()}</span>
                </p>
              );
            }
            if (line.variant === "dim") {
              return <p key={i} className="opacity-40">{line.text}</p>;
            }
            return <p key={i} className="opacity-80">{line.text}</p>;
          })}
        </div>
      )}
    </div>
  );
}
