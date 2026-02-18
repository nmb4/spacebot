const crypto = require("node:crypto");

function firstNonEmpty(values, fallback) {
  for (const value of values) {
    if (typeof value === "string" && value.trim() !== "") {
      return value.trim();
    }
  }
  return fallback;
}

function getConversationIdentity(requestEnvelope) {
  const system = requestEnvelope?.context?.System || {};
  const session = requestEnvelope?.session || {};
  const request = requestEnvelope?.request || {};

  const appId = firstNonEmpty(
    [system.application?.applicationId, session.application?.applicationId],
    "unknown_app",
  );
  const userId = firstNonEmpty(
    [system.user?.userId, session.user?.userId],
    "unknown_user",
  );
  const deviceId = firstNonEmpty([system.device?.deviceId], "unknown_device");
  const requestId = firstNonEmpty([request.requestId], "unknown_request");

  return { appId, userId, deviceId, requestId };
}

function createHashedUserId(identity, salt = "") {
  return crypto
    .createHash("sha256")
    .update(`${salt}|${identity.appId}|${identity.userId}|${identity.deviceId}`)
    .digest("hex")
    .slice(0, 24);
}

function createConversationContext(requestEnvelope, options = {}) {
  const prefix = options.prefix || "echo_show";
  const salt = options.salt || "";
  const identity = getConversationIdentity(requestEnvelope);
  const userHash = createHashedUserId(identity, salt);

  return {
    conversationId: `${prefix}:${userHash}`,
    senderId: `alexa:${userHash}`,
    userHash,
    requestId: identity.requestId,
  };
}

module.exports = {
  createConversationContext,
  createHashedUserId,
  getConversationIdentity,
};
