const MAX_TITLE_LEN = 80;
const MAX_BODY_LEN = 700;
const MAX_ITEM_LEN = 100;
const MAX_ITEMS = 7;

function truncate(text, maxLen) {
  if (typeof text !== "string") {
    return "";
  }
  const trimmed = text.trim();
  if (trimmed.length <= maxLen) {
    return trimmed;
  }
  return `${trimmed.slice(0, maxLen - 1)}…`;
}

function normalizeImageUrl(value) {
  if (typeof value !== "string" || value.trim() === "") {
    return undefined;
  }
  try {
    const parsed = new URL(value.trim());
    if (parsed.protocol !== "https:") {
      return undefined;
    }
    return parsed.toString();
  } catch {
    return undefined;
  }
}

function normalizeDirective(payload) {
  const root =
    payload && typeof payload === "object" ? payload.echo_show : undefined;
  if (!root || typeof root !== "object") {
    return null;
  }

  if (root.template !== "content_list_v1") {
    return null;
  }

  const title = truncate(root.title, MAX_TITLE_LEN);
  const body = truncate(root.body, MAX_BODY_LEN);
  const items = Array.isArray(root.items)
    ? root.items
        .filter((value) => typeof value === "string")
        .map((value) => truncate(value, MAX_ITEM_LEN))
        .filter(Boolean)
        .slice(0, MAX_ITEMS)
    : [];
  const imageUrl = normalizeImageUrl(root.image_url);

  if (!title && !body && items.length === 0 && !imageUrl) {
    return null;
  }

  return {
    template: "content_list_v1",
    title,
    body,
    items,
    imageUrl,
  };
}

function parseCandidate(candidate) {
  try {
    const parsed = JSON.parse(candidate);
    return normalizeDirective(parsed);
  } catch {
    return null;
  }
}

function deriveSpeechFromDirective(directive) {
  if (directive.body) {
    return directive.body;
  }
  if (directive.title) {
    return directive.title;
  }
  if (directive.items.length > 0) {
    return directive.items.join(". ");
  }
  return "I've updated the display.";
}

function cleanSpeech(text) {
  if (typeof text !== "string") {
    return "";
  }
  return text.replace(/\s+/g, " ").trim();
}

function extractVisualDirective(responseText) {
  const raw = typeof responseText === "string" ? responseText : "";
  let directive = null;
  let speechText = cleanSpeech(raw);

  const blockRegex = /```(?:json)?\s*([\s\S]*?)```/gi;
  let match = blockRegex.exec(raw);
  let removeStart = -1;
  let removeEnd = -1;

  while (match) {
    const parsedDirective = parseCandidate(match[1]);
    if (parsedDirective) {
      directive = parsedDirective;
      removeStart = match.index;
      removeEnd = blockRegex.lastIndex;
      break;
    }
    match = blockRegex.exec(raw);
  }

  if (!directive) {
    const topLevel = parseCandidate(raw.trim());
    if (topLevel) {
      directive = topLevel;
      speechText = "";
    }
  } else {
    const before = raw.slice(0, removeStart);
    const after = raw.slice(removeEnd);
    speechText = cleanSpeech(`${before} ${after}`);
  }

  if (!speechText && directive) {
    speechText = deriveSpeechFromDirective(directive);
  }

  return { speechText, directive };
}

module.exports = {
  extractVisualDirective,
  normalizeDirective,
  truncate,
};
