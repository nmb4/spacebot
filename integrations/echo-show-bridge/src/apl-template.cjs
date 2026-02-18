const APL_DOCUMENT = {
  type: "APL",
  version: "1.8",
  settings: {},
  theme: "dark",
  import: [],
  mainTemplate: {
    parameters: ["payload"],
    items: [
      {
        type: "Container",
        width: "100vw",
        height: "100vh",
        direction: "column",
        paddingLeft: "5vw",
        paddingRight: "5vw",
        paddingTop: "4vh",
        paddingBottom: "4vh",
        items: [
          {
            type: "Text",
            text: "${payload.title}",
            maxLines: 2,
            fontSize: "32dp",
            fontWeight: "700",
            color: "#FFFFFF",
            spacing: 8,
            display: "${payload.title ? 'normal' : 'none'}",
          },
          {
            type: "Text",
            text: "${payload.body}",
            maxLines: 6,
            fontSize: "24dp",
            color: "#E8E8E8",
            spacing: 12,
            display: "${payload.body ? 'normal' : 'none'}",
          },
          {
            type: "Sequence",
            data: "${payload.items}",
            spacing: 6,
            grow: 1,
            numbered: false,
            display: "${payload.items.length > 0 ? 'normal' : 'none'}",
            item: {
              type: "Text",
              text: "• ${data}",
              fontSize: "20dp",
              color: "#F5F5F5",
              maxLines: 2,
            },
          },
          {
            type: "Image",
            source: "${payload.imageUrl}",
            width: "100%",
            height: "36vh",
            scale: "best-fill",
            borderRadius: "8dp",
            alignSelf: "center",
            display: "${payload.imageUrl ? 'normal' : 'none'}",
          },
        ],
      },
    ],
  },
};

function buildAplDirective(directive) {
  return {
    type: "Alexa.Presentation.APL.RenderDocument",
    token: "spacebot-echo-show",
    document: APL_DOCUMENT,
    datasources: {
      payload: {
        title: directive.title || "",
        body: directive.body || "",
        items: directive.items || [],
        imageUrl: directive.imageUrl || "",
      },
    },
  };
}

function supportsApl(requestEnvelope) {
  return Boolean(
    requestEnvelope?.context?.System?.device?.supportedInterfaces?.[
      "Alexa.Presentation.APL"
    ],
  );
}

module.exports = {
  buildAplDirective,
  supportsApl,
};
