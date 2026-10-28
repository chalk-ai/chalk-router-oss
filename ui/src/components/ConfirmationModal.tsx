"use client";

import { useEffect, useId, useRef } from "react";

interface ConfirmationModalProps {
  open: boolean;
  title: string;
  description: string;
  confirmLabel: string;
  confirming?: boolean;
  onConfirm: () => void | Promise<void>;
  onCancel: () => void;
}

/** A shared confirmation dialog for irreversible admin actions. */
export function ConfirmationModal({
  open,
  title,
  description,
  confirmLabel,
  confirming = false,
  onConfirm,
  onCancel,
}: ConfirmationModalProps) {
  const titleId = useId();
  const descriptionId = useId();
  const cancelRef = useRef<HTMLButtonElement>(null);
  const previousFocus = useRef<HTMLElement | null>(null);

  useEffect(() => {
    if (!open) return;
    previousFocus.current = document.activeElement as HTMLElement | null;
    cancelRef.current?.focus();
    return () => previousFocus.current?.focus();
  }, [open]);

  if (!open) return null;

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center p-4"
      role="presentation"
      onMouseDown={(event) => {
        if (event.target === event.currentTarget && !confirming) onCancel();
      }}
      style={{ background: "rgba(0, 0, 0, 0.65)" }}
    >
      <div
        role="alertdialog"
        aria-modal="true"
        aria-labelledby={titleId}
        aria-describedby={descriptionId}
        onKeyDown={(event) => {
          if (event.key === "Escape" && !confirming) onCancel();
        }}
        className="w-full max-w-md rounded-lg p-6 shadow-2xl"
        style={{ background: "#111827", border: "1px solid #7f1d1d" }}
      >
        <h2 id={titleId} className="text-base font-semibold text-white">
          {title}
        </h2>
        <p id={descriptionId} className="mt-2 text-sm" style={{ color: "#d1d5db" }}>
          {description}
        </p>
        <div className="mt-6 flex justify-end gap-3">
          <button
            ref={cancelRef}
            type="button"
            disabled={confirming}
            onClick={onCancel}
            className="rounded-md px-3 py-2 text-sm disabled:opacity-50"
            style={{ background: "#1f2937", border: "1px solid #374151", color: "#e5e7eb" }}
          >
            Cancel
          </button>
          <button
            type="button"
            disabled={confirming}
            onClick={() => void onConfirm()}
            className="rounded-md px-3 py-2 text-sm font-medium disabled:opacity-50"
            style={{ background: "#b91c1c", color: "white" }}
          >
            {confirming ? "Working..." : confirmLabel}
          </button>
        </div>
      </div>
    </div>
  );
}
