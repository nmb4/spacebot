function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function resolveFetch(fetchImpl) {
  if (typeof fetchImpl === "function") {
    return fetchImpl;
  }

  if (typeof globalThis.fetch === "function") {
    return globalThis.fetch.bind(globalThis);
  }

  try {
    const nodeFetch = require("node-fetch");
    return (...args) => nodeFetch(...args);
  } catch (error) {
    throw new Error("No fetch implementation available");
  }
}

class SpacebotWebhookClient {
  constructor({
    baseUrl,
    pollIntervalMs = 350,
    maxWaitMs = 8000,
    fetchImpl,
  }) {
    if (!baseUrl) {
      throw new Error("Spacebot webhook baseUrl is required");
    }
    this.baseUrl = baseUrl.replace(/\/+$/, "");
    this.pollIntervalMs = pollIntervalMs;
    this.maxWaitMs = maxWaitMs;
    this.fetch = resolveFetch(fetchImpl);
  }

  async sendMessage({ conversationId, senderId, content, agentId }) {
    const response = await this.fetch(`${this.baseUrl}/send`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        conversation_id: conversationId,
        sender_id: senderId,
        content,
        agent_id: agentId,
      }),
    });
    if (!response.ok) {
      throw new Error(`Spacebot /send failed: HTTP ${response.status}`);
    }
  }

  async pollMessages(conversationId) {
    const response = await this.fetch(
      `${this.baseUrl}/poll/${encodeURIComponent(conversationId)}`,
      {
        method: "GET",
        headers: { accept: "application/json" },
      },
    );
    if (!response.ok) {
      throw new Error(`Spacebot /poll failed: HTTP ${response.status}`);
    }
    const body = await response.json();
    const messages = Array.isArray(body?.messages) ? body.messages : [];
    return messages;
  }

  async collectReply(conversationId) {
    const deadline = Date.now() + this.maxWaitMs;
    const textParts = [];
    const rawMessages = [];
    let sawAnyResponse = false;
    let sawStreamStart = false;
    let completed = false;

    while (Date.now() < deadline) {
      const batch = await this.pollMessages(conversationId);
      if (batch.length > 0) {
        sawAnyResponse = true;
      }

      let sawStreamEnd = false;
      let sawPlainText = false;

      for (const message of batch) {
        rawMessages.push(message);
        switch (message.type) {
          case "stream_start":
            sawStreamStart = true;
            break;
          case "stream_chunk":
            if (typeof message.content === "string") {
              textParts.push(message.content);
            }
            break;
          case "stream_end":
            sawStreamEnd = true;
            break;
          case "text":
            sawPlainText = true;
            if (typeof message.content === "string") {
              textParts.push(message.content);
            }
            break;
          default:
            break;
        }
      }

      if (sawStreamEnd) {
        completed = true;
        break;
      }

      if (!sawStreamStart && sawPlainText) {
        completed = true;
        break;
      }

      await sleep(this.pollIntervalMs);
    }

    const text = textParts.join("").trim();
    return {
      text,
      rawMessages,
      timedOut: !completed,
      receivedMessages: sawAnyResponse,
    };
  }
}

module.exports = { SpacebotWebhookClient };
