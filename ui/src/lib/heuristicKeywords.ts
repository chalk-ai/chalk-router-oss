import { DEFAULT_HARD_KEYWORDS, type HeuristicBackend } from "./routeGrid";

const same = (a: string, b: string) => a.toLowerCase() === b.toLowerCase();

/** Mirror the runtime's defaults-minus-removals-plus-additions without trimming. */
export function activeHeuristicKeywords(config: HeuristicBackend): string[] {
  const defaults = (config.use_default_keywords ?? true)
    ? DEFAULT_HARD_KEYWORDS.filter(word => !config.removed_hard_keywords.some(removed => same(word, removed)))
    : [];
  return [...defaults, ...config.added_hard_keywords].filter((word, index, words) => !words.slice(0, index).some(previous => same(previous, word)));
}

export function addHeuristicKeyword(config: HeuristicBackend, word: string): HeuristicBackend {
  const isDefault = (config.use_default_keywords ?? true) && DEFAULT_HARD_KEYWORDS.some(value => same(value, word));
  return {
    ...config,
    removed_hard_keywords: config.removed_hard_keywords.filter(value => !same(value, word)),
    added_hard_keywords: isDefault || config.added_hard_keywords.some(value => same(value, word))
      ? config.added_hard_keywords : [...config.added_hard_keywords, word],
  };
}

export function removeHeuristicKeyword(config: HeuristicBackend, word: string): HeuristicBackend {
  const isDefault = (config.use_default_keywords ?? true) && DEFAULT_HARD_KEYWORDS.some(value => same(value, word));
  return {
    ...config,
    added_hard_keywords: config.added_hard_keywords.filter(value => !same(value, word)),
    removed_hard_keywords: isDefault && !config.removed_hard_keywords.some(value => same(value, word))
      ? [...config.removed_hard_keywords, word] : config.removed_hard_keywords,
  };
}
