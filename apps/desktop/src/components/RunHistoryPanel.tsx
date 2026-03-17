/**
 * RunHistoryPanel — sidebar panel listing past pipeline runs.
 *
 * Calls `get_run_history` on mount and whenever `refreshKey` changes.
 * Each entry shows: prompt (truncated), status badge, duration, token count.
 */

import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Badge } from "@/components/ui/badge";
import { ScrollArea } from "@/components/ui/scroll-area";
import { Separator } from "@/components/ui/separator";

// ── Types ─────────────────────────────────────────────────────────────────────

interface RunSummary {
  run_id: string;
  prompt: string;
  duration_ms: number;
  total_tokens: number;
  success: boolean;
  started_at_secs: number;
}

// ── Helpers ───────────────────────────────────────────────────────────────────

function fmtDuration(ms: number): string {
  if (ms < 1000) return `${ms}ms`;
  const s = (ms / 1000).toFixed(1);
  return `${s}s`;
}

function fmtTokens(n: number): string {
  if (n >= 1000) return `${(n / 1000).toFixed(1)}k`;
  return String(n);
}

function fmtTime(secs: number): string {
  const d = new Date(secs * 1000);
  return d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}

function truncate(s: string, max = 60) {
  return s.length > max ? s.slice(0, max - 1) + "…" : s;
}

// ── RunHistoryPanel ───────────────────────────────────────────────────────────

interface Props {
  refreshKey?: number;
  projectDir?: string;
}

export default function RunHistoryPanel({ refreshKey = 0, projectDir }: Props) {
  const [runs, setRuns] = useState<RunSummary[]>([]);
  const [loading, setLoading] = useState(false);

  useEffect(() => {
    setLoading(true);
    invoke<RunSummary[]>("get_run_history", {
      projectDir: projectDir ?? null,
    })
      .then(setRuns)
      .catch(() => setRuns([]))
      .finally(() => setLoading(false));
  }, [refreshKey, projectDir]);

  return (
    <aside className="flex h-full w-64 flex-col border-l border-gray-800 bg-gray-950">
      <div className="flex items-center justify-between px-4 py-3">
        <h2 className="text-xs font-semibold uppercase tracking-wider text-gray-500">
          Run History
        </h2>
        {loading && (
          <div className="h-3 w-3 animate-spin rounded-full border border-gray-500 border-t-transparent" />
        )}
      </div>
      <Separator className="bg-gray-800" />
      <ScrollArea className="flex-1">
        {runs.length === 0 && !loading ? (
          <p className="px-4 py-6 text-center text-xs text-gray-600">
            No runs yet
          </p>
        ) : (
          <div className="divide-y divide-gray-800/60">
            {runs.map((run) => (
              <div
                key={run.run_id}
                className="px-4 py-3 hover:bg-gray-900 transition-colors"
              >
                {/* Status + time */}
                <div className="mb-1 flex items-center justify-between gap-2">
                  <Badge
                    variant={run.success ? "success" : "destructive"}
                    className="text-xs"
                  >
                    {run.success ? "ok" : "fail"}
                  </Badge>
                  <span className="text-xs text-gray-600">
                    {fmtTime(run.started_at_secs)}
                  </span>
                </div>
                {/* Prompt */}
                <p className="text-xs text-gray-300 leading-snug">
                  {truncate(run.prompt)}
                </p>
                {/* Metrics */}
                <div className="mt-1.5 flex gap-3 text-xs text-gray-600">
                  <span>⏱ {fmtDuration(run.duration_ms)}</span>
                  <span>🪙 {fmtTokens(run.total_tokens)} tok</span>
                </div>
              </div>
            ))}
          </div>
        )}
      </ScrollArea>
    </aside>
  );
}
