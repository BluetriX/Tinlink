// Run with: node --test apps/web/tests/*.test.js
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  Gate, MutedSpeechDetector, can, inputName, isUnread, mentionCandidates, mentionQuery, shareBitrate, shareSize,
} from "../logic.js";

test("permissions", () => {
  assert.ok(can("VIEW_CHANNEL | SEND_MESSAGES", "SEND_MESSAGES"));
  assert.ok(!can("VIEW_CHANNEL", "SEND_MESSAGES"));
  assert.ok(can("ADMINISTRATOR", "KICK_MEMBERS"));
  assert.ok(!can(undefined, "VIEW_CHANNEL"));
});

test("unread channels", () => {
  assert.ok(!isUnread({ last_message_id: null, last_read_id: null }));
  assert.ok(isUnread({ last_message_id: "0192a", last_read_id: null }));
  assert.ok(isUnread({ last_message_id: "0192b", last_read_id: "0192a" }));
  assert.ok(!isUnread({ last_message_id: "0192a", last_read_id: "0192a" }));
});

test("mention query before the caret", () => {
  assert.deepEqual(mentionQuery("hi @al", 6), { start: 3, prefix: "al" });
  assert.deepEqual(mentionQuery("@", 1), { start: 0, prefix: "" });
  assert.equal(mentionQuery("mail a@b", 8), null);
  assert.equal(mentionQuery("hi @al there", 12), null);
});

test("mention candidates", () => {
  const members = [
    { name: "bob", display_name: "Bob" },
    { name: "alice", display_name: "Alice" },
    { name: "albert", display_name: "Zed" },
    { name: "x", display_name: "Alfred" },
  ];
  assert.deepEqual(mentionCandidates(members, "Al").map((m) => m.name), ["albert", "alice", "x"]);
});

test("voice activation gate keeps word endings", () => {
  const gate = new Gate(-50, 500);
  assert.equal(gate.update(-70, 0), false);
  assert.equal(gate.update(-40, 100), true);
  assert.equal(gate.update(-70, 500), true);
  assert.equal(gate.update(-70, 601), false);
});

test("muted speech fires after 5 s and then cools down", () => {
  const d = new MutedSpeechDetector();
  let fired = 0;
  for (let t = 0; t <= 7_000; t += 20) if (d.update(true, false, t)) fired++;
  assert.equal(fired, 1);
  // Still talking: the cooldown suppresses repeats for 30 s.
  for (let t = 7_020; t < 34_000; t += 20) if (d.update(true, false, t)) fired++;
  assert.equal(fired, 1);
  for (let t = 34_000; t <= 42_000; t += 20) if (d.update(true, false, t)) fired++;
  assert.equal(fired, 2);
});

test("muted speech tolerates short pauses and resets on long ones", () => {
  const d = new MutedSpeechDetector();
  let fired = false;
  // 1.5 s talk, 1.5 s pause, repeated: counts as talking.
  for (let t = 0; t <= 12_000; t += 20) fired ||= d.update(t % 3000 < 1500, false, t);
  assert.ok(fired);
  const e = new MutedSpeechDetector();
  fired = false;
  // 4 s talk, 3 s pause: never 5 s in a row.
  for (let t = 0; t <= 30_000; t += 20) fired ||= e.update(t % 7000 < 4000, false, t);
  assert.ok(!fired);
  // Transmitting never fires.
  const f = new MutedSpeechDetector();
  fired = false;
  for (let t = 0; t <= 20_000; t += 20) fired ||= f.update(true, true, t);
  assert.ok(!fired);
});

test("share sizes and bitrates", () => {
  assert.deepEqual(shareSize(1080), [1920, 1080]);
  assert.equal(shareSize(0), null);
  assert.equal(shareBitrate(1080, 30), 4_000_000);
  assert.equal(shareBitrate(1440, 60), 9_000_000);
  assert.equal(shareBitrate(720, 15), 1_500_000);
});

test("push-to-talk input names", () => {
  assert.equal(inputName({ type: "keydown", code: "F8" }), "F8");
  assert.equal(inputName({ type: "mousedown", button: 3 }), "Mouse4");
  assert.equal(inputName({ type: "pointerdown", button: 0 }), null);
});
