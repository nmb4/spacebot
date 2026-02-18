#!/usr/bin/env node
const fs = require("node:fs/promises");
const path = require("node:path");

async function main() {
  const eventPathArg = process.argv[2];
  if (!eventPathArg) {
    throw new Error("Usage: node scripts/invoke-local.cjs <event-json-path>");
  }

  const eventPath = path.resolve(process.cwd(), eventPathArg);
  const raw = await fs.readFile(eventPath, "utf8");
  const event = JSON.parse(raw);

  const { handler } = require("../src/index.cjs");
  const response = await handler(event);
  console.log(JSON.stringify(response, null, 2));
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
