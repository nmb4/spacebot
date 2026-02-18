const Alexa = require("ask-sdk-core");

const { buildAplDirective, supportsApl } = require("./apl-template.cjs");
const { handleChatTurn } = require("./bridge-service.cjs");
const { createConfig } = require("./config.cjs");
const { SpacebotWebhookClient } = require("./spacebot-client.cjs");

let runtime;

function getRuntime() {
  if (runtime !== undefined) {
    return runtime;
  }

  try {
    const config = createConfig();
    const client = new SpacebotWebhookClient({
      baseUrl: config.baseUrl,
      pollIntervalMs: config.pollIntervalMs,
      maxWaitMs: config.maxWaitMs,
    });
    runtime = { config, client };
  } catch (error) {
    console.error("Echo bridge configuration error", error);
    runtime = null;
  }

  return runtime;
}

function buildNotConfiguredResponse(handlerInput) {
  return handlerInput.responseBuilder
    .speak(
      "The Spacebot webhook is not configured yet. Please set SPACEBOT WEBHOOK BASE in your skill code environment settings.",
    )
    .reprompt("After setup, ask me to send a message to Spacebot.")
    .getResponse();
}

function getQueryFromIntent(request) {
  return request?.intent?.slots?.query?.value?.trim() || "";
}

const LaunchRequestHandler = {
  canHandle(handlerInput) {
    return (
      Alexa.getRequestType(handlerInput.requestEnvelope) === "LaunchRequest"
    );
  },
  async handle(handlerInput) {
    if (!getRuntime()) {
      return buildNotConfiguredResponse(handlerInput);
    }

    const speechText =
      "Spacebot Echo is ready. What should I ask your Spacebot agent?";

    return handlerInput.responseBuilder
      .speak(speechText)
      .reprompt("What should I send to your Spacebot channel?")
      .getResponse();
  },
};

const ChatIntentHandler = {
  canHandle(handlerInput) {
    return (
      Alexa.getRequestType(handlerInput.requestEnvelope) === "IntentRequest" &&
      Alexa.getIntentName(handlerInput.requestEnvelope) === "ChatIntent"
    );
  },
  async handle(handlerInput) {
    const runtimeState = getRuntime();
    if (!runtimeState) {
      return buildNotConfiguredResponse(handlerInput);
    }

    const query = getQueryFromIntent(handlerInput.requestEnvelope.request);
    if (!query) {
      return handlerInput.responseBuilder
        .speak("I didn't catch that. What should I ask Spacebot?")
        .reprompt("Tell me what to ask Spacebot.")
        .getResponse();
    }

    const result = await handleChatTurn({
      requestEnvelope: handlerInput.requestEnvelope,
      userText: query,
      client: runtimeState.client,
      conversationPrefix: runtimeState.config.conversationPrefix,
      userHashSalt: runtimeState.config.userHashSalt,
      agentId: runtimeState.config.agentId,
    });

    const response = handlerInput.responseBuilder
      .speak(result.speechText)
      .reprompt("Anything else for Spacebot?")
      .withSimpleCard("Spacebot Echo", result.speechText);

    if (result.directive && supportsApl(handlerInput.requestEnvelope)) {
      response.addDirective(buildAplDirective(result.directive));
    }

    return response.getResponse();
  },
};

const HelpIntentHandler = {
  canHandle(handlerInput) {
    return (
      Alexa.getRequestType(handlerInput.requestEnvelope) === "IntentRequest" &&
      Alexa.getIntentName(handlerInput.requestEnvelope) === "AMAZON.HelpIntent"
    );
  },
  handle(handlerInput) {
    if (!getRuntime()) {
      return buildNotConfiguredResponse(handlerInput);
    }

    return handlerInput.responseBuilder
      .speak(
        "Say anything and I will send it to your Spacebot channel, then read the reply.",
      )
      .reprompt("What should I ask?")
      .getResponse();
  },
};

const CancelAndStopIntentHandler = {
  canHandle(handlerInput) {
    if (Alexa.getRequestType(handlerInput.requestEnvelope) !== "IntentRequest") {
      return false;
    }
    const intent = Alexa.getIntentName(handlerInput.requestEnvelope);
    return intent === "AMAZON.CancelIntent" || intent === "AMAZON.StopIntent";
  },
  handle(handlerInput) {
    return handlerInput.responseBuilder.speak("Goodbye.").getResponse();
  },
};

const FallbackIntentHandler = {
  canHandle(handlerInput) {
    return (
      Alexa.getRequestType(handlerInput.requestEnvelope) === "IntentRequest" &&
      Alexa.getIntentName(handlerInput.requestEnvelope) ===
        "AMAZON.FallbackIntent"
    );
  },
  handle(handlerInput) {
    if (!getRuntime()) {
      return buildNotConfiguredResponse(handlerInput);
    }

    return handlerInput.responseBuilder
      .speak("Please say what you want me to send to Spacebot.")
      .reprompt("What should I ask Spacebot?")
      .getResponse();
  },
};

const SessionEndedRequestHandler = {
  canHandle(handlerInput) {
    return (
      Alexa.getRequestType(handlerInput.requestEnvelope) ===
      "SessionEndedRequest"
    );
  },
  handle(handlerInput) {
    return handlerInput.responseBuilder.getResponse();
  },
};

const ErrorHandler = {
  canHandle() {
    return true;
  },
  handle(handlerInput, error) {
    console.error("Echo bridge error", error);
    return handlerInput.responseBuilder
      .speak(
        "I couldn't reach Spacebot right now. Please try again in a moment.",
      )
      .reprompt("Try asking again.")
      .getResponse();
  },
};

exports.handler = Alexa.SkillBuilders.custom()
  .addRequestHandlers(
    LaunchRequestHandler,
    ChatIntentHandler,
    HelpIntentHandler,
    CancelAndStopIntentHandler,
    FallbackIntentHandler,
    SessionEndedRequestHandler,
  )
  .addErrorHandlers(ErrorHandler)
  .withCustomUserAgent("spacebot/echo-show-bridge")
  .lambda();
