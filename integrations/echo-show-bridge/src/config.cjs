const DEFAULT_POLL_INTERVAL_MS = 350;
const DEFAULT_MAX_WAIT_MS = 5000;
const MAX_SAFE_WAIT_MS = 6500;
const DEFAULT_CONVERSATION_PREFIX = "echo_show";

function toPositiveInt(value, fallback) {
  if (value === undefined || value === null || value === "") {
    return fallback;
  }
  const parsed = Number.parseInt(String(value), 10);
  if (!Number.isFinite(parsed) || parsed <= 0) {
    return fallback;
  }
  return parsed;
}

function createConfig(env = process.env) {
  const baseUrl = env.SPACEBOT_WEBHOOK_BASE;
  if (!baseUrl) {
    throw new Error("SPACEBOT_WEBHOOK_BASE is required");
  }

  const maxWaitMs = toPositiveInt(env.SPACEBOT_MAX_WAIT_MS, DEFAULT_MAX_WAIT_MS);

  return {
    baseUrl: baseUrl.replace(/\/+$/, ""),
    agentId: env.SPACEBOT_AGENT_ID || undefined,
    conversationPrefix:
      env.SPACEBOT_CONVERSATION_PREFIX || DEFAULT_CONVERSATION_PREFIX,
    pollIntervalMs: toPositiveInt(
      env.SPACEBOT_POLL_INTERVAL_MS,
      DEFAULT_POLL_INTERVAL_MS,
    ),
    maxWaitMs: Math.min(maxWaitMs, MAX_SAFE_WAIT_MS),
    userHashSalt: env.SPACEBOT_USER_HASH_SALT || "",
  };
}

module.exports = {
  createConfig,
  DEFAULT_CONVERSATION_PREFIX,
  DEFAULT_POLL_INTERVAL_MS,
  DEFAULT_MAX_WAIT_MS,
  MAX_SAFE_WAIT_MS,
};
