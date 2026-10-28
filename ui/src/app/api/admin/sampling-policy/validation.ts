export function isOutcomeScalar(value: unknown): value is string | number | boolean | null {
  return (
    value === null ||
    typeof value === "string" ||
    typeof value === "boolean" ||
    (typeof value === "number" && Number.isFinite(value))
  );
}

export function samplingRuleValidationError(rule: unknown, index: number): string | null {
  if (typeof rule !== "object" || rule === null || Array.isArray(rule)) {
    return `rules[${index}] must be an object`;
  }
  const { applies_to, value } = rule as Record<string, unknown>;
  if (
    applies_to !== "request_tag" &&
    applies_to !== "key_id" &&
    applies_to !== "usage_pool_id"
  ) {
    return `rules[${index}].applies_to must be request_tag, key_id, or usage_pool_id`;
  }
  if (typeof value !== "string" || value.trim() === "") {
    return `rules[${index}].value must be a non-empty string`;
  }
  return null;
}

