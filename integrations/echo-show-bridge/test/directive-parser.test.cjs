const test = require("node:test");
const assert = require("node:assert/strict");

const { extractVisualDirective } = require("../src/directive-parser.cjs");

test("extracts directive from fenced json and keeps spoken text", () => {
  const input = [
    "Here is your update.",
    "```json",
    '{"echo_show":{"template":"content_list_v1","title":"Now","body":"Do these","items":["A","B"],"image_url":"https://example.com/image.png"}}',
    "```",
  ].join("\n");

  const result = extractVisualDirective(input);

  assert.equal(result.speechText, "Here is your update.");
  assert.ok(result.directive);
  assert.equal(result.directive.template, "content_list_v1");
  assert.equal(result.directive.items.length, 2);
});

test("ignores malformed directive and returns plain speech", () => {
  const input = "This is plain speech. ```json { bad json } ```";
  const result = extractVisualDirective(input);
  assert.equal(result.speechText, "This is plain speech. ```json { bad json } ```");
  assert.equal(result.directive, null);
});

test("sanitizes non-https image and truncates long fields", () => {
  const veryLong = "x".repeat(300);
  const input = [
    "```json",
    JSON.stringify({
      echo_show: {
        template: "content_list_v1",
        title: veryLong,
        body: veryLong,
        items: [veryLong],
        image_url: "http://insecure.example.com/a.png",
      },
    }),
    "```",
  ].join("\n");

  const result = extractVisualDirective(input);
  assert.ok(result.directive);
  assert.equal(result.directive.imageUrl, undefined);
  assert.ok(result.directive.title.length <= 80);
  assert.ok(result.directive.body.length <= 700);
  assert.ok(result.directive.items[0].length <= 100);
});
