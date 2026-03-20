import { useState } from "react";
import { invoke } from "@tauri-apps/api/core";

export interface ClarificationPayload {
  source: string;
  objective: string;
  questions: string[];
}

interface ClarificationModalProps {
  payload: ClarificationPayload;
  onSubmitted?: (answer: string | null) => void;
  onClose: () => void;
}

export default function ClarificationModal({
  payload,
  onSubmitted,
  onClose,
}: ClarificationModalProps) {
  const [answer, setAnswer] = useState("");
  const [submitting, setSubmitting] = useState(false);

  const submit = async (response: string | null) => {
    setSubmitting(true);
    try {
      await invoke("submit_clarification_response", {
        response,
      });
      onSubmitted?.(response);
    } finally {
      setSubmitting(false);
      onClose();
    }
  };

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/60 backdrop-blur-sm">
      <div className="w-full max-w-2xl rounded-2xl border border-brand-700/40 bg-gray-900 p-6 shadow-2xl">
        <div className="mb-4 flex items-start gap-3">
          <span className="mt-0.5 text-2xl">❓</span>
          <div>
            <h2 className="text-base font-semibold text-brand-200">
              Clarification Needed
            </h2>
            <p className="text-xs text-gray-400">
              {payload.source} paused the pipeline because the current request is still ambiguous.
            </p>
          </div>
        </div>

        <div className="mb-4 rounded-lg border border-gray-700 bg-gray-800/60 p-3">
          <p className="mb-1 text-xs font-medium uppercase tracking-wide text-gray-500">
            Current objective
          </p>
          <p className="text-sm leading-relaxed text-gray-200">
            {payload.objective}
          </p>
        </div>

        <div className="mb-4 rounded-lg border border-yellow-800/40 bg-yellow-950/20 p-3">
          <p className="mb-2 text-xs font-medium uppercase tracking-wide text-yellow-400">
            Questions
          </p>
          <ul className="space-y-1 text-sm text-yellow-100">
            {payload.questions.map((question) => (
              <li key={question}>• {question}</li>
            ))}
          </ul>
        </div>

        <label className="mb-2 block text-xs font-medium uppercase tracking-wide text-gray-500">
          Your answer
        </label>
        <textarea
          value={answer}
          onChange={(event) => setAnswer(event.target.value)}
          placeholder="Provide the missing details so the pipeline can continue."
          className="min-h-32 w-full rounded-xl border border-gray-700 bg-gray-950 px-4 py-3 text-sm text-gray-100 outline-none transition focus:border-brand-500"
        />

        <div className="mt-4 flex gap-3">
          <button
            onClick={() => submit(null)}
            disabled={submitting}
            className="rounded-lg border border-gray-600 px-4 py-2.5 text-sm font-medium text-gray-300 transition hover:bg-gray-800 disabled:opacity-50"
          >
            Abort run
          </button>
          <button
            onClick={() => submit(answer)}
            disabled={submitting || !answer.trim()}
            className="rounded-lg bg-brand-700 px-4 py-2.5 text-sm font-medium text-white transition hover:bg-brand-600 disabled:cursor-not-allowed disabled:opacity-50"
          >
            Continue pipeline
          </button>
        </div>
      </div>
    </div>
  );
}