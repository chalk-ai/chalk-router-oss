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
        className="w-full max-w-md rounded-cds-6 border border-cds-border-strong bg-cds-raised p-4 shadow-cds-m"
      >
        <h2 id={titleId} className="section-title !text-cds-ink-strong">
          {title}
        </h2>
        <p id={descriptionId} className="mt-2 text-cds-md text-cds-ink-secondary">
          {description}
        </p>
        <div className="mt-4 flex justify-end gap-2">
          <button
            ref={cancelRef}
            type="button"
            disabled={confirming}
            onClick={onCancel}
            className="btn btn-secondary btn-md"
          >
            Cancel
          </button>
          <button
            type="button"
            disabled={confirming}
            onClick={() => void onConfirm()}
            className="btn btn-destructive btn-md"
          >
            {confirming ? "Working..." : confirmLabel}
          </button>
        </div>
      </div>
    </div>
  );
}
