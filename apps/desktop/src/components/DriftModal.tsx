import { invoke } from "@tauri-apps/api/core";

export interface DriftDetectedPayload {
  kind: "scope" | "direction" | "both";
  reason: string;
}

interface DriftModalProps {
  payload: DriftDetectedPayload;
  onClose: () => void;
}

const KIND_LABEL: Record<DriftDetectedPayload["kind"], string> = {
  scope:     "Scope Drift",
  direction: "Direction Drift",
  both:      "Scope & Direction Drift",
};

const KIND_DESC: Record<DriftDetectedPayload["kind"], string> = {
  scope:     "The agent appears to be working outside the original goal.",
  direction: "The agent appears to be moving away from the original goal.",
  both:      "The agent has drifted in both scope and direction.",
};

export default function DriftModal({ payload, onClose }: DriftModalProps) {
  const submit = async (decision: "stop" | "restart" | "ignore") => {
    try {
      await invoke("submit_drift_decision", { decision });
      // Only close the modal once the backend has acknowledged the decision.
      onClose();
    } catch (e) {
      console.error("submit_drift_decision failed:", e);
      // On failure, abort cleanly so the backend is never left waiting forever.
      try { await invoke("submit_drift_decision", { decision: "stop" }); } catch {}
      onClose();
    }
  };

  return (
    /* Backdrop */
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/60 backdrop-blur-sm">
      <div className="w-full max-w-md rounded-2xl border border-yellow-600/40 bg-gray-900 p-6 shadow-2xl">
        {/* Header */}
        <div className="mb-4 flex items-start gap-3">
          <span className="mt-0.5 text-2xl">⚠️</span>
          <div>
            <h2 className="text-base font-semibold text-yellow-300">
              {KIND_LABEL[payload.kind]}
            </h2>
            <p className="text-xs text-gray-400">{KIND_DESC[payload.kind]}</p>
          </div>
        </div>

        {/* Reason */}
        <div className="mb-6 rounded-lg border border-gray-700 bg-gray-800/60 p-3">
          <p className="text-xs font-medium text-gray-400 uppercase tracking-wide mb-1">
            Judge's assessment
          </p>
          <p className="text-sm text-gray-200 leading-relaxed">{payload.reason}</p>
        </div>

        {/* Actions */}
        <div className="flex flex-col gap-2">
          <button
            onClick={() => submit("stop")}
            className="w-full rounded-lg bg-red-700 px-4 py-2.5 text-sm font-medium text-white hover:bg-red-600 active:scale-95 transition"
          >
            Stop — Abort the pipeline
          </button>
          <button
            onClick={() => submit("restart")}
            className="w-full rounded-lg bg-brand-700 px-4 py-2.5 text-sm font-medium text-white hover:bg-brand-600 active:scale-95 transition"
          >
            Restart — Reinforce goal &amp; retry
          </button>
          <button
            onClick={() => submit("ignore")}
            className="w-full rounded-lg border border-gray-600 px-4 py-2.5 text-sm font-medium text-gray-300 hover:bg-gray-700 active:scale-95 transition"
          >
            Ignore — Let the agent continue
          </button>
        </div>
      </div>
    </div>
  );
}
