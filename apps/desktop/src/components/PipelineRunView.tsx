/**
 * PipelineRunView — live view while a pipeline is running.
 *
 * Receives `events` (per-stage progress) and a `done` flag to render:
 *   - A row per stage: role icon + status badge + summary
 *   - A "cancel" button while running
 *   - A final success/failure banner when done
 */

import { invoke } from "@tauri-apps/api/core";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Progress } from "@/components/ui/progress";
import { Separator } from "@/components/ui/separator";

// ── Types ────────────────────────────────────────────────────────────────────

export type PipelineEventDto =
  | { type: "stage_started"; role: string }
  | { type: "plan_ready"; steps: string[]; affected_files: string[]; complexity: string }
  | { type: "stage_completed"; role: string; summary: string; success: boolean }
  | { type: "pipeline_failed"; error: string }
  | { type: "drift_detected"; kind: string; reason: string }
  | { type: "network_error"; category: string; message: string; role: string };

export type PipelineDoneEvent =
  | { status: "ok"; stages: StageSummary[] }
  | { status: "err"; message: string };

export interface StageSummary {
  role: string;
  summary: string;
  success: boolean;
}

const STAGE_ORDER = ["planner", "coder", "risk", "reviewer"];

// ── PlanTodoList ─────────────────────────────────────────────────────────────────

const COMPLEXITY_COLOR: Record<string, string> = {
  low:    "text-green-400",
  medium: "text-yellow-400",
  high:   "text-red-400",
};

function PlanTodoList({
  steps,
  affectedFiles,
  complexity,
}: {
  steps: string[];
  affectedFiles: string[];
  complexity: string;
}) {
  const colorClass = COMPLEXITY_COLOR[complexity] ?? "text-gray-400";
  return (
    <div className="mt-2 rounded-lg border border-gray-700 bg-gray-900/60 px-4 py-3 space-y-2">
      <div className="flex items-center gap-2 text-xs text-gray-400">
        <span>Execution plan</span>
        <span className="·" />
        <span className={`font-medium ${colorClass}`}>
          {complexity.toUpperCase()}
        </span>
      </div>
      <ol className="space-y-1">
        {steps.map((step, i) => (
          <li key={i} className="flex items-start gap-2 text-xs text-gray-300">
            <span className="mt-0.5 flex h-4 w-4 shrink-0 items-center justify-center rounded border border-gray-600 text-[10px] text-gray-500">
              {i + 1}
            </span>
            <span>{step}</span>
          </li>
        ))}
      </ol>
      {affectedFiles.length > 0 && (
        <div className="pt-1 border-t border-gray-700">
          <p className="text-[10px] text-gray-500 mb-1">Files</p>
          <ul className="space-y-0.5">
            {affectedFiles.map((f) => (
              <li key={f} className="text-[11px] font-mono text-brand-400 truncate">
                • {f}
              </li>
            ))}
          </ul>
        </div>
      )}
    </div>
  );
}

const ROLE_ICON: Record<string, string> = {
  planner: "🧠",
  coder: "💻",
  risk: "🛡️",
  reviewer: "🔍",
};

// ── StageRow ─────────────────────────────────────────────────────────────────

function StageRow({
  role,
  status,
  summary,
}: {
  role: string;
  status: "waiting" | "running" | "completed" | "failed";
  summary?: string;
}) {
  return (
    <div className="flex items-start gap-3 rounded-lg bg-gray-800/60 px-4 py-3 text-sm">
      <span className="mt-0.5 text-base leading-none">
        {ROLE_ICON[role] ?? "⚙️"}
      </span>
      <div className="min-w-0 flex-1">
        <div className="flex items-center gap-2">
          <span className="font-medium capitalize text-brand-300">{role}</span>
          {status === "running" && (
            <div className="h-3 w-3 animate-spin rounded-full border-2 border-brand-400 border-t-transparent" />
          )}
          {status === "completed" && (
            <Badge variant="success" className="text-xs">done</Badge>
          )}
          {status === "failed" && (
            <Badge variant="destructive" className="text-xs">failed</Badge>
          )}
          {status === "waiting" && (
            <Badge variant="outline" className="text-xs text-gray-500">waiting</Badge>
          )}
        </div>
        {summary && (
          <p className="mt-1 text-xs text-gray-400 leading-relaxed">{summary}</p>
        )}
      </div>
    </div>
  );
}

// ── PipelineRunView ───────────────────────────────────────────────────────────

interface Props {
  events: PipelineEventDto[];
  done: PipelineDoneEvent | null;
  onDismiss?: () => void;
}

export default function PipelineRunView({ events, done, onDismiss }: Props) {
  // Derive per-stage status from the event stream
  const stageStatus: Record<string, { status: "waiting" | "running" | "completed" | "failed"; summary?: string }> =
    Object.fromEntries(STAGE_ORDER.map((r) => [r, { status: "waiting" }]));

  // Extract plan todo-list from a plan_ready event (if present)
  let planReady: { steps: string[]; affected_files: string[]; complexity: string } | null = null;
  // Collect network error warnings
  const networkErrors: { category: string; message: string; role: string }[] = [];

  for (const ev of events) {
    if (ev.type === "stage_started") {
      stageStatus[ev.role] = { status: "running" };
    } else if (ev.type === "plan_ready") {
      planReady = ev;
    } else if (ev.type === "stage_completed") {
      stageStatus[ev.role] = {
        status: ev.success ? "completed" : "failed",
        summary: ev.summary,
      };
    } else if (ev.type === "pipeline_failed") {
      // Mark the currently-running stage (if any) as failed
      for (const role of STAGE_ORDER) {
        if (stageStatus[role].status === "running") {
          stageStatus[role] = { status: "failed", summary: ev.error };
        }
      }
    } else if (ev.type === "network_error") {
      networkErrors.push({ category: ev.category, message: ev.message, role: ev.role });
    }
  }

  const completedCount = STAGE_ORDER.filter(
    (r) => stageStatus[r].status === "completed"
  ).length;
  const progressPct = Math.round((completedCount / STAGE_ORDER.length) * 100);

  const isRunning = !done;
  const succeeded = done?.status === "ok";

  async function handleCancel() {
    try {
      await invoke("cancel_pipeline");
    } catch {
      // best-effort
    }
  }

  return (
    <div className="card border-brand-800 bg-gray-900/80 space-y-4">
      {/* Header */}
      <div className="flex items-center justify-between">
        <div className="flex items-center gap-2">
          {isRunning ? (
            <>
              <div className="h-3 w-3 animate-spin rounded-full border-2 border-brand-400 border-t-transparent" />
              <span className="text-sm font-medium text-brand-300">
                Running pipeline…
              </span>
            </>
          ) : succeeded ? (
            <>
              <span className="text-green-400">✅</span>
              <span className="text-sm font-medium text-green-300">
                Pipeline complete
              </span>
            </>
          ) : (
            <>
              <span className="text-red-400">💥</span>
              <span className="text-sm font-medium text-red-300">
                Pipeline failed
              </span>
            </>
          )}
        </div>
        {isRunning && (
          <Button variant="ghost" size="sm" onClick={handleCancel} className="h-7 text-xs text-gray-400 hover:text-red-400">
            Cancel
          </Button>
        )}
        {!isRunning && onDismiss && (
          <Button variant="ghost" size="sm" onClick={onDismiss} className="h-7 text-xs">
            Dismiss
          </Button>
        )}
      </div>

      {/* Progress bar */}
      {isRunning && (
        <Progress value={progressPct} className="h-1.5" />
      )}

      <Separator className="bg-gray-800" />

      {/* Stage rows */}
      <div className="space-y-2">
        {STAGE_ORDER.map((role) => {
          const s = stageStatus[role];
          return (
            <div key={role}>
              <StageRow
                role={role}
                status={s.status}
                summary={s.summary}
              />
              {/* Show execution plan as a todo list under the planner row */}
              {role === "planner" && planReady && (
                <PlanTodoList
                  steps={planReady.steps}
                  affectedFiles={planReady.affected_files}
                  complexity={planReady.complexity}
                />
              )}
            </div>
          );
        })}
      </div>

      {/* Network error warnings */}
      {networkErrors.length > 0 && (
        <>
          <Separator className="bg-gray-800" />
          <div className="space-y-1.5">
            {networkErrors.map((ne, i) => (
              <div
                key={i}
                className="flex items-start gap-2 rounded-lg bg-yellow-950/30 px-4 py-2.5 text-xs text-yellow-200"
              >
                <span className="mt-0.5 shrink-0">⚠️</span>
                <div className="min-w-0">
                  <span className="font-medium text-yellow-400">
                    [{ne.category}]
                  </span>{" "}
                  <span className="capitalize text-yellow-300">{ne.role}</span>
                  {" — "}
                  {ne.message}
                </div>
              </div>
            ))}
          </div>
        </>
      )}

      {/* Final error banner */}
      {done?.status === "err" && (
        <>
          <Separator className="bg-gray-800" />
          <p className="rounded-lg bg-red-950/40 px-4 py-3 text-xs text-red-300">
            {done.message}
          </p>
        </>
      )}
    </div>
  );
}
