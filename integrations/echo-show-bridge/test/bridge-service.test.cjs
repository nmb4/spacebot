const test = require("node:test");
const assert = require("node:assert/strict");

const {
  buildSpacebotPrompt,
  handleChatTurn,
  VISUAL_PROMPT_PREAMBLE,
  TIMEOUT_FALLBACK_SPEECH,
} = require("../src/bridge-service.cjs");

function makeEnvelope() {
  return {
    context: {
      System: {
        application: { applicationId: "amzn1.ask.skill.test" },
        user: { userId: "amzn1.ask.account.user" },
        device: {
          deviceId: "amzn1.ask.device.device",
          supportedInterfaces: {
            "Alexa.Presentation.APL": {},
          },
        },
      },
    },
    request: {
      requestId: "EdwRequestId.test",
    },
  };
}

test("buildSpacebotPrompt includes strict visual preamble and user text", () => {
  const prompt = buildSpacebotPrompt("show my tasks");
  assert.ok(prompt.includes(VISUAL_PROMPT_PREAMBLE));
  assert.ok(prompt.includes("show my tasks"));
});

test("handleChatTurn sends prompt and parses directive", async () => {
  const sent = [];
  const mockClient = {
    async sendMessage(payload) {
      sent.push(payload);
    },
    async collectReply() {
      return {
        text: [
          "Here is the status.",
          "```json",
          '{"echo_show":{"template":"content_list_v1","title":"Status","body":"All systems normal","items":["worker healthy"],"image_url":"https://example.com/a.png"}}',
          "```",
        ].join("\n"),
        rawMessages: [{ type: "text", content: "mock" }],
      };
    },
  };

  const result = await handleChatTurn({
    requestEnvelope: makeEnvelope(),
    userText: "status report",
    client: mockClient,
    conversationPrefix: "echo_show",
    userHashSalt: "salt",
    agentId: "main",
  });

  assert.equal(sent.length, 1);
  assert.equal(sent[0].agentId, "main");
  assert.ok(sent[0].conversationId.startsWith("echo_show:"));
  assert.ok(sent[0].content.includes("status report"));
  assert.equal(result.speechText, "Here is the status.");
  assert.ok(result.directive);
  assert.equal(result.directive.template, "content_list_v1");
});

test("handleChatTurn uses timeout fallback speech when no reply is ready", async () => {
  const mockClient = {
    async sendMessage() {},
    async collectReply() {
      return {
        text: "",
        rawMessages: [],
        timedOut: true,
      };
    },
  };

  const result = await handleChatTurn({
    requestEnvelope: makeEnvelope(),
    userText: "status report",
    client: mockClient,
    conversationPrefix: "echo_show",
    userHashSalt: "salt",
    agentId: "main",
  });

  assert.equal(result.speechText, TIMEOUT_FALLBACK_SPEECH);
  assert.equal(result.directive, null);
  assert.equal(result.timedOut, true);
});
