/**
 * MemoryPanel — slide-out long-term memory recall panel.
 *
 * Opens when the user types `/remember [query]` or clicks the 🧠 button.
 * Calls `recall_memories` on the backend, shows ranked cards, and lets the
 * user attach one card as context for the next pipeline request.
 */

import { useState, useEffect, useRef } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Separator } from "@/components/ui/separator";

// ──────────────────────────────────────────────
// Types
// ──────────────────────────────────────────────

export interface MemoryCardDto {
  id: string;
  session_id: string;
  created_at_secs: number;
  title: string;
  problem: string;
  solution: string;
  key_patterns: string[];
  tags: string[];
  affected_files: string[];
}

/** The context hint string injected into the next pipeline request. */
export function buildContextHint(card: MemoryCardDto): string {
  return `[Memory] ${card.title}\nProblem: ${card.problem}\nSolution: ${card.solution}`;
}

interface Props {
  open: boolean;
  /** Pre-filled search query when opened via `/remember <query>`. */
  initialQuery: string;
  /** ID of the card currently injected (if any). */
  injectedCardId: string | null;
  onClose: () => void;
  onInject: (hint: string, cardId: string, cardTitle: string) => void;
  onClearInject: () => void;
}

// ──────────────────────────────────────────────
// MemoryPanel
// ──────────────────────────────────────────────

export default function MemoryPanel({
  open,
  initialQuery,
  injectedCardId,
  onClose,
  onInject,
  onClearInject,
}: Props) {
  const [query, setQuery] = useState(initialQuery);
  const [cards, setCards] = useState<MemoryCardDto[]>([]);
  const [loading, setLoading] = useState(false);
  const [searched, setSearched] = useState(false);
  const inputRef = useRef<HTMLInputElement>(null);

  // When the panel opens (or query changes), run the search automatically.
  useEffect(() => {
    if (!open) return;
    setQuery(initialQuery);
    runSearch(initialQuery);
    // Focus the input after a short delay so the panel transition finishes first.
    setTimeout(() => inputRef.current?.focus(), 80);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, initialQuery]);

  async function runSearch(q: string) {
    setLoading(true);
    setSearched(false);
    try {
      const results = await invoke<MemoryCardDto[]>("recall_memories", {
        projectDir: null,
        query: q.trim() || null,
        topK: q.trim() ? 5 : 10,
      });
      setCards(results);
    } catch {
      setCards([]);
    } finally {
      setLoading(false);
      setSearched(true);
    }
  }

  function handleSubmit(e: React.FormEvent) {
    e.preventDefault();
    runSearch(query);
  }

  if (!open) return null;

  return (
    <>
      {/* Backdrop */}
      <div
        className="fixed inset-0 z-40 bg-black/40 backdrop-blur-sm"
        onClick={onClose}
      />

      {/* Slide-out panel */}
      <div className="fixed right-0 top-0 z-50 flex h-full w-96 flex-col border-l border-gray-800 bg-gray-950 shadow-2xl">
        {/* Header */}
        <div className="flex items-center gap-2 border-b border-gray-800 px-4 py-3">
          <span className="text-base" aria-hidden>🧠</span>
          <h2 className="flex-1 text-sm font-semibold text-white">Memory Recall</h2>
          <button
            onClick={onClose}
            className="rounded-md p-1 text-gray-400 hover:bg-gray-800 hover:text-gray-200 transition-colors"
            aria-label="Close memory panel"
          >
            <svg className="h-4 w-4" fill="none" viewBox="0 0 24 24" strokeWidth={2} stroke="currentColor">
              <path strokeLinecap="round" strokeLinejoin="round" d="M6 18L18 6M6 6l12 12" />
            </svg>
          </button>
        </div>

        {/* Search bar */}
        <div className="border-b border-gray-800 p-3">
          <form onSubmit={handleSubmit} className="flex gap-2">
            <input
              ref={inputRef}
              className="input-text flex-1 text-sm"
              placeholder="搜索历史解决方案…"
              value={query}
              onChange={(e) => setQuery(e.target.value)}
            />
            <button
              type="submit"
              className="btn-primary px-3 text-xs"
              disabled={loading}
            >
              {loading ? "…" : "搜索"}
            </button>
          </form>
          {!searched && !loading && (
            <button
              type="button"
              onClick={() => runSearch("")}
              className="mt-2 text-xs text-gray-500 hover:text-gray-300 transition-colors"
            >
              显示最近记录 →
            </button>
          )}
        </div>

        {/* Card list */}
        <div className="flex-1 overflow-y-auto">
          {loading && (
            <div className="flex flex-col items-center justify-center gap-2 py-12 text-sm text-gray-400">
              <span className="animate-spin text-xl">⠋</span>
              正在检索记忆…
            </div>
          )}

          {!loading && searched && cards.length === 0 && (
            <div className="px-5 py-10 text-center">
              <p className="text-sm text-gray-400">未找到匹配的记忆卡片</p>
              <p className="mt-1 text-xs text-gray-600">
                记忆卡片由夜间调度器从会话中自动提炼生成
              </p>
            </div>
          )}

          {!loading &&
            cards.map((card) => {
              const isInjected = injectedCardId === card.id;

              return (
                <div
                  key={card.id}
                  className={`border-b border-gray-800/60 p-4 transition-colors ${
                    isInjected ? "bg-brand-900/25 border-l-2 border-l-brand-600" : "hover:bg-gray-900/40"
                  }`}
                >
                  {/* Title */}
                  <p className="mb-1.5 text-sm font-semibold leading-snug text-white">
                    {card.title}
                  </p>

                  {/* Tags */}
                  {card.tags.length > 0 && (
                    <div className="mb-2 flex flex-wrap gap-1">
                      {card.tags.map((tag) => (
                        <span
                          key={tag}
                          className="rounded bg-sky-900/50 px-1.5 py-0.5 text-[10px] text-sky-300"
                        >
                          {tag}
                        </span>
                      ))}
                    </div>
                  )}

                  {/* Problem */}
                  <p className="mb-1 line-clamp-2 text-xs text-gray-400">
                    <span className="font-medium text-gray-500">问题: </span>
                    {card.problem}
                  </p>

                  {/* Solution */}
                  <p className="mb-3 line-clamp-2 text-xs text-gray-400">
                    <span className="font-medium text-gray-500">方案: </span>
                    {card.solution}
                  </p>

                  {/* Key patterns */}
                  {card.key_patterns.length > 0 && (
                    <p className="mb-3 text-xs text-gray-600">
                      <span className="font-medium">模式: </span>
                      {card.key_patterns.slice(0, 3).join(" · ")}
                    </p>
                  )}

                  <Separator className="mb-3 bg-gray-800/60" />

                  {/* Action row */}
                  {isInjected ? (
                    <div className="flex items-center justify-between">
                      <span className="flex items-center gap-1.5 text-xs text-brand-400 font-medium">
                        <svg
                          className="h-3.5 w-3.5"
                          fill="none"
                          viewBox="0 0 24 24"
                          strokeWidth={2.5}
                          stroke="currentColor"
                        >
                          <path strokeLinecap="round" strokeLinejoin="round" d="M4.5 12.75l6 6 9-13.5" />
                        </svg>
                        已加入下一次请求
                      </span>
                      <button
                        type="button"
                        onClick={onClearInject}
                        className="text-xs text-gray-500 hover:text-gray-300 transition-colors"
                      >
                        移除
                      </button>
                    </div>
                  ) : (
                    <button
                      type="button"
                      onClick={() => onInject(buildContextHint(card), card.id, card.title)}
                      className="flex items-center gap-1.5 rounded px-2 py-1 text-xs text-gray-400 transition-colors hover:bg-gray-700/60 hover:text-white"
                    >
                      <svg
                        className="h-3.5 w-3.5"
                        fill="none"
                        viewBox="0 0 24 24"
                        strokeWidth={2}
                        stroke="currentColor"
                      >
                        <path strokeLinecap="round" strokeLinejoin="round" d="M12 4.5v15m7.5-7.5h-15" />
                      </svg>
                      加入下一次请求
                    </button>
                  )}
                </div>
              );
            })}
        </div>

        {/* Footer hint */}
        {injectedCardId && (
          <div className="border-t border-gray-800 bg-gray-900/60 px-4 py-2.5 text-xs text-gray-500">
            🧠 记忆上下文将在下次发送请求时自动注入
          </div>
        )}
      </div>
    </>
  );
}
