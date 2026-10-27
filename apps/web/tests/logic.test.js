// Run with: node --test apps/web/tests/*.test.js
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  CHANNEL_PERMISSIONS, Gate, cameraGrid, cameraSize, MutedSpeechDetector, TypingTracker, can, canEditRole, formatPerms, memberPosition,
  overwriteState, parsePerms, roleColor, setOverwriteState, highlightRanges, searchTerms, inputName, isUnread, mentionCandidates, mentionQuery, shareBitrate,
  shareSize, sortMembers, typingText,
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

test("member list puts online members first", () => {
  const members = [
    { name: "zed", online: true },
    { name: "bob", display_name: "Bob", online: false },
    { name: "x", display_name: "Alice", online: true },
    { name: "carl" },
  ];
  assert.deepEqual(sortMembers(members).map((m) => m.name), ["x", "zed", "bob", "carl"]);
});

test("typing notices expire and end with a message", () => {
  const t = new TypingTracker(5000);
  t.add("c1", "u1", "bob", 0);
  t.add("c1", "u2", "alice", 1000);
  t.add("c2", "u3", "carl", 0);
  assert.deepEqual(t.names("c1", 2000), ["alice", "bob"]);
  assert.deepEqual(t.names("c1", 5500), ["alice"]);
  t.add("c1", "u2", "alice", 5500);
  assert.deepEqual(t.names("c1", 9000), ["alice"]);
  t.remove("c1", "u2");
  assert.deepEqual(t.names("c1", 9000), []);
  assert.deepEqual(t.names("c2", 9000), []);
});

test("typing text", () => {
  assert.equal(typingText([]), "");
  assert.equal(typingText(["bob"]), "bob is typing…");
  assert.equal(typingText(["a", "b", "c"]), "a, b and c are typing…");
  assert.equal(typingText(["a", "b", "c", "d"]), "Several people are typing…");
});

test("search terms split like the server", () => {
  assert.deepEqual(searchTerms("Pizza, fr!"), ["pizza", "fr"]);
  assert.deepEqual(searchTerms("Grüße 2026"), ["grüße", "2026"]);
  assert.deepEqual(searchTerms(" ?! "), []);
});

test("highlight ranges at word starts", () => {
  assert.deepEqual(highlightRanges("Pizza am Freitag? pizza!", ["piz", "fr"]), [[0, 3], [9, 11], [18, 21]]);
  // Not inside words, overlaps merge.
  assert.deepEqual(highlightRanges("Salamipizza", ["piz"]), []);
  assert.deepEqual(highlightRanges("freitag", ["fr", "frei"]), [[0, 4]]);
});

test("permission strings", () => {
  const set = parsePerms("SEND_MESSAGES | VIEW_CHANNEL|FUTURE_FLAG");
  assert.ok(set.has("VIEW_CHANNEL") && set.has("FUTURE_FLAG"));
  assert.equal(formatPerms(set), "VIEW_CHANNEL | SEND_MESSAGES | FUTURE_FLAG");
  assert.equal(formatPerms(parsePerms("")), "");
  assert.equal(CHANNEL_PERMISSIONS.length, 10);
});

test("overwrite tri-state", () => {
  let ow = { target: { kind: "role", id: "r" }, allow: "", deny: "VIEW_CHANNEL" };
  assert.equal(overwriteState(ow, "VIEW_CHANNEL"), "deny");
  assert.equal(overwriteState(ow, "SPEAK"), "inherit");
  ow = setOverwriteState(ow, "VIEW_CHANNEL", "allow");
  ow = setOverwriteState(ow, "SPEAK", "deny");
  assert.deepEqual([ow.allow, ow.deny], ["VIEW_CHANNEL", "SPEAK"]);
  ow = setOverwriteState(ow, "SPEAK", "inherit");
  assert.equal(ow.deny, "");
  assert.equal(ow.target.id, "r");
});

test("role hierarchy helpers", () => {
  const roles = [{ id: "a", position: 3 }, { id: "b", position: 1 }];
  assert.equal(memberPosition({ role_ids: ["a", "b"] }, roles), 3);
  assert.equal(memberPosition({ role_ids: [] }, roles), 0);
  assert.ok(canEditRole({ position: 1 }, 3));
  assert.ok(!canEditRole({ position: 3 }, 3));
  assert.ok(canEditRole({ everyone: true, position: 0 }, 0));
  assert.equal(roleColor(0x3366ff), "#3366ff");
  assert.equal(roleColor(null), null);
});

test("webcam grid stays close to square", () => {
  assert.deepEqual(cameraGrid(0), { cols: 1, rows: 1 });
  assert.deepEqual(cameraGrid(1), { cols: 1, rows: 1 });
  assert.deepEqual(cameraGrid(2), { cols: 2, rows: 1 });
  assert.deepEqual(cameraGrid(3), { cols: 2, rows: 2 });
  assert.deepEqual(cameraGrid(5), { cols: 3, rows: 2 });
  assert.deepEqual(cameraGrid(10), { cols: 4, rows: 3 });
  assert.deepEqual(cameraSize(360), [640, 360]);
  assert.deepEqual(cameraSize(undefined), [1280, 720]);
});
