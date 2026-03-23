import { useState, useRef, useEffect, useCallback } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import PipelineRunView, {
  type PipelineEventDto,
  type PipelineDoneEvent,
} from "@/components/PipelineRunView";
import RunHistoryPanel from "@/components/RunHistoryPanel";
import SessionListPanel, { type ConversationTurn } from "@/components/SessionListPanel";
import DriftModal, { type DriftDetectedPayload } from "@/components/DriftModal";
import ClarificationModal, {
  type ClarificationPayload,
} from "@/components/ClarificationModal";
import SettingsPanel from "@/components/SettingsPanel";
import CommandResultView, { type CommandResult } from "@/components/CommandResultView";

const CURRENT_SESSION_STORAGE_KEY = "harnesscode.currentSessionId.v1";

// ──────────────────────────────────────────────
// Chat message model
// ──────────────────────────────────────────────

interface ChatMessage {
  id: string;
  /** "user" = human bubble, "agent" = pipeline card, "command" = /command result */
  type: "user" | "agent" | "command";
  content: string;
  /** Set while streaming; removed when done */
  pipelineEvents?: PipelineEventDto[];
  pipelineDone?: PipelineDoneEvent;
  /** Populated for type==="command" messages */
  commandResult?: CommandResult;
}

type ScopeReadyEvent = Extract<PipelineEventDto, { type: "scope_ready" }>;
type PlanReadyEvent = Extract<PipelineEventDto, { type: "plan_ready" }>;

interface SessionStatePayload {
  execution_summary: string | null;
  last_scope: ScopeReadyEvent | null;
  last_plan: PlanReadyEvent | null;
  known_relevant_files: string[];
  open_questions: string[];
}

interface RequestContextPayload {
  session_id: string | null;
  current_request: string;
  session_state: SessionStatePayload;
}

// ──────────────────────────────────────────────
// Built-in /command parser + executor
// ──────────────────────────────────────────────

type BuiltinCmd =
  | { tag: "help" }
  | { tag: "cost" }
  | { tag: "clear" }
  | { tag: "init" }
  | { tag: "rename"; name: string | null }
  | { tag: "session_list" }
  | { tag: "session_use"; id: string | null }
  | { tag: "session_delete"; id: string }
  | { tag: "unknown"; raw: string };

function parseBuiltin(input: string): BuiltinCmd | null {
  const trimmed = input.trim();
  if (!trimmed.startsWith("/")) return null;
  const withoutSlash = trimmed.slice(1);
  const parts = withoutSlash.split(/\s+/);
  const cmd = (parts[0] ?? "").toLowerCase();
  const arg1 = parts[1] ?? null;
  const rest = parts.slice(2).join(" ") || null;
  switch (cmd) {
    case "help": case "?": return { tag: "help" };
    case "cost":            return { tag: "cost" };
    case "clear": case "reset": return { tag: "clear" };
    case "init":            return { tag: "init" };
    case "rename": {
      const fullName = withoutSlash.slice("rename".length).trim() || null;
      return { tag: "rename", name: fullName };
    }
    case "session":
      switch ((arg1 ?? "").toLowerCase()) {
        case "list":   return { tag: "session_list" };
        case "use":    return { tag: "session_use", id: rest };
        case "delete": case "rm":
          return rest
            ? { tag: "session_delete", id: rest }
            : { tag: "unknown", raw: "/session delete requires a session id" };
        default: return { tag: "unknown", raw: "Unknown /session subcommand. Try /help." };
      }
    default:
      return { tag: "unknown", raw: `Unknown command: ${trimmed}. Type /help for available commands.` };
  }
}

const HELP_TEXT: CommandResult = {
  command: "/help",
  status: "info",
  title: "Built-in commands",
  lines: [
    { variant: "kv", text: "/help              : Show this reference" },
    { variant: "kv", text: "/init              : Generate or update AGENTS.md for this project" },
    { variant: "kv", text: "/cost              : Turn count + estimated token usage" },
    { variant: "kv", text: "/clear             : Wipe current session history" },
    { variant: "kv", text: "/rename [name]     : Rename current session" },
    { variant: "kv", text: "/session list      : List all saved sessions" },
    { variant: "kv", text: "/session use [id]  : Switch to another session" },
    { variant: "kv", text: "/session delete id : Permanently delete a session" },
    { variant: "divider", text: "" },
    { variant: "dim", text: "Anything else is sent to the AI pipeline." },
  ],
};

function fmtRelTime(secs: number): string {
  const diff = Math.floor(Date.now() / 1000) - secs;
  if (diff < 120)    return "just now";
  if (diff < 3600)   return `${Math.floor(diff / 60)}m ago`;
  if (diff < 86400)  return `${Math.floor(diff / 3600)}h ago`;
  if (diff < 604800) return `${Math.floor(diff / 86400)}d ago`;
  return new Date(secs * 1000).toLocaleDateString();
}

function loadCurrentSessionId(): string {
  if (typeof window === "undefined") return "default";
  return window.localStorage.getItem(CURRENT_SESSION_STORAGE_KEY) || "default";
}

/** Convert persisted ConversationTurns to ChatMessages for display. */
function turnsToMessages(turns: ConversationTurn[]): ChatMessage[] {
  const msgs: ChatMessage[] = [];
  for (const turn of turns) {
    msgs.push({
      id: `turn-user-${turn.timestamp_secs}`,
      type: "user",
      content: turn.request,
    });
    msgs.push({
      id: `turn-agent-${turn.timestamp_secs}`,
      type: "agent",
      content: turn.response_summary,
    });
  }
  return msgs;
}


function buildRequestContext(
  messages: ChatMessage[],
  sessionId: string,
  currentRequest: string,
): RequestContextPayload {
  let lastScope: ScopeReadyEvent | null = null;
  let lastPlan: PlanReadyEvent | null = null;

  for (const message of messages) {
    for (const event of message.pipelineEvents ?? []) {
      if (event.type === "scope_ready") {
        lastScope = event;
      } else if (event.type === "plan_ready") {
        lastPlan = event;
      }
    }
  }

  const executionSummaryParts: string[] = [];
  if (lastScope) {
    executionSummaryParts.push(`Last scoped objective: ${lastScope.objective}`);
  }
  if (lastPlan?.steps.length) {
    executionSummaryParts.push(
      `Last plan steps: ${lastPlan.steps.slice(0, 3).join("; ")}`,
    );
  }

  const knownRelevantFiles = Array.from(
    new Set([
      ...(lastScope?.relevant_files ?? []),
      ...(lastPlan?.affected_files ?? []),
    ]),
  );

  return {
    session_id: sessionId,
    current_request: currentRequest,
    session_state: {
      execution_summary: executionSummaryParts.length
        ? executionSummaryParts.join("\n")
        : null,
      last_scope: lastScope,
      last_plan: lastPlan,
      known_relevant_files: knownRelevantFiles,
      open_questions:
        lastScope?.needs_user_clarification && lastScope.clarifying_questions.length
          ? lastScope.clarifying_questions
          : [],
    },
  };
}

// ──────────────────────────────────────────────
// Main App
// ──────────────────────────────────────────────

export default function App() {
  const [messages, setMessages] = useState<ChatMessage[]>([
    {
      id: "welcome",
      type: "agent",
      content:
        "👋 Welcome to HarnessCode! I'm your safe AI coding agent. Tell me what you'd like to build or fix.",
    },
  ]);
  const [input, setInput] = useState("");
  const [loading, setLoading] = useState(false);
  const [historyKey, setHistoryKey] = useState(0);
  const [sessionRefreshKey, setSessionRefreshKey] = useState(0);
  const [driftPayload, setDriftPayload] = useState<DriftDetectedPayload | null>(
    null,
  );
  const [clarificationPayload, setClarificationPayload] =
    useState<ClarificationPayload | null>(null);
  const [currentSessionId, setCurrentSessionId] = useState<string>(() =>
    loadCurrentSessionId(),
  );
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [cmdPending, setCmdPending] = useState(false);
  const bottomRef = useRef<HTMLDivElement>(null);
  const unlistenRef = useRef<UnlistenFn[]>([]);

  // Auto-scroll to bottom on new messages
  useEffect(() => {
    bottomRef.current?.scrollIntoView({ behavior: "smooth" });
  }, [messages]);

  // Cleanup event listeners on unmount
  useEffect(() => {
    return () => {
      unlistenRef.current.forEach((fn) => fn());
    };
  }, []);

  useEffect(() => {
    if (typeof window === "undefined") return;
    window.localStorage.setItem(CURRENT_SESSION_STORAGE_KEY, currentSessionId);
  }, [currentSessionId]);

  /** Switch to a different session and restore its conversation history. */
  const switchSession = useCallback(
    (sessionId: string, turns: ConversationTurn[]) => {
      setCurrentSessionId(sessionId);
      if (turns.length > 0) {
        setMessages(turnsToMessages(turns));
      } else {
        setMessages([
          {
            id: "welcome",
            type: "agent",
            content: "👋 Welcome to HarnessCode! I'm your safe AI coding agent. Tell me what you'd like to build or fix.",
          },
        ]);
      }
      setHistoryKey((k) => k + 1);
    },
    [],
  );

  // ── Built-in /command executor ──────────────────────────────────────────
  const handleCommand = useCallback(
    async (raw: string, parsed: BuiltinCmd) => {
      const cmdMsgId = crypto.randomUUID();

      type MemTurn = { request: string; response_summary: string; timestamp_secs: number };
      type MemFull = { conversation_turns: MemTurn[]; compacted_summary: string | null; title: string | null };
      type SessionSummary = { session_id: string; title: string | null; updated_at_secs: number };

      const pushResult = (result: CommandResult) => {
        setMessages((prev) => [
          ...prev,
          { id: `user-cmd-${cmdMsgId}`, type: "user" as const, content: raw },
          { id: cmdMsgId, type: "command" as const, content: "", commandResult: result },
        ]);
      };

      setCmdPending(true);
      try {
        switch (parsed.tag) {
          case "help": {
            pushResult(HELP_TEXT);
            break;
          }

          case "cost": {
            const mem = await invoke<MemFull>("get_session_memory", {
              sessionId: currentSessionId,
              projectDir: null,
            });
            const turns = mem.conversation_turns.length;
            const estTokens = mem.conversation_turns.reduce(
              (acc, t) => acc + Math.floor((t.request.length + t.response_summary.length) / 4),
              0,
            );
            const compactedTokens = mem.compacted_summary
              ? Math.floor(mem.compacted_summary.length / 4)
              : null;
            pushResult({
              command: "/cost",
              status: "info",
              title: `Session: ${mem.title ?? currentSessionId}`,
              lines: [
                { variant: "kv", text: `Turns in history   : ${turns}` },
                { variant: "kv", text: `Est. history tokens: ~${estTokens}` },
                ...(compactedTokens !== null
                  ? [{ variant: "kv" as const, text: `Compacted summary  : ~${compactedTokens} tokens` }]
                  : []),
              ],
            });
            break;
          }

          case "clear": {
            await invoke("clear_session_memory", { sessionId: currentSessionId, projectDir: null });
            setMessages([{
              id: "welcome",
              type: "agent",
              content: "👋 Welcome to HarnessCode! I'm your safe AI coding agent. Tell me what you'd like to build or fix.",
            }]);
            setHistoryKey((k) => k + 1);
            setSessionRefreshKey((k) => k + 1);
            break;
          }

          case "rename": {
            const newTitle = parsed.name ?? window.prompt("New session name:");
            if (!newTitle?.trim()) {
              pushResult({ command: raw, status: "info", title: "Aborted." });
              break;
            }
            await invoke("save_session_memory", {
              sessionId: currentSessionId,
              title: newTitle.trim(),
              persistentSummary: null,
              projectDir: null,
            });
            setSessionRefreshKey((k) => k + 1);
            pushResult({ command: raw, status: "ok", title: `Session renamed to "${newTitle.trim()}"` });
            break;
          }

          case "session_list": {
            const sessions = await invoke<SessionSummary[]>("list_memory_sessions", { projectDir: null });
            if (sessions.length === 0) {
              pushResult({ command: raw, status: "info", title: "No saved sessions yet." });
            } else {
              pushResult({
                command: raw,
                status: "info",
                title: `${sessions.length} session${sessions.length === 1 ? "" : "s"}`,
                lines: [
                  { variant: "heading", text: "session id  ·  title  ·  last updated" },
                  ...sessions.map((s) => ({
                    variant: "body" as const,
                    text: `${s.session_id === currentSessionId ? "▶ " : "  "}${s.session_id.slice(0, 22).padEnd(22)}  ${(s.title ?? "—").slice(0, 24).padEnd(24)}  ${fmtRelTime(s.updated_at_secs)}`,
                  })),
                ],
              });
            }
            break;
          }

          case "session_use": {
            const targetId = parsed.id ?? window.prompt("Session id (leave blank to cancel):");
            if (!targetId?.trim()) {
              pushResult({ command: raw, status: "info", title: "Aborted." });
              break;
            }
            const trimmedId = targetId.trim();
            if (trimmedId === currentSessionId) {
              pushResult({ command: raw, status: "info", title: `Already on session "${currentSessionId}".` });
              break;
            }
            const mem = await invoke<MemFull>("get_session_memory", {
              sessionId: trimmedId,
              projectDir: null,
            });
            switchSession(trimmedId, mem.conversation_turns);
            pushResult({ command: raw, status: "ok", title: `Switched to session "${trimmedId}"` });
            break;
          }

          case "session_delete": {
            if (!window.confirm(`Delete session "${parsed.id}"? This cannot be undone.`)) {
              pushResult({ command: raw, status: "info", title: "Aborted." });
              break;
            }
            await invoke("delete_session_memory", { sessionId: parsed.id, projectDir: null });
            if (parsed.id === currentSessionId) {
              switchSession("default", []);
            }
            setSessionRefreshKey((k) => k + 1);
            pushResult({ command: raw, status: "ok", title: `Session "${parsed.id}" deleted.` });
            break;
          }

          case "init": {
            const existed = await invoke<boolean>("generate_agents_md", { projectDir: null });
            pushResult({
              command: raw,
              status: "ok",
              title: existed ? "AGENTS.md regenerated." : "AGENTS.md created.",
              lines: [{ variant: "dim", text: "File written to the project directory." }],
            });
            break;
          }

          case "unknown": {
            pushResult({ command: raw, status: "error", title: parsed.raw });
            break;
          }
        }
      } catch (err) {
        pushResult({
          command: raw,
          status: "error",
          title: `Error: ${err instanceof Error ? err.message : String(err)}`,
        });
      } finally {
        setCmdPending(false);
      }
    },
    [currentSessionId, switchSession],
  );

  // ── AI pipeline submit ─────────────────────────────────────────────────────
  const handleSubmit = useCallback(
    async (e: React.FormEvent) => {
      e.preventDefault();
      const prompt = input.trim();
      if (!prompt || loading || cmdPending) return;

      // Intercept built-in /commands before sending to AI
      const parsed = parseBuiltin(prompt);
      if (parsed) {
        setInput("");
        await handleCommand(prompt, parsed);
        return;
      }

      const userMsgId = crypto.randomUUID();
      const agentMsgId = crypto.randomUUID();
      const requestContext = buildRequestContext(messages, currentSessionId, prompt);

      setMessages((prev) => [
        ...prev,
        { id: userMsgId, type: "user", content: prompt },
        { id: agentMsgId, type: "agent", content: "", pipelineEvents: [] },
      ]);
      setInput("");
      setLoading(true);

      // Tear down any leftover listeners from a prior run
      unlistenRef.current.forEach((fn) => fn());
      unlistenRef.current = [];

      const appendEvent = (ev: PipelineEventDto) => {
        setMessages((prev) =>
          prev.map((m) =>
            m.id === agentMsgId
              ? { ...m, pipelineEvents: [...(m.pipelineEvents ?? []), ev] }
              : m,
          ),
        );
      };

      const ul1 = await listen<PipelineEventDto>("pipeline:event", (e) => {
        const ev = e.payload;
        if (ev.type === "drift_detected") {
          // Show the drift modal; don't add it to the pipeline event stream.
          setDriftPayload(ev as unknown as DriftDetectedPayload);
        } else if (ev.type === "clarification_requested") {
          setClarificationPayload({
            source: ev.source,
            objective: ev.objective,
            questions: ev.questions,
          });
        } else {
          appendEvent(ev);
        }
      });

      const ul2 = await listen<PipelineDoneEvent>("pipeline:done", (e) => {
        const done = e.payload;
        setMessages((prev) =>
          prev.map((m) =>
            m.id === agentMsgId
              ? {
                  ...m,
                  pipelineDone: done,
                  content:
                    done.status === "ok"
                      ? `Pipeline completed — ${done.stages.length} stages passed.`
                      : `Pipeline failed: ${done.message}`,
                }
              : m,
          ),
        );
        // Refresh history sidebar and session list after run completes
        setHistoryKey((k) => k + 1);
        setSessionRefreshKey((k) => k + 1);
        setLoading(false);
        // Detach listeners; they're one-shot per run
        ul1();
        ul2();
      });

      unlistenRef.current = [ul1, ul2];

      try {
        await invoke("start_pipeline", {
          prompt,
          projectDir: null,
          requestContext,
        });
      } catch (err) {
        setMessages((prev) =>
          prev.map((m) =>
            m.id === agentMsgId
              ? {
                  ...m,
                  pipelineEvents: undefined,
                  pipelineDone: { status: "err", message: String(err) },
                  content: `Failed to start pipeline: ${err}`,
                }
              : m,
          ),
        );
        setLoading(false);
      }
    },
    [input, loading, cmdPending, messages, currentSessionId, handleCommand],
  );

  return (
    <div className="flex h-screen flex-col overflow-hidden">
      {/* ── Settings panel (slide-out) ── */}
      <SettingsPanel
        open={settingsOpen}
        currentSessionId={currentSessionId}
        onSessionChange={(id) => switchSession(id, [])}
        onSessionDeleted={() => setSessionRefreshKey((k) => k + 1)}
        onClose={() => setSettingsOpen(false)}
      />
      {/* ── Drift modal (portal-like, renders on top) ── */}
      {driftPayload && (
        <DriftModal
          payload={driftPayload}
          onClose={() => setDriftPayload(null)}
        />
      )}
      {clarificationPayload && (
        <ClarificationModal
          payload={clarificationPayload}
          onClose={() => setClarificationPayload(null)}
        />
      )}
      {/* ── Header ── */}
      <header className="flex items-center gap-3 border-b border-gray-800 bg-gray-900/80 px-6 py-4 backdrop-blur">
        <div className="flex h-8 w-8 items-center justify-center rounded-lg bg-brand-600 text-base font-bold text-white">
          HC
        </div>
        <div>
          <h1 className="text-sm font-semibold text-white">HarnessCode</h1>
          <p className="text-xs text-gray-400">
            Safe AI Coding Agent · Powered by Cybernetics
          </p>
        </div>
        <div className="ml-auto flex items-center gap-2">
          <button
            onClick={() => setSettingsOpen(true)}
            className="rounded-md p-1.5 text-gray-400 hover:bg-gray-800 hover:text-gray-200 transition-colors"
            aria-label="Settings"
            title="Settings"
          >
            <svg
              className="h-4 w-4"
              fill="none"
              viewBox="0 0 24 24"
              strokeWidth={1.5}
              stroke="currentColor"
            >
              <path
                strokeLinecap="round"
                strokeLinejoin="round"
                d="M9.594 3.94c.09-.542.56-.94 1.11-.94h2.593c.55 0 1.02.398 1.11.94l.213 1.281c.063.374.313.686.645.87.074.04.147.083.22.127.325.196.72.257 1.075.124l1.217-.456a1.125 1.125 0 0 1 1.37.49l1.296 2.247a1.125 1.125 0 0 1-.26 1.431l-1.003.827c-.293.241-.438.613-.43.992a7.723 7.723 0 0 1 0 .255c-.008.378.137.75.43.991l1.004.827c.424.35.534.955.26 1.43l-1.298 2.248a1.125 1.125 0 0 1-1.369.491l-1.217-.456c-.355-.133-.75-.072-1.076.124a6.47 6.47 0 0 1-.22.128c-.331.183-.581.495-.644.869l-.213 1.281c-.09.543-.56.94-1.11.94h-2.594c-.55 0-1.019-.398-1.11-.94l-.213-1.281c-.062-.374-.312-.686-.644-.87a6.52 6.52 0 0 1-.22-.127c-.325-.196-.72-.257-1.076-.124l-1.217.456a1.125 1.125 0 0 1-1.369-.49l-1.297-2.247a1.125 1.125 0 0 1 .26-1.431l1.004-.827c.292-.24.437-.613.43-.991a6.932 6.932 0 0 1 0-.255c.007-.38-.138-.751-.43-.992l-1.004-.827a1.125 1.125 0 0 1-.26-1.43l1.297-2.247a1.125 1.125 0 0 1 1.37-.491l1.216.456c.356.133.751.072 1.076-.124.072-.044.146-.086.22-.128.332-.183.582-.495.644-.869l.214-1.28Z"
              />
              <path
                strokeLinecap="round"
                strokeLinejoin="round"
                d="M15 12a3 3 0 1 1-6 0 3 3 0 0 1 6 0Z"
              />
            </svg>
          </button>
          <div
            className={`h-2 w-2 rounded-full ${
              loading ? "animate-pulse bg-yellow-400" : "bg-green-400"
            }`}
          />
          <span className="text-xs text-gray-400">
            {loading ? "Running…" : "Ready"}
          </span>
        </div>
      </header>

      {/* ── Body (sessions sidebar + chat + history sidebar) ── */}
      <div className="flex flex-1 overflow-hidden">
        {/* ── Sessions sidebar ── */}
        <SessionListPanel
          currentSessionId={currentSessionId}
          refreshKey={sessionRefreshKey}
          onSwitch={switchSession}
        />
        {/* ── Chat log ── */}
        <main className="flex flex-1 flex-col overflow-hidden">
          <div className="flex-1 overflow-y-auto px-4 py-6">
            <div className="mx-auto max-w-3xl space-y-4">
              {messages.map((msg) => (
                <div
                  key={msg.id}
                  className={`flex ${
                    msg.type === "user" ? "justify-end" : "justify-start"
                  }`}
                >
                  <div className="max-w-[85%] space-y-2">
                    {/* Command result card */}
                    {msg.type === "command" && msg.commandResult && (
                      <CommandResultView result={msg.commandResult} />
                    )}

                    {/* Text bubble — show when there's text and no live pipeline view */}
                    {msg.content && !msg.pipelineEvents && msg.type !== "command" && (
                      <div
                        className={`rounded-2xl px-4 py-2.5 text-sm ${
                          msg.type === "user"
                            ? "bg-brand-700 text-white rounded-br-md"
                            : "bg-gray-800 text-gray-200 rounded-bl-md"
                        }`}
                      >
                        {msg.content}
                      </div>
                    )}

                    {/* Live pipeline card — while running or on first render after start */}
                    {msg.pipelineEvents !== undefined && (
                      <PipelineRunView
                        events={msg.pipelineEvents}
                        done={msg.pipelineDone ?? null}
                        onDismiss={() =>
                          setMessages((prev) =>
                            prev.map((m) =>
                              m.id === msg.id
                                ? { ...m, pipelineEvents: undefined }
                                : m,
                            ),
                          )
                        }
                      />
                    )}
                  </div>
                </div>
              ))}
              <div ref={bottomRef} />
            </div>
          </div>

          {/* ── Input ── */}
          <div className="border-t border-gray-800 bg-gray-900/80 px-4 py-4 backdrop-blur">
            <form
              onSubmit={handleSubmit}
              className="mx-auto flex max-w-3xl gap-3 relative"
            >
              {/* Slash-command hint strip — shown while the user is typing a /command */}
              {input.startsWith("/") && !loading && (
                <div className="absolute bottom-full left-0 right-0 mb-2">
                  <div className="rounded-lg border border-sky-800/50 bg-gray-900 px-3 py-2 text-xs text-gray-400">
                    <span className="mr-2 font-semibold text-sky-400">⌘</span>
                    <span className="text-gray-300">/help</span>
                    {" · "}<span className="text-gray-300">/init</span>
                    {" · "}<span className="text-gray-300">/cost</span>
                    {" · "}<span className="text-gray-300">/clear</span>
                    {" · "}<span className="text-gray-300">/rename [name]</span>
                    {" · "}<span className="text-gray-300">/session list|use|delete</span>
                  </div>
                </div>
              )}
              <input
                className="input-text flex-1"
                placeholder={
                  loading
                    ? "Pipeline running…"
                    : cmdPending
                    ? "Command running…"
                    : "Describe what to build, or type / for commands…"
                }
                value={input}
                onChange={(e) => setInput(e.target.value)}
                disabled={loading || cmdPending}
              />
              <button
                type="submit"
                className={`btn-primary ${input.startsWith("/") ? "!bg-sky-700 hover:!bg-sky-600" : ""}`}
                disabled={loading || cmdPending || !input.trim()}
              >
                {loading ? "Running…" : cmdPending ? "…" : input.startsWith("/") ? "Run cmd" : "Run"}
              </button>
            </form>
          </div>
        </main>

        {/* ── History sidebar ── */}
        <RunHistoryPanel refreshKey={historyKey} />
      </div>
    </div>
  );
}
