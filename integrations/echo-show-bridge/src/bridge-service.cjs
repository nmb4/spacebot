const { createConversationContext } = require("./conversation.cjs");
const { extractVisualDirective } = require("./directive-parser.cjs");

const VISUAL_PROMPT_PREAMBLE = [
  "You are replying through an Amazon Echo Show 5 interface connected to Spacebot.",
  "Output concise, natural spoken text for Alexa.",
  "If a visual update helps, include exactly one JSON object in a fenced ```json code block using this schema:",
  '{"echo_show":{"template":"content_list_v1","title":"...","body":"...","items":["..."],"image_url":"https://..."}}',
  "Rules:",
  "- Use template value content_list_v1 only.",
  "- image_url must be HTTPS.",
  "- Keep text clear and compact for a small display.",
].join("\n");

const TIMEOUT_FALLBACK_SPEECH =
  "Spacebot braucht noch etwas laenger. Bitte versuch es gleich noch einmal.";

function buildSpacebotPrompt(userText) {
  return `${VISUAL_PROMPT_PREAMBLE}\n\nUser message:\n${userText.trim()}`;
}

async function handleChatTurn({
  requestEnvelope,
  userText,
  client,
  conversationPrefix,
  userHashSalt,
  agentId,
}) {
  const conversation = createConversationContext(requestEnvelope, {
    prefix: conversationPrefix,
    salt: userHashSalt,
  });

  const content = buildSpacebotPrompt(userText);

  await client.sendMessage({
    conversationId: conversation.conversationId,
    senderId: conversation.senderId,
    content,
    agentId,
  });

  const reply = await client.collectReply(conversation.conversationId);
  const parsed = extractVisualDirective(reply.text);
  let speechText = parsed.speechText || "I have an update for you.";
  if (reply.timedOut && !parsed.speechText) {
    speechText = TIMEOUT_FALLBACK_SPEECH;
  }

  return {
    conversationId: conversation.conversationId,
    senderId: conversation.senderId,
    speechText,
    directive: parsed.directive,
    rawMessages: reply.rawMessages,
    timedOut: reply.timedOut,
  };
}

module.exports = {
  buildSpacebotPrompt,
  handleChatTurn,
  VISUAL_PROMPT_PREAMBLE,
  TIMEOUT_FALLBACK_SPEECH,
};
