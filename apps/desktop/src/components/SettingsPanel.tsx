/**
 * SettingsPanel — slide-out settings panel for global configuration.
 *
 * Currently exposes:
 * - `max_tool_turns` — maximum tool-call rounds before the agent loop terminates
 *
 * Reads current values via `get_config` on open, saves via `save_settings`.
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

// ── Component ─────────────────────────────────────────────────────────────────

interface Props {
  open: boolean;
  onClose: () => void;
}

export default function SettingsPanel({ open, onClose }: Props) {
  const [maxToolTurns, setMaxToolTurns] = useState<string>("100");
  const [saving, setSaving] = useState(false);
  const [saved, setSaved] = useState(false);

  // Load current config when opened
  useEffect(() => {
    if (!open) return;
    invoke<ConfigDto>("get_config")
      .then((cfg) => {
        setMaxToolTurns(String(cfg.max_tool_turns ?? 100));
        setSaved(false);
      })
      .catch(() => {});
  }, [open]);

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
