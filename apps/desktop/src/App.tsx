import { useState, useRef, useEffect, useCallback } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import PipelineRunView, {
  type PipelineEventDto,
  type PipelineDoneEvent,
} from "@/components/PipelineRunView";
import RunHistoryPanel from "@/components/RunHistoryPanel";
import DriftModal, { type DriftDetectedPayload } from "@/components/DriftModal";
import SettingsPanel from "@/components/SettingsPanel";

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
  const [driftPayload, setDriftPayload] = useState<DriftDetectedPayload | null>(null);
  const [settingsOpen, setSettingsOpen] = useState(false);
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
        const ev = e.payload;
        if (ev.type === "drift_detected") {
          // Show the drift modal; don't add it to the pipeline event stream.
          setDriftPayload(ev as unknown as DriftDetectedPayload);
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
      {/* ── Settings panel (slide-out) ── */}
      <SettingsPanel
        open={settingsOpen}
        onClose={() => setSettingsOpen(false)}
      />
      {/* ── Drift modal (portal-like, renders on top) ── */}
      {driftPayload && (
        <DriftModal
          payload={driftPayload}
          onClose={() => setDriftPayload(null)}
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
