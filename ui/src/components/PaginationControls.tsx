"use client";

export function PaginationControls({
  hasPrevious,
  hasNext,
  loading,
  onPrevious,
  onNext,
}: {
  hasPrevious: boolean;
  hasNext: boolean;
  loading: boolean;
  onPrevious: () => void;
  onNext: () => void;
}) {
  if (!hasPrevious && !hasNext) return null;

  const buttonClass =
    "text-xs px-3 py-1.5 rounded transition-colors disabled:opacity-40";
  const buttonStyle = {
    background: "#1f2937",
    color: "#d1d5db",
    border: "1px solid #374151",
  };

  return (
    <div className="flex items-center justify-end gap-2 mt-3">
      <button
        type="button"
        onClick={onPrevious}
        disabled={!hasPrevious || loading}
        className={buttonClass}
        style={buttonStyle}
      >
        Previous
      </button>
      <button
        type="button"
        onClick={onNext}
        disabled={!hasNext || loading}
        className={buttonClass}
        style={buttonStyle}
      >
        Next
      </button>
    </div>
  );
}
