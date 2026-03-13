import { useState, useRef, useEffect } from "react";
import { invoke } from "@tauri-apps/api/core";

// ──────────────────────────────────────────────
// Types that mirror the Rust AgentTaskResponse enum
// ──────────────────────────────────────────────

interface AgentOutput {
  role: "planner" | "coder" | "reviewer";
  summary: string;
  payload: unknown;
  success: boolean;
}

type AgentTaskResponse =
  | { card_type: "code_diff"; outputs: AgentOutput[]; summary: string }
  | { card_type: "risk_alert"; filepath: string; reason: string; blocked: boolean }
  | { card_type: "error"; message: string };

// ──────────────────────────────────────────────
// Message model for the chat log
// ──────────────────────────────────────────────

interface ChatMessage {
  id: string;
  type: "user" | "agent";
  content: string;
  response?: AgentTaskResponse;
  loading?: boolean;
}

// ──────────────────────────────────────────────
// Generative UI Cards
// ──────────────────────────────────────────────

function CodeDiffCard({
  outputs,
  summary,
}: {
  outputs: AgentOutput[];
  summary: string;
}) {
  return (
    <div className="card border-brand-800 bg-gray-900/80 space-y-3">
      <div className="flex items-center gap-2">
        <span className="text-green-400 text-lg">✅</span>
        <h3 className="font-semibold text-brand-300">Pipeline Complete</h3>
      </div>
      <p className="text-sm text-gray-400">{summary}</p>
      <div className="space-y-2">
        {outputs.map((o) => (
          <div
            key={o.role}
            className="flex items-start gap-3 rounded-lg bg-gray-800 p-3 text-sm"
          >
            <span className="mt-0.5 text-base">
              {o.role === "planner" ? "🧠" : o.role === "coder" ? "💻" : "🔍"}
            </span>
            <div>
              <span className="font-medium capitalize text-brand-300">
                {o.role}
              </span>
              <p className="text-gray-300">{o.summary}</p>
            </div>
            <span className="ml-auto text-xs">
              {o.success ? (
                <span className="text-green-400">passed</span>
              ) : (
                <span className="text-red-400">failed</span>
              )}
            </span>
          </div>
        ))}
      </div>
    </div>
  );
}

function RiskAlertCard({
  filepath,
  reason,
  blocked,
}: {
  filepath: string;
  reason: string;
  blocked: boolean;
}) {
  return (
    <div
      className={`card space-y-2 ${
        blocked ? "border-red-800 bg-red-950/40" : "border-yellow-800 bg-yellow-950/30"
      }`}
    >
      <div className="flex items-center gap-2">
        <span className="text-lg">{blocked ? "🚫" : "⚠️"}</span>
        <h3
          className={`font-semibold ${
            blocked ? "text-red-400" : "text-yellow-400"
          }`}
        >
          {blocked ? "HIGH RISK — Operation Blocked" : "Risk Warning"}
        </h3>
      </div>
      <p className="text-sm text-gray-300">
        File:{" "}
        <code className="rounded bg-gray-800 px-1 py-0.5 text-xs text-brand-300">
          {filepath}
        </code>
      </p>
      <p className="text-sm text-gray-400">{reason}</p>
      {blocked && (
        <p className="text-xs text-red-400">
          Explicit confirmation required before proceeding.
        </p>
      )}
    </div>
  );
}

function ErrorCard({ message }: { message: string }) {
  return (
    <div className="card border-red-800 bg-red-950/30 space-y-1">
      <div className="flex items-center gap-2">
        <span className="text-lg">💥</span>
        <h3 className="font-semibold text-red-400">Error</h3>
      </div>
      <p className="text-sm text-gray-400">{message}</p>
    </div>
  );
}

function LoadingCard() {
  return (
    <div className="card border-brand-900 bg-gray-900/60 space-y-2">
      <div className="flex items-center gap-3">
        <div className="h-4 w-4 animate-spin rounded-full border-2 border-brand-400 border-t-transparent" />
        <span className="text-sm text-brand-300">
          Running multi-agent pipeline…
        </span>
      </div>
      <div className="space-y-1.5 pl-7 text-xs text-gray-500">
        <p>🧠 Planner is analysing the task…</p>
        <p>💻 Coder is generating changes…</p>
        <p>🔍 Reviewer is validating output…</p>
      </div>
    </div>
  );
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
  const bottomRef = useRef<HTMLDivElement>(null);

  // Auto-scroll to bottom on new messages
  useEffect(() => {
    bottomRef.current?.scrollIntoView({ behavior: "smooth" });
  }, [messages]);

  async function handleSubmit(e: React.FormEvent) {
    e.preventDefault();
    const prompt = input.trim();
    if (!prompt || loading) return;

    const userMsgId = crypto.randomUUID();
    const agentMsgId = crypto.randomUUID();

    // Append user message + loading placeholder
    setMessages((prev) => [
      ...prev,
      { id: userMsgId, type: "user", content: prompt },
      { id: agentMsgId, type: "agent", content: "", loading: true },
    ]);
    setInput("");
    setLoading(true);

    try {
      const response = await invoke<AgentTaskResponse>("invoke_agent_task", {
        prompt,
      });

      setMessages((prev) =>
        prev.map((m) =>
          m.id === agentMsgId
            ? {
                ...m,
                loading: false,
                content: getResponseSummary(response),
                response,
              }
            : m
        )
      );
    } catch (err) {
      setMessages((prev) =>
        prev.map((m) =>
          m.id === agentMsgId
            ? {
                ...m,
                loading: false,
                content: "An unexpected error occurred.",
                response: {
                  card_type: "error",
                  message: String(err),
                },
              }
            : m
        )
      );
    } finally {
      setLoading(false);
    }
  }

  function getResponseSummary(r: AgentTaskResponse): string {
    switch (r.card_type) {
      case "code_diff":
        return r.summary;
      case "risk_alert":
        return r.blocked
          ? `Operation blocked: high-risk file '${r.filepath}'.`
          : `Risk warning for '${r.filepath}'.`;
      case "error":
        return r.message;
    }
  }

  return (
    <div className="flex h-screen flex-col">
      {/* ── Header ── */}
      <header className="flex items-center gap-3 border-b border-gray-800 bg-gray-900/80 px-6 py-4 backdrop-blur">
        <div className="flex h-8 w-8 items-center justify-center rounded-lg bg-brand-600 text-base font-bold text-white">
          HC
        </div>
        <div>
          <h1 className="text-sm font-semibold text-white">HarnessCode</h1>
          <p className="text-xs text-gray-400">Safe AI Coding Agent · Powered by Cybernetics</p>
        </div>
        <div className="ml-auto flex items-center gap-1.5">
          <div className="h-2 w-2 rounded-full bg-green-400 animate-pulse" />
          <span className="text-xs text-gray-400">Online</span>
        </div>
      </header>

      {/* ── Chat log ── */}
      <main className="flex-1 overflow-y-auto px-4 py-6 space-y-4">
        <div className="mx-auto max-w-3xl space-y-4">
          {messages.map((msg) => (
            <div
              key={msg.id}
              className={`flex ${msg.type === "user" ? "justify-end" : "justify-start"}`}
            >
              <div
                className={`max-w-[80%] space-y-2 ${
                  msg.type === "user" ? "items-end" : "items-start"
                }`}
              >
                {/* Bubble */}
                {(msg.loading || msg.content) && (
                  <div
                    className={`rounded-2xl px-4 py-2.5 text-sm ${
                      msg.type === "user"
                        ? "bg-brand-700 text-white rounded-br-md"
                        : "bg-gray-800 text-gray-200 rounded-bl-md"
                    }`}
                  >
                    {msg.loading ? (
                      <span className="text-gray-400 italic">Thinking…</span>
                    ) : (
                      msg.content
                    )}
                  </div>
                )}

                {/* Generative UI card */}
                {msg.loading && <LoadingCard />}
                {!msg.loading && msg.response && (
                  <div className="w-full">
                    {msg.response.card_type === "code_diff" && (
                      <CodeDiffCard
                        outputs={msg.response.outputs}
                        summary={msg.response.summary}
                      />
                    )}
                    {msg.response.card_type === "risk_alert" && (
                      <RiskAlertCard
                        filepath={msg.response.filepath}
                        reason={msg.response.reason}
                        blocked={msg.response.blocked}
                      />
                    )}
                    {msg.response.card_type === "error" && (
                      <ErrorCard message={msg.response.message} />
                    )}
                  </div>
                )}
              </div>
            </div>
          ))}
          <div ref={bottomRef} />
        </div>
      </main>

      {/* ── Input area ── */}
      <footer className="border-t border-gray-800 bg-gray-900/80 px-4 py-4 backdrop-blur">
        <form
          onSubmit={handleSubmit}
          className="mx-auto flex max-w-3xl items-end gap-3"
        >
          <textarea
            value={input}
            onChange={(e) => setInput(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && !e.shiftKey) {
                e.preventDefault();
                handleSubmit(e as unknown as React.FormEvent);
              }
            }}
            placeholder="What do you want to build or fix today? (Enter to send, Shift+Enter for newline)"
            rows={2}
            className="input-text resize-none"
            disabled={loading}
          />
          <button
            type="submit"
            disabled={!input.trim() || loading}
            className="btn-primary flex-shrink-0 px-5 py-3"
          >
            {loading ? (
              <div className="h-4 w-4 animate-spin rounded-full border-2 border-white border-t-transparent" />
            ) : (
              <span>Send</span>
            )}
          </button>
        </form>
        <p className="mx-auto mt-2 max-w-3xl text-center text-xs text-gray-600">
          HarnessCode v0.1.0 · All operations are sandboxed and risk-checked.
        </p>
      </footer>
    </div>
  );
}
