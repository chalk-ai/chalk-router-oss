export const MAX_API_KEY_DESCRIPTION_LENGTH = 120;

export function normalizeApiKeyDescription(description: string): string {
  return description.trim();
}

export function apiKeyDescriptionError(description: string): string | null {
  const normalized = normalizeApiKeyDescription(description);
  if (!normalized) return "Description is required.";
  if (Array.from(normalized).length > MAX_API_KEY_DESCRIPTION_LENGTH) {
    return `Description must be ${MAX_API_KEY_DESCRIPTION_LENGTH} characters or fewer.`;
  }
  if (Array.from(normalized).some((character) => /\p{Cc}/u.test(character))) {
    return "Description must not contain control characters.";
  }
  return null;
}
