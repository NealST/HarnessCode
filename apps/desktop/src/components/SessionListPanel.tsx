/**
 * SessionListPanel — left sidebar listing saved sessions.
 *
 * Displays all sessions ordered by last-updated. Supports:
 *  - switching to a session by clicking it (restores chat messages from turns)
 *  - creating a new session via the "+" button
 *  - deleting a session via the trash icon
 */

import { useEffect, useState, useCallback } from "react";
import { invoke } from "@tauri-apps/api/core";
import { ScrollArea } from "@/components/ui/scroll-area";
import { Separator } from "@/components/ui/separator";

// ── Types ─────────────────────────────────────────────────────────────────────

export interface SessionMemorySummary {
  session_id: string;
  title: string | null;
  updated_at_secs: number;
}

export interface ConversationTurn {
  request: string;
  response_summary: string;
  timestamp_secs: number;
}

interface SessionMemoryFull {
  session_id: string;
  title: string | null;
  conversation_turns: ConversationTurn[];
}

// ── Helpers ───────────────────────────────────────────────────────────────────

function fmtRelativeTime(secs: number): string {
  const now = Math.floor(Date.now() / 1000);
  const diff = now - secs;
  if (diff < 120) return "just now";
  if (diff < 3600) return `${Math.floor(diff / 60)}m ago`;
  if (diff < 86400) return `${Math.floor(diff / 3600)}h ago`;
  if (diff < 604800) return `${Math.floor(diff / 86400)}d ago`;
  return new Date(secs * 1000).toLocaleDateString();
}

function truncate(s: string, max = 28) {
  return s.length > max ? s.slice(0, max - 1) + "…" : s;
}

// ── SessionListPanel ──────────────────────────────────────────────────────────

interface Props {
  currentSessionId: string;
  refreshKey?: number;
  projectDir?: string;
  onSwitch: (sessionId: string, turns: ConversationTurn[]) => void;
}

export default function SessionListPanel({
  currentSessionId,
  refreshKey = 0,
  projectDir,
  onSwitch,
}: Props) {
  const [sessions, setSessions] = useState<SessionMemorySummary[]>([]);
  const [loading, setLoading] = useState(false);
  const [deletingId, setDeletingId] = useState<string | null>(null);

  const load = useCallback(() => {
    setLoading(true);
    invoke<SessionMemorySummary[]>("list_memory_sessions", {
      projectDir: projectDir ?? null,
    })
      .then(setSessions)
      .catch(() => setSessions([]))
      .finally(() => setLoading(false));
  }, [projectDir]);

  useEffect(() => {
    load();
  }, [load, refreshKey, currentSessionId]);

  const handleSwitch = useCallback(
    async (sessionId: string) => {
      if (sessionId === currentSessionId) return;
      try {
        const mem = await invoke<SessionMemoryFull>("get_session_memory", {
          sessionId,
          projectDir: projectDir ?? null,
        });
        onSwitch(sessionId, mem.conversation_turns ?? []);
      } catch {
        // Still switch even if we can't load history
        onSwitch(sessionId, []);
      }
    },
    [currentSessionId, onSwitch, projectDir],
  );

  const handleNewSession = useCallback(() => {
    const newId = `session-${Date.now()}`;
    onSwitch(newId, []);
  }, [onSwitch]);

  const handleDelete = useCallback(
    async (e: React.MouseEvent, sessionId: string) => {
      e.stopPropagation();
      if (!confirm(`Delete session "${sessionId}"? This cannot be undone.`)) return;
      setDeletingId(sessionId);
      try {
        await invoke("delete_session_memory", {
          sessionId,
          projectDir: projectDir ?? null,
        });
        // If we deleted the active session, switch to default
        if (sessionId === currentSessionId) {
          onSwitch("default", []);
        }
        load();
      } catch (err) {
        alert(`Failed to delete session "${sessionId}": ${err instanceof Error ? err.message : String(err)}`);
      } finally {
        setDeletingId(null);
      }
    },
    [currentSessionId, load, onSwitch, projectDir],
  );

  return (
    <aside className="flex h-full w-52 flex-col border-r border-gray-800 bg-gray-950">
      {/* Header */}
      <div className="flex items-center justify-between px-3 py-3">
        <h2 className="text-xs font-semibold uppercase tracking-wider text-gray-500">
          Sessions
        </h2>
        <div className="flex items-center gap-1">
          {loading && (
            <div className="h-3 w-3 animate-spin rounded-full border border-gray-500 border-t-transparent" />
          )}
          {/* New session button */}
          <button
            onClick={handleNewSession}
            className="rounded p-1 text-gray-500 hover:bg-gray-800 hover:text-gray-300 transition-colors"
            title="New session"
            aria-label="New session"
          >
            <svg className="h-3.5 w-3.5" fill="none" viewBox="0 0 24 24" strokeWidth={2} stroke="currentColor">
              <path strokeLinecap="round" strokeLinejoin="round" d="M12 4.5v15m7.5-7.5h-15" />
            </svg>
          </button>
        </div>
      </div>
      <Separator className="bg-gray-800" />

      <ScrollArea className="flex-1">
        {sessions.length === 0 && !loading ? (
          <p className="px-3 py-5 text-center text-xs text-gray-600">
            No sessions yet
          </p>
        ) : (
          <div className="py-1">
            {sessions.map((s) => {
              const isActive = s.session_id === currentSessionId;
              const label = truncate(s.title || s.session_id);
              return (
                <div
                  key={s.session_id}
                  onClick={() => handleSwitch(s.session_id)}
                  className={`group relative flex cursor-pointer items-start gap-2 px-3 py-2.5 transition-colors ${
                    isActive
                      ? "bg-brand-900/30 text-brand-200"
                      : "text-gray-400 hover:bg-gray-900 hover:text-gray-200"
                  }`}
                >
                  {/* Active indicator */}
                  {isActive && (
                    <span className="mt-0.5 h-1.5 w-1.5 flex-shrink-0 rounded-full bg-brand-400" />
                  )}
                  {!isActive && <span className="mt-0.5 h-1.5 w-1.5 flex-shrink-0" />}

                  <div className="min-w-0 flex-1">
                    <p className="truncate text-xs font-medium leading-tight">{label}</p>
                    <p className="mt-0.5 text-xs text-gray-600">
                      {fmtRelativeTime(s.updated_at_secs)}
                    </p>
                  </div>

                  {/* Delete button — only on hover, hidden for active to avoid accidents */}
                  {!isActive && (
                    <button
                      onClick={(e) => handleDelete(e, s.session_id)}
                      disabled={deletingId === s.session_id}
                      className="invisible absolute right-2 top-2 rounded p-0.5 text-gray-600 hover:bg-gray-800 hover:text-red-400 group-hover:visible transition-colors"
                      title={`Delete ${s.session_id}`}
                      aria-label={`Delete session ${s.session_id}`}
                    >
                      <svg className="h-3 w-3" fill="none" viewBox="0 0 24 24" strokeWidth={2} stroke="currentColor">
                        <path strokeLinecap="round" strokeLinejoin="round" d="m14.74 9-.346 9m-4.788 0L9.26 9m9.968-3.21c.342.052.682.107 1.022.166m-1.022-.165L18.16 19.673a2.25 2.25 0 0 1-2.244 2.077H8.084a2.25 2.25 0 0 1-2.244-2.077L4.772 5.79m14.456 0a48.108 48.108 0 0 0-3.478-.397m-12 .562c.34-.059.68-.114 1.022-.165m0 0a48.11 48.11 0 0 1 3.478-.397m7.5 0v-.916c0-1.18-.91-2.164-2.09-2.201a51.964 51.964 0 0 0-3.32 0c-1.18.037-2.09 1.022-2.09 2.201v.916m7.5 0a48.667 48.667 0 0 0-7.5 0" />
                      </svg>
                    </button>
                  )}
                </div>
              );
            })}
          </div>
        )}
      </ScrollArea>
    </aside>
  );
}
