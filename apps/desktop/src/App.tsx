import { useState, useRef, useEffect, useCallback } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import PipelineRunView, {
  type PipelineEventDto,
  type PipelineDoneEvent,
} from "@/components/PipelineRunView";
import RunHistoryPanel from "@/components/RunHistoryPanel";

// ──────────────────────────────────────────────
// Chat message model
// ──────────────────────────────────────────────

interface ChatMessage {
  id: string;
  type: "user" | "agent";
  content: string;
  /** Set while streaming; removed when done */
  pipelineEvents?: PipelineEventDto[];
  pipelineDone?: PipelineDoneEvent;
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

  const handleSubmit = useCallback(
    async (e: React.FormEvent) => {
      e.preventDefault();
      const prompt = input.trim();
      if (!prompt || loading) return;

      const userMsgId = crypto.randomUUID();
      const agentMsgId = crypto.randomUUID();

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
              : m
          )
        );
      };

      const ul1 = await listen<PipelineEventDto>("pipeline:event", (e) => {
        appendEvent(e.payload);
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
              : m
          )
        );
        // Refresh history sidebar after run completes
        setHistoryKey((k) => k + 1);
        setLoading(false);
        // Detach listeners; they're one-shot per run
        ul1();
        ul2();
      });

      unlistenRef.current = [ul1, ul2];

      try {
        await invoke("start_pipeline", { prompt, projectDir: null });
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
              : m
          )
        );
        setLoading(false);
      }
    },
    [input, loading]
  );

  return (
    <div className="flex h-screen flex-col overflow-hidden">
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
        <div className="ml-auto flex items-center gap-1.5">
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

      {/* ── Body (chat + history sidebar) ── */}
      <div className="flex flex-1 overflow-hidden">
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
                    {/* Text bubble — show when there's text and no live pipeline view */}
                    {msg.content && !msg.pipelineEvents && (
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
                                : m
                            )
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
              className="mx-auto flex max-w-3xl gap-3"
            >
              <input
                className="input-text flex-1"
                placeholder={
                  loading
                    ? "Pipeline running…"
                    : "Describe what you'd like to build or fix…"
                }
                value={input}
                onChange={(e) => setInput(e.target.value)}
                disabled={loading}
              />
              <button
                type="submit"
                className="btn-primary"
                disabled={loading || !input.trim()}
              >
                {loading ? "Running…" : "Run"}
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
