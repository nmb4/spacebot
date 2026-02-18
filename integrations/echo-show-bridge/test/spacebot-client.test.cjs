const test = require("node:test");
const assert = require("node:assert/strict");

const { SpacebotWebhookClient } = require("../src/spacebot-client.cjs");

function jsonResponse(status, body) {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

test("collectReply aggregates stream chunks until stream_end", async () => {
  const calls = [];
  const fetchImpl = async (url, options = {}) => {
    calls.push({ url, method: options.method || "GET" });

    if (url.endsWith("/send")) {
      return new Response("", { status: 202 });
    }

    const pollCallCount = calls.filter((call) =>
      call.url.includes("/poll/"),
    ).length;
    if (pollCallCount === 1) {
      return jsonResponse(200, {
        messages: [{ type: "stream_start" }, { type: "stream_chunk", content: "Hello " }],
      });
    }
    return jsonResponse(200, {
      messages: [{ type: "stream_chunk", content: "world" }, { type: "stream_end" }],
    });
  };

  const client = new SpacebotWebhookClient({
    baseUrl: "http://localhost:18789",
    pollIntervalMs: 1,
    maxWaitMs: 100,
    fetchImpl,
  });

  await client.sendMessage({
    conversationId: "echo_show:test",
    senderId: "alexa:test",
    content: "hello",
  });
  const result = await client.collectReply("echo_show:test");

  assert.equal(result.text, "Hello world");
  assert.equal(result.rawMessages.length, 4);
});

test("collectReply accepts non-stream text response", async () => {
  const fetchImpl = async (url) => {
    if (url.endsWith("/send")) {
      return new Response("", { status: 202 });
    }
    return jsonResponse(200, {
      messages: [{ type: "text", content: "Done." }],
    });
  };

  const client = new SpacebotWebhookClient({
    baseUrl: "http://localhost:18789",
    pollIntervalMs: 1,
    maxWaitMs: 50,
    fetchImpl,
  });

  const result = await client.collectReply("echo_show:test");
  assert.equal(result.text, "Done.");
});

test("collectReply returns timeout metadata when response never arrives", async () => {
  const fetchImpl = async (url) => {
    if (url.endsWith("/send")) {
      return new Response("", { status: 202 });
    }
    return jsonResponse(200, { messages: [] });
  };

  const client = new SpacebotWebhookClient({
    baseUrl: "http://localhost:18789",
    pollIntervalMs: 1,
    maxWaitMs: 10,
    fetchImpl,
  });

  const result = await client.collectReply("echo_show:test");
  assert.equal(result.text, "");
  assert.equal(result.receivedMessages, false);
  assert.equal(result.timedOut, true);
});
