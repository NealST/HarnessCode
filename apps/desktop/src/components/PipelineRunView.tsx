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
  | {
      type: "judge_ready";
      route: string;
      route_reason_code: string;
      ready_for_scoper: boolean;
      ready_for_planner: boolean;
      ask_user_clarification: boolean;
      effective_request: string;
      goal_is_concrete: boolean;
      constraints_are_stable: boolean;
      history_resolves_references: boolean;
      repository_grounding_needed: boolean;
      prior_scope_can_be_reused: boolean;
      skip_scoper_criteria_met: string[];
      missing_information: string[];
      clarifying_questions: string[];
      confidence: string;
    }
  | {
      type: "scope_ready";
      task_type: string;
      objective: string;
      in_scope: string[];
      out_of_scope: string[];
      unknowns: string[];
      success_criteria: string[];
      relevant_files: string[];
      needs_user_clarification: boolean;
      clarifying_questions: string[];
      confidence: string;
    }
  | {
      type: "clarification_requested";
      source: string;
      objective: string;
      questions: string[];
    }
  | {
      type: "plan_ready";
      phase_count: number;
      phases: { phase_id: number; title: string; step_count: number; complexity: string }[];
      complexity: string;
    }
  | { type: "stage_completed"; role: string; summary: string; success: boolean }
  | { type: "stage_skipped"; role: string }
  | { type: "stage_retrying"; role: string; reason: string; attempt: number }
  | { type: "phase_started"; phase_id: number; title: string; total_phases: number }
  | {
      type: "phase_completed";
      phase_id: number;
      title: string;
      total_phases: number;
      explanation: string;
      files_changed: number;
      affected_files: string[];
    }
  | { type: "phase_retrying"; phase_id: number; title: string; reason: string; attempt: number }
  | { type: "phase_failed"; phase_id: number; title: string; reason: string }
  | {
      type: "risk_assessed";
      risk_level: "low" | "medium" | "high" | "unknown";
      reason: string;
      affected_areas: string[];
      breaking_change: boolean;
      security_implications: string;
      cr_focus: string;
      risk_unavailable: boolean;
    }
  | {
      type: "review_completed";
      approved: boolean;
      criteria_met: boolean;
      issues: string[];
      security_concerns: string[];
      recommendation: string;
    }
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

const STAGE_ORDER = ["judge", "scoper", "planner", "conductor", "risk", "reviewer"];

// ── PlanTodoList ─────────────────────────────────────────────────────────────────

const COMPLEXITY_COLOR: Record<string, string> = {
  low: "text-green-400",
  medium: "text-yellow-400",
  high: "text-red-400",
};

function PlanTodoList({
  phases,
  complexity,
}: {
  phases: { phase_id: number; title: string; step_count: number; complexity: string }[];
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
        {phases.map((phase) => (
          <li key={phase.phase_id} className="flex items-start gap-2 text-xs text-gray-300">
            <span className="mt-0.5 flex h-4 w-4 shrink-0 items-center justify-center rounded border border-gray-600 text-[10px] text-gray-500">
              {phase.phase_id}
            </span>
            <span className="flex-1">{phase.title}</span>
            <span className="text-gray-500">{phase.step_count} step{phase.step_count !== 1 ? "s" : ""}</span>
          </li>
        ))}
      </ol>
    </div>
  );
}
function ScopeCard({
  scope,
}: {
  scope: Extract<PipelineEventDto, { type: "scope_ready" }>;
}) {
  return (
    <div className="mt-2 rounded-lg border border-gray-700 bg-gray-900/60 px-4 py-3 space-y-3">
      <div className="flex items-center gap-2 text-xs text-gray-400">
        <span>Problem frame</span>
        <span className="text-brand-300 uppercase">{scope.task_type}</span>
        <span>confidence {scope.confidence}</span>
      </div>
      <p className="text-sm text-gray-200 leading-relaxed">{scope.objective}</p>
      {scope.in_scope.length > 0 && (
        <div>
          <p className="mb-1 text-[10px] uppercase tracking-wide text-gray-500">In scope</p>
          <ul className="space-y-1 text-xs text-gray-300">
            {scope.in_scope.map((item) => (
              <li key={item}>• {item}</li>
            ))}
          </ul>
        </div>
      )}
      {scope.out_of_scope.length > 0 && (
        <div>
          <p className="mb-1 text-[10px] uppercase tracking-wide text-gray-500">Out of scope</p>
          <ul className="space-y-1 text-xs text-gray-400">
            {scope.out_of_scope.map((item) => (
              <li key={item}>• {item}</li>
            ))}
          </ul>
        </div>
      )}
      {scope.success_criteria.length > 0 && (
        <div>
          <p className="mb-1 text-[10px] uppercase tracking-wide text-gray-500">Success criteria</p>
          <ul className="space-y-1 text-xs text-gray-300">
            {scope.success_criteria.map((item) => (
              <li key={item}>• {item}</li>
            ))}
          </ul>
        </div>
      )}
      {(scope.unknowns.length > 0 || scope.needs_user_clarification) && (
        <div className="rounded-md border border-yellow-900/60 bg-yellow-950/20 px-3 py-2">
          <p className="mb-1 text-[10px] uppercase tracking-wide text-yellow-500">Open questions</p>
          <ul className="space-y-1 text-xs text-yellow-100">
            {scope.unknowns.map((item) => (
              <li key={item}>• {item}</li>
            ))}
            {scope.clarifying_questions.map((item) => (
              <li key={item}>? {item}</li>
            ))}
          </ul>
        </div>
      )}
      {scope.relevant_files.length > 0 && (
        <div>
          <p className="mb-1 text-[10px] uppercase tracking-wide text-gray-500">Relevant files</p>
          <ul className="space-y-0.5">
            {scope.relevant_files.map((file) => (
              <li key={file} className="truncate font-mono text-[11px] text-brand-400">
                • {file}
              </li>
            ))}
          </ul>
        </div>
      )}
    </div>
  );
}

const ROLE_ICON: Record<string, string> = {
  judge: "⚖️",
  scoper: "🧭",
  planner: "🧠",
  conductor: "💻",
  risk: "🛡️",
  reviewer: "🔍",
};

// ── RiskCard ────────────────────────────────────────────────────────────────────────────────

const RISK_CONFIG = {
  high:    { color: "text-red-400",    border: "border-red-900/60",    bg: "bg-red-950/20",    icon: "🚨", badge: "destructive" as const },
  medium:  { color: "text-yellow-400", border: "border-yellow-900/60", bg: "bg-yellow-950/20", icon: "⚠️",  badge: "warning"     as const },
  low:     { color: "text-green-400",  border: "border-green-900/40",  bg: "bg-green-950/10",  icon: "✅",  badge: "success"     as const },
  unknown: { color: "text-gray-400",   border: "border-gray-700",      bg: "bg-gray-900/60",   icon: "❓",  badge: "outline"     as const },
};

function RiskCard({
  ev,
}: {
  ev: Extract<PipelineEventDto, { type: "risk_assessed" }>;
}) {
  if (ev.risk_unavailable) {
    return (
      <div className="mt-2 rounded-lg border border-yellow-900/60 bg-yellow-950/20 px-4 py-3">
        <div className="flex items-center gap-2 text-xs text-yellow-400">
          <span>⚠️</span>
          <span>Risk assessment unavailable — review proceeded without risk data</span>
        </div>
      </div>
    );
  }

  const cfg = RISK_CONFIG[ev.risk_level] ?? RISK_CONFIG.unknown;
  return (
    <div
      className={`mt-2 rounded-lg border ${cfg.border} ${cfg.bg} px-4 py-3 space-y-2`}
    >
      {/* Header */}
      <div className="flex items-center gap-2">
        <span className="text-base leading-none">{cfg.icon}</span>
        <span className={`text-sm font-semibold ${cfg.color}`}>
          {ev.risk_level.toUpperCase()}
        </span>
        <Badge variant={cfg.badge} className="text-[10px] uppercase">
          {ev.risk_level}
        </Badge>
      </div>

      {/* Reason */}
      <p className="text-xs text-gray-300 leading-relaxed">{ev.reason}</p>

      {/* Affected areas */}
      {ev.affected_areas.length > 0 && (
        <div className="flex flex-wrap gap-1">
          {ev.affected_areas.map((area) => (
            <span
              key={area}
              className="rounded border border-gray-700 px-1.5 py-0.5 text-[10px] text-gray-400"
            >
              {area}
            </span>
          ))}
        </div>
      )}

      {/* Breaking change */}
      {ev.breaking_change && (
        <div className="flex items-center gap-1.5 text-xs text-red-400 font-medium">
          <span>⚡</span>
          <span>Breaking change</span>
        </div>
      )}

      {/* Security implications */}
      {ev.security_implications && (
        <div className="text-xs text-orange-300">
          <span className="font-medium">🔒 Security: </span>
          {ev.security_implications}
        </div>
      )}

      {/* Code review focus */}
      {ev.cr_focus && (
        <div className="rounded border border-gray-700 bg-gray-900/40 px-3 py-2">
          <p className="text-[10px] uppercase tracking-wide text-gray-500 mb-1">
            Review focus
          </p>
          <p className="text-xs text-gray-300">{ev.cr_focus}</p>
        </div>
      )}
    </div>
  );
}

// ── ReviewCard ───────────────────────────────────────────────────────────────

function ReviewCard({
  ev,
}: {
  ev: Extract<PipelineEventDto, { type: "review_completed" }>;
}) {
  const hasIssues = ev.issues.length > 0;
  const hasSecurity = ev.security_concerns.length > 0;
  const borderColor = ev.approved
    ? ev.criteria_met
      ? hasIssues ? "border-yellow-900/60" : "border-green-900/40"
      : "border-yellow-900/60"
    : "border-red-900/60";
  const bgColor = ev.approved
    ? ev.criteria_met
      ? hasIssues ? "bg-yellow-950/20" : "bg-green-950/10"
      : "bg-yellow-950/20"
    : "bg-red-950/20";
  // Degrade to cautionary yellow when approved but criteria not fully met
  const icon = ev.approved
    ? ev.criteria_met
      ? hasIssues ? "⚠️" : "✅"
      : "⚠️"
    : "❌";
  const labelColor = ev.approved
    ? ev.criteria_met
      ? hasIssues ? "text-yellow-400" : "text-green-400"
      : "text-yellow-400"
    : "text-red-400";
  const label = ev.approved
    ? ev.criteria_met
      ? "APPROVED"
      : "APPROVED WITH CAVEATS"
    : "CONCERNS FOUND";

  return (
    <div className={`mt-2 rounded-lg border ${borderColor} ${bgColor} px-4 py-3 space-y-2`}>
      {/* Header */}
      <div className="flex items-center gap-2">
        <span className="text-base leading-none">{icon}</span>
        <span className={`text-sm font-semibold ${labelColor}`}>{label}</span>
        <span
          className={`rounded border px-1.5 py-0.5 text-[10px] uppercase ${
            ev.criteria_met
              ? "border-green-800 text-green-400"
              : "border-red-800 text-red-400"
          }`}
        >
          criteria {ev.criteria_met ? "met" : "not met"}
        </span>
      </div>

      {/* Recommendation */}
      <p className="text-xs text-gray-200 leading-relaxed">{ev.recommendation}</p>

      {/* Issues */}
      {hasIssues && (
        <div className="rounded border border-yellow-900/40 bg-yellow-950/10 px-3 py-2 space-y-1">
          <p className="text-[10px] uppercase tracking-wide text-yellow-500">Issues</p>
          <ul className="space-y-1">
            {ev.issues.map((issue, i) => (
              <li key={i} className="text-xs text-yellow-100">
                • {issue}
              </li>
            ))}
          </ul>
        </div>
      )}

      {/* Security concerns */}
      {hasSecurity && (
        <div className="rounded border border-red-900/50 bg-red-950/20 px-3 py-2 space-y-1">
          <p className="text-[10px] uppercase tracking-wide text-red-400">Security</p>
          <ul className="space-y-1">
            {ev.security_concerns.map((concern, i) => (
              <li key={i} className="text-xs text-red-200">
                🔒 {concern}
              </li>
            ))}
          </ul>
        </div>
      )}
    </div>
  );
}

// ── StageRow ─────────────────────────────────────────────────────────────────

function StageRow({
  role,
  status,
  summary,
}: {
  role: string;
  status: "waiting" | "running" | "completed" | "failed" | "skipped" | "advisory";
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
            <Badge variant="success" className="text-xs">
              done
            </Badge>
          )}
          {status === "failed" && (
            <Badge variant="destructive" className="text-xs">
              failed
            </Badge>
          )}
          {status === "skipped" && (
            <Badge variant="outline" className="text-xs text-gray-500">
              skipped
            </Badge>
          )}
          {status === "advisory" && (
            <Badge variant="outline" className="text-xs text-yellow-400 border-yellow-700">
              reviewed
            </Badge>
          )}
          {status === "waiting" && (
            <Badge variant="outline" className="text-xs text-gray-500">
              waiting
            </Badge>
          )}
        </div>
        {summary && (
          <p className="mt-1 text-xs text-gray-400 leading-relaxed">
            {summary}
          </p>
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
  const stageStatus: Record<
    string,
    { status: "waiting" | "running" | "completed" | "failed" | "skipped" | "advisory"; summary?: string }
  > = Object.fromEntries(STAGE_ORDER.map((r) => [r, { status: "waiting" }]));

  // Extract plan todo-list from a plan_ready event (if present)
  let planReady: Extract<PipelineEventDto, { type: "plan_ready" }> | null = null;
  let judgeReady: Extract<PipelineEventDto, { type: "judge_ready" }> | null = null;
  let scopeReady: Extract<PipelineEventDto, { type: "scope_ready" }> | null = null;
  let riskAssessed: Extract<PipelineEventDto, { type: "risk_assessed" }> | null = null;
  let reviewCompleted: Extract<PipelineEventDto, { type: "review_completed" }> | null = null;
  // Collect network error warnings
  const networkErrors: { category: string; message: string; role: string }[] =
    [];
  // Track phase progress within the conductor stage
  let activePhase: { id: number; title: string; total: number } | null = null;
  const completedPhases: number[] = [];

  for (const ev of events) {
    if (ev.type === "stage_started") {
      stageStatus[ev.role] = { status: "running" };
    } else if (ev.type === "judge_ready") {
      judgeReady = ev;
    } else if (ev.type === "scope_ready") {
      scopeReady = ev;
    } else if (ev.type === "plan_ready") {
      planReady = ev;
    } else if (ev.type === "phase_started") {
      activePhase = { id: ev.phase_id, title: ev.title, total: ev.total_phases };
      stageStatus["conductor"] = {
        status: "running",
        summary: `Phase ${ev.phase_id}/${ev.total_phases}: ${ev.title}`,
      };
    } else if (ev.type === "phase_completed") {
      completedPhases.push(ev.phase_id);
      stageStatus["conductor"] = {
        status: "running",
        summary: `Phase ${ev.phase_id}/${ev.total_phases} done — ${ev.explanation}`,
      };
      if (ev.phase_id === ev.total_phases) activePhase = null;
    } else if (ev.type === "phase_retrying") {
      stageStatus["conductor"] = {
        status: "running",
        summary: `Phase ${ev.phase_id} retrying (attempt ${ev.attempt}): ${ev.reason}`,
      };
    } else if (ev.type === "phase_failed") {
      stageStatus["conductor"] = { status: "failed", summary: `Phase ${ev.phase_id} failed: ${ev.reason}` };
    } else if (ev.type === "risk_assessed") {
      riskAssessed = ev;
    } else if (ev.type === "review_completed") {
      reviewCompleted = ev;
      stageStatus["reviewer"] = { status: "advisory", summary: ev.recommendation };
    } else if (ev.type === "clarification_requested") {
      // Handled at the App level via a separate Tauri event listener → ClarificationModal.
      // No stageStatus update needed here.
    } else if (ev.type === "stage_skipped") {
      stageStatus[ev.role] = { status: "skipped" };
    } else if (ev.type === "stage_retrying") {
      stageStatus[ev.role] = { status: "running", summary: `Retrying (attempt ${ev.attempt}): ${ev.reason}` };
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
      networkErrors.push({
        category: ev.category,
        message: ev.message,
        role: ev.role,
      });
    }
  }

  // "advisory" counts as done for progress purposes (reviewer always finishes this way)
  const completedCount = STAGE_ORDER.filter(
    (r) => stageStatus[r].status === "completed" || stageStatus[r].status === "advisory" || stageStatus[r].status === "skipped",
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
          <Button
            variant="ghost"
            size="sm"
            onClick={handleCancel}
            className="h-7 text-xs text-gray-400 hover:text-red-400"
          >
            Cancel
          </Button>
        )}
        {!isRunning && onDismiss && (
          <Button
            variant="ghost"
            size="sm"
            onClick={onDismiss}
            className="h-7 text-xs"
          >
            Dismiss
          </Button>
        )}
      </div>

      {/* Progress bar */}
      {isRunning && <Progress value={progressPct} className="h-1.5" />}

      <Separator className="bg-gray-800" />

      {/* Stage rows */}
      <div className="space-y-2">
        {STAGE_ORDER.map((role) => {
          const s = stageStatus[role];
          return (
            <div key={role}>
              <StageRow role={role} status={s.status} summary={s.summary} />
              {role === "judge" && judgeReady && (
                <div className="mt-2 rounded-lg border border-gray-700 bg-gray-900/60 px-4 py-3 space-y-2">
                  <div className="flex items-center gap-2 text-xs text-gray-400">
                    <span>Routing decision</span>
                    <span className="rounded border border-gray-700 px-1.5 py-0.5 text-[10px] uppercase text-brand-300">
                      {judgeReady.route}
                    </span>
                    <span className="text-[10px] uppercase text-gray-500">
                      {judgeReady.route_reason_code}
                    </span>
                    <span className="text-brand-300 uppercase">
                      {judgeReady.confidence}
                    </span>
                  </div>
                  <p className="text-sm text-gray-200 leading-relaxed">
                    {judgeReady.effective_request}
                  </p>
                  <div className="grid grid-cols-2 gap-2 text-xs text-gray-400">
                    <div>goal concrete: {String(judgeReady.goal_is_concrete)}</div>
                    <div>constraints stable: {String(judgeReady.constraints_are_stable)}</div>
                    <div>history resolves refs: {String(judgeReady.history_resolves_references)}</div>
                    <div>repo grounding needed: {String(judgeReady.repository_grounding_needed)}</div>
                    <div>prior scope reusable: {String(judgeReady.prior_scope_can_be_reused)}</div>
                  </div>
                  {judgeReady.skip_scoper_criteria_met.length > 0 && (
                    <div>
                      <p className="mb-1 text-[10px] uppercase tracking-wide text-gray-500">
                        Skip-Scoper criteria met
                      </p>
                      <ul className="space-y-1 text-xs text-gray-300">
                        {judgeReady.skip_scoper_criteria_met.map((item) => (
                          <li key={item}>• {item}</li>
                        ))}
                      </ul>
                    </div>
                  )}
                  {(judgeReady.missing_information.length > 0 ||
                    judgeReady.clarifying_questions.length > 0) && (
                    <div className="rounded-md border border-yellow-900/60 bg-yellow-950/20 px-3 py-2">
                      <ul className="space-y-1 text-xs text-yellow-100">
                        {judgeReady.missing_information.map((item) => (
                          <li key={item}>• {item}</li>
                        ))}
                        {judgeReady.clarifying_questions.map((item) => (
                          <li key={item}>? {item}</li>
                        ))}
                      </ul>
                    </div>
                  )}
                </div>
              )}
              {role === "scoper" && scopeReady && <ScopeCard scope={scopeReady} />}
              {/* Show execution plan as a todo list under the planner row */}
              {role === "planner" && planReady && (
                <PlanTodoList
                  phases={planReady.phases}
                  complexity={planReady.complexity}
                />
              )}
              {/* Show structured risk assessment under the risk row */}
              {role === "risk" && riskAssessed && (
                <RiskCard ev={riskAssessed} />
              )}
              {/* Show structured review under the reviewer row */}
              {role === "reviewer" && reviewCompleted && (
                <ReviewCard ev={reviewCompleted} />
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
