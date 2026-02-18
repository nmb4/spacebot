const test = require("node:test");
const assert = require("node:assert/strict");

const { createConfig, MAX_SAFE_WAIT_MS } = require("../src/config.cjs");

test("createConfig caps max wait to safe bound", () => {
  const config = createConfig({
    SPACEBOT_WEBHOOK_BASE: "https://example.test",
    SPACEBOT_MAX_WAIT_MS: "99999",
  });

  assert.equal(config.maxWaitMs, MAX_SAFE_WAIT_MS);
});
