/**
 * SettingsPanel — slide-out settings panel for global configuration and
 * session-memory management.
 */

import { useEffect, useState, useCallback } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Separator } from "@/components/ui/separator";

// ── Types ─────────────────────────────────────────────────────────────────────

interface ConfigDto {
  default_profile: string | null;
  max_tool_turns: number | null;
  profiles: Array<{
    name: string;
    provider: string;
    model: string;
    base_url: string | null;
    has_api_key: boolean;
  }>;
}

interface SessionMemorySummary {
  session_id: string;
  title: string | null;
  updated_at_secs: number;
}

interface SessionMemory {
  session_id: string;
  title: string | null;
  persistent_summary: string | null;
  clarified_facts: string[];
  effective_requests: string[];
}

// ── Component ─────────────────────────────────────────────────────────────────

interface Props {
  open: boolean;
  currentSessionId: string;
  onSessionChange: (sessionId: string) => void;
  onClose: () => void;
}

export default function SettingsPanel({
  open,
  currentSessionId,
  onSessionChange,
  onClose,
}: Props) {
  const [maxToolTurns, setMaxToolTurns] = useState<string>("100");
  const [sessionIdInput, setSessionIdInput] = useState(currentSessionId);
  const [persistentSummary, setPersistentSummary] = useState("");
  const [sessionTitle, setSessionTitle] = useState("");
  const [sessions, setSessions] = useState<SessionMemorySummary[]>([]);
  const [clarifiedFacts, setClarifiedFacts] = useState<string[]>([]);
  const [effectiveRequests, setEffectiveRequests] = useState<string[]>([]);
  const [saving, setSaving] = useState(false);
  const [saved, setSaved] = useState(false);

  // Load current config when opened
  useEffect(() => {
    if (!open) return;
    Promise.all([
      invoke<ConfigDto>("get_config"),
      invoke<SessionMemorySummary[]>("list_memory_sessions", { projectDir: null }),
      invoke<SessionMemory>("get_session_memory", {
        sessionId: currentSessionId,
        projectDir: null,
      }),
    ])
      .then(([cfg, listedSessions, memory]) => {
        setMaxToolTurns(String(cfg.max_tool_turns ?? 100));
        setSessions(listedSessions);
        setSessionIdInput(currentSessionId);
        setSessionTitle(memory.title ?? "");
        setPersistentSummary(memory.persistent_summary ?? "");
        setClarifiedFacts(memory.clarified_facts ?? []);
        setEffectiveRequests(memory.effective_requests ?? []);
        setSaved(false);
      })
      .catch(() => {});
  }, [open, currentSessionId]);

  const handleSave = useCallback(async () => {
    setSaving(true);
    try {
      const parsed = parseInt(maxToolTurns, 10);
      const value = isNaN(parsed) || parsed <= 0 ? null : parsed;
      await invoke("save_settings", { maxToolTurns: value });
      setSaved(true);
      setTimeout(() => setSaved(false), 2000);
    } catch {
      // silently ignore save errors for now
    } finally {
      setSaving(false);
    }
  }, [maxToolTurns]);

  const handleSwitchSession = useCallback(async () => {
    const nextSessionId = sessionIdInput.trim() || "default";
    onSessionChange(nextSessionId);
    try {
      const memory = await invoke<SessionMemory>("get_session_memory", {
        sessionId: nextSessionId,
        projectDir: null,
      });
      setSessionTitle(memory.title ?? "");
      setPersistentSummary(memory.persistent_summary ?? "");
      setClarifiedFacts(memory.clarified_facts ?? []);
      setEffectiveRequests(memory.effective_requests ?? []);
      const listedSessions = await invoke<SessionMemorySummary[]>(
        "list_memory_sessions",
        { projectDir: null },
      );
      setSessions(listedSessions);
    } catch {
      // ignore
    }
  }, [onSessionChange, sessionIdInput]);

  const handleSaveSessionMemory = useCallback(async () => {
    try {
      const memory = await invoke<SessionMemory>("save_session_memory", {
        sessionId: currentSessionId,
        title: sessionTitle || null,
        persistentSummary: persistentSummary || null,
        projectDir: null,
      });
      setSessionTitle(memory.title ?? "");
      setPersistentSummary(memory.persistent_summary ?? "");
      const listedSessions = await invoke<SessionMemorySummary[]>(
        "list_memory_sessions",
        { projectDir: null },
      );
      setSessions(listedSessions);
    } catch {
      // ignore
    }
  }, [currentSessionId, persistentSummary, sessionTitle]);

  const handleClearSession = useCallback(async () => {
    try {
      const memory = await invoke<SessionMemory>("clear_session_memory", {
        sessionId: currentSessionId,
        projectDir: null,
      });
      setSessionTitle(memory.title ?? "");
      setPersistentSummary(memory.persistent_summary ?? "");
      setClarifiedFacts(memory.clarified_facts ?? []);
      setEffectiveRequests(memory.effective_requests ?? []);
      const listedSessions = await invoke<SessionMemorySummary[]>(
        "list_memory_sessions",
        { projectDir: null },
      );
      setSessions(listedSessions);
    } catch {
      // ignore
    }
  }, [currentSessionId]);

  if (!open) return null;

  return (
    <>
      {/* Backdrop */}
      <div
        className="fixed inset-0 z-40 bg-black/40 backdrop-blur-sm"
        onClick={onClose}
      />

      {/* Panel */}
      <div className="fixed right-0 top-0 z-50 flex h-full w-80 flex-col border-l border-gray-800 bg-gray-950 shadow-2xl">
        {/* Header */}
        <div className="flex items-center justify-between px-5 py-4">
          <h2 className="text-sm font-semibold text-white">Settings</h2>
          <button
            onClick={onClose}
            className="rounded-md p-1 text-gray-400 hover:bg-gray-800 hover:text-gray-200 transition-colors"
            aria-label="Close settings"
          >
            <svg
              className="h-4 w-4"
              fill="none"
              viewBox="0 0 24 24"
              strokeWidth={2}
              stroke="currentColor"
            >
              <path
                strokeLinecap="round"
                strokeLinejoin="round"
                d="M6 18L18 6M6 6l12 12"
              />
            </svg>
          </button>
        </div>
        <Separator className="bg-gray-800" />

        {/* Body */}
        <div className="flex-1 overflow-y-auto px-5 py-5 space-y-6">
          {/* ── Guardrails section ── */}
          <section>
            <h3 className="mb-3 text-xs font-semibold uppercase tracking-wider text-gray-500">
              Guardrails
            </h3>

            <div className="space-y-2">
              <label
                htmlFor="maxToolTurns"
                className="block text-sm text-gray-300"
              >
                Max tool turns
              </label>
              <p className="text-xs text-gray-500 leading-relaxed">
                Maximum number of tool-call rounds before the agent loop is
                terminated. The agent will be warned at 80% of this budget.
              </p>
              <input
                id="maxToolTurns"
                type="number"
                min={1}
                max={10000}
                className="input-text w-full"
                value={maxToolTurns}
                onChange={(e) => {
                  setMaxToolTurns(e.target.value);
                  setSaved(false);
                }}
              />
            </div>
          </section>

          <section>
            <h3 className="mb-3 text-xs font-semibold uppercase tracking-wider text-gray-500">
              Session Memory
            </h3>

            <div className="space-y-3">
              <div className="space-y-2">
                <label className="block text-sm text-gray-300">Current session</label>
                <div className="flex gap-2">
                  <input
                    className="input-text w-full"
                    value={sessionIdInput}
                    onChange={(e) => setSessionIdInput(e.target.value)}
                    placeholder="default"
                  />
                  <button className="btn-primary whitespace-nowrap" onClick={handleSwitchSession}>
                    Use
                  </button>
                </div>
                <p className="text-xs text-gray-500 leading-relaxed">
                  Session memory is now stored in the core memory layer and shared across desktop and CLI when they use the same session id.
                </p>
              </div>

              {sessions.length > 0 && (
                <div className="space-y-2">
                  <p className="text-xs uppercase tracking-wide text-gray-500">Known sessions</p>
                  <div className="flex flex-wrap gap-2">
                    {sessions.map((session) => (
                      <button
                        key={session.session_id}
                        onClick={() => {
                          setSessionIdInput(session.session_id);
                          onSessionChange(session.session_id);
                        }}
                        className={`rounded-md border px-2 py-1 text-xs transition ${
                          session.session_id === currentSessionId
                            ? "border-brand-500 bg-brand-900/30 text-brand-200"
                            : "border-gray-700 text-gray-400 hover:bg-gray-900"
                        }`}
                      >
                        {session.title || session.session_id}
                      </button>
                    ))}
                  </div>
                </div>
              )}

              <div className="space-y-2">
                <label className="block text-sm text-gray-300">Session title</label>
                <input
                  className="input-text w-full"
                  value={sessionTitle}
                  onChange={(e) => setSessionTitle(e.target.value)}
                  placeholder="Optional human-readable title"
                />
              </div>

              <div className="space-y-2">
                <label className="block text-sm text-gray-300">Persistent summary</label>
                <textarea
                  className="min-h-28 w-full rounded-xl border border-gray-700 bg-gray-950 px-3 py-2 text-sm text-gray-100 outline-none transition focus:border-brand-500"
                  value={persistentSummary}
                  onChange={(e) => setPersistentSummary(e.target.value)}
                  placeholder="Editable long-lived summary for this session"
                />
              </div>

              {clarifiedFacts.length > 0 && (
                <div>
                  <p className="mb-1 text-xs uppercase tracking-wide text-gray-500">Clarified facts</p>
                  <ul className="space-y-1 text-xs text-gray-400">
                    {clarifiedFacts.map((fact) => (
                      <li key={fact}>• {fact}</li>
                    ))}
                  </ul>
                </div>
              )}

              {effectiveRequests.length > 0 && (
                <div>
                  <p className="mb-1 text-xs uppercase tracking-wide text-gray-500">Recent effective requests</p>
                  <ul className="space-y-1 text-xs text-gray-400">
                    {effectiveRequests.map((request) => (
                      <li key={request}>• {request}</li>
                    ))}
                  </ul>
                </div>
              )}

              <div className="flex gap-2">
                <button className="btn-primary" onClick={handleSaveSessionMemory}>
                  Save Summary
                </button>
                <button
                  className="rounded-md border border-red-800 px-3 py-2 text-sm text-red-300 transition hover:bg-red-950/40"
                  onClick={handleClearSession}
                >
                  Clear Session
                </button>
              </div>
            </div>
          </section>
        </div>

        {/* Footer */}
        <Separator className="bg-gray-800" />
        <div className="flex items-center justify-between px-5 py-4">
          {saved && <span className="text-xs text-green-400">✓ Saved</span>}
          {!saved && <span />}
          <button
            className="btn-primary"
            onClick={handleSave}
            disabled={saving}
          >
            {saving ? "Saving…" : "Save"}
          </button>
        </div>
      </div>
    </>
  );
}
