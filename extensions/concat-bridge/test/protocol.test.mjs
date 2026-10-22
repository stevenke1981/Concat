// Tests for lib/protocol.js: the settings, the requests, the client, and the
// state reducers. The client tests run over real HTTP against the mock bridge in
// mock-bridge.mjs, through the same functions background.js calls.
// Run with: npm test (in extensions/concat-bridge), or
//   node --test "extensions/concat-bridge/test/*.test.mjs"
import { test } from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import { randomBytes } from "node:crypto";
import * as P from "../lib/protocol.js";
import { startMockBridge, TOKEN } from "./mock-bridge.mjs";

const PNG = Buffer.from("89504e470d0a1a0a00000000", "hex"); // PNG signature bytes; the content is not decoded here

// --- settings and requests ----------------------------------------------------

test("settings fall back to the default port and trim the token", () => {
  assert.deepEqual(P.normalizeSettings({}), { port: 47615, token: "" });
  assert.deepEqual(P.normalizeSettings({ port: "0", token: "  abc  " }), { port: 47615, token: "abc" });
  assert.deepEqual(P.normalizeSettings({ port: 65536, token: TOKEN }), { port: 47615, token: TOKEN });
  assert.deepEqual(P.normalizeSettings({ port: "50000", token: TOKEN }), { port: 50000, token: TOKEN });
  assert.equal(P.parsePort("12.5"), null);
  assert.equal(P.parsePort("1"), 1);
});

test("only hello goes without the token", () => {
  const settings = { port: 47615, token: TOKEN };
  assert.equal(P.buildRequest(settings, { op: "hello" }).url, "http://127.0.0.1:47615/v1/hello");
  assert.equal(P.buildRequest(settings, { op: "hello" }).init.headers, undefined);
  for (const call of [
    { op: "next", site: "flow", wait: 20 },
    { op: "state", id: "a" },
    { op: "progress", id: "a", message: "x", fraction: null },
    { op: "done", id: "a" },
    { op: "fail", id: "a", error: "x" },
    { op: "file", id: "a", bytes: PNG, mediaType: "image/png", name: "n", index: 1 },
  ]) {
    const { init } = P.buildRequest(settings, call);
    assert.equal(init.headers["X-Concat-Token"], TOKEN, `${call.op} carries the token`);
  }
});

test("the next poll names the site and clamps the wait", () => {
  const settings = { port: 47615, token: TOKEN };
  assert.equal(
    P.buildRequest(settings, { op: "next", site: "chatgpt-web", wait: 20 }).url,
    "http://127.0.0.1:47615/v1/jobs/next?site=chatgpt-web&wait=20",
  );
  assert.equal(
    P.buildRequest(settings, { op: "next", site: "flow", wait: 600 }).url,
    "http://127.0.0.1:47615/v1/jobs/next?site=flow&wait=25",
  );
  assert.equal(P.clampWait(-3), 0);
  assert.equal(P.clampWait("nonsense"), 0);
});

test("progress clamps its fraction and sends null when there is none", () => {
  const settings = { port: 1, token: TOKEN };
  const body = (fraction) =>
    JSON.parse(P.buildRequest(settings, { op: "progress", id: "j", message: "m", fraction }).init.body);
  assert.deepEqual(body(0.4), { message: "m", fraction: 0.4 });
  assert.deepEqual(body(3), { message: "m", fraction: 1 });
  assert.deepEqual(body(undefined), { message: "m", fraction: null });
  assert.deepEqual(body(Number.NaN), { message: "m", fraction: null });
});

test("file headers carry the media type, an ASCII name and the index", () => {
  const settings = { port: 1, token: TOKEN };
  const { url, init } = P.buildRequest(settings, {
    op: "file",
    id: "job9",
    bytes: PNG,
    mediaType: "video/mp4",
    name: "映画 clip.mp4",
    index: 3,
  });
  assert.equal(url, "http://127.0.0.1:1/v1/jobs/job9/file");
  assert.equal(init.headers["Content-Type"], "video/mp4");
  // Each non-ASCII character becomes one underscore: the two Japanese characters make two.
  assert.equal(init.headers["X-File-Name"], "__ clip.mp4");
  assert.equal(init.headers["X-Index"], "3");
  assert.equal(init.body, PNG);

  const bare = P.buildRequest(settings, { op: "file", id: "j", bytes: PNG }).init.headers;
  assert.equal(bare["Content-Type"], "application/octet-stream");
  assert.equal(bare["X-File-Name"], undefined);
  assert.equal(bare["X-Index"], undefined);
});

test("asciiHeader keeps printable ASCII only and bounds the length", () => {
  assert.equal(P.asciiHeader("aéb\nc"), "a_b_c");
  assert.equal(P.asciiHeader("x".repeat(500)).length, 200);
  assert.equal(P.asciiHeader(undefined), "");
});

test("an unknown call is an error, not a silent request", () => {
  assert.throws(() => P.buildRequest({ port: 1, token: TOKEN }, { op: "nope" }), /unknown bridge call/);
});

test("base64 decodes to the same bytes", () => {
  const bytes = randomBytes(1 << 20);
  const decoded = P.base64ToBytes(bytes.toString("base64"));
  assert.equal(decoded.length, bytes.length);
  assert.ok(Buffer.from(decoded).equals(bytes));
  assert.throws(() => P.base64ToBytes("***not base64***"));
});

test("ended states and retry delays", () => {
  for (const state of ["done", "failed", "cancelled", "gone"]) assert.equal(P.isEndedState(state), true);
  for (const state of ["queued", "taken", "unknown"]) assert.equal(P.isEndedState(state), false);
  assert.equal(P.retryDelay("unauthorized"), 10000);
  assert.equal(P.retryDelay("unreachable"), 5000);
  assert.equal(P.retryDelay(undefined), 5000);
});

// --- status reducer -----------------------------------------------------------

test("the status moves through the connection states", () => {
  let s = P.initialStatus();
  assert.equal(s.concat, "offline");
  s = P.reduceStatus(s, { type: "hello-ok", version: "0.9.0" });
  assert.deepEqual([s.concat, s.version], ["ok", "0.9.0"]);
  s = P.reduceStatus(s, { type: "poll-unauthorized" });
  assert.equal(s.concat, "unpaired");
  // Hello does not check the token, so it does not clear "unpaired".
  s = P.reduceStatus(s, { type: "hello-ok", version: "0.9.0" });
  assert.equal(s.concat, "unpaired");
  s = P.reduceStatus(s, { type: "poll-ok" });
  assert.equal(s.concat, "ok");
  s = P.reduceStatus(s, { type: "poll-unreachable" });
  assert.equal(s.concat, "offline");
  s = P.reduceStatus(s, { type: "hello-fail" });
  assert.deepEqual([s.concat, s.version], ["offline", ""]);
});

test("a settings change puts the state back to being checked", () => {
  const unpaired = { concat: "unpaired", version: "0.1.0", lastError: "" };
  const checking = P.reduceStatus(unpaired, { type: "settings-changed" });
  assert.equal(checking.concat, "offline");
  // The hello that follows clears the checking state; an unpaired answer from a poll would set it again.
  assert.equal(P.reduceStatus(checking, { type: "hello-ok", version: "0.1.0" }).concat, "ok");
});

test("the last error is set and cleared without touching the connection state", () => {
  let s = P.reduceStatus(P.initialStatus(), { type: "poll-ok" });
  s = P.reduceStatus(s, { type: "error", message: "Flow: the tab was closed" });
  assert.deepEqual([s.concat, s.lastError], ["ok", "Flow: the tab was closed"]);
  s = P.reduceStatus(s, { type: "clear-error" });
  assert.equal(s.lastError, "");
  assert.deepEqual(P.reduceStatus(s, { type: "unknown-event" }), s);
});

test("a failed call maps to the status event it causes", () => {
  assert.deepEqual(P.eventForError(new P.ConcatError("unreachable", "x")), { type: "poll-unreachable" });
  assert.deepEqual(P.eventForError(new P.ConcatError("aborted", "x")), { type: "poll-unreachable" });
  assert.deepEqual(P.eventForError(new P.ConcatError("unauthorized", "x")), { type: "poll-unauthorized" });
  assert.deepEqual(P.eventForError(new P.ConcatError("http", "Concat answered 500")), {
    type: "error",
    message: "Concat answered 500",
  });
  assert.deepEqual(P.eventForError(new Error("boom")), { type: "error", message: "boom" });
});

test("site counts show tabs and whether any is busy", () => {
  assert.deepEqual(P.summarizeSites([]), {
    flow: { tabs: 0, busy: false },
    "chatgpt-web": { tabs: 0, busy: false },
  });
  const counts = P.summarizeSites([
    { site: "flow", busy: false },
    { site: "flow", busy: true },
    { site: "chatgpt-web", busy: false },
    { site: null, busy: false },
  ]);
  assert.deepEqual(counts.flow, { tabs: 2, busy: true });
  assert.deepEqual(counts["chatgpt-web"], { tabs: 1, busy: false });
});

test("a job goes to the most recently focused free tab", () => {
  const tabs = [
    { id: 1, connectedAt: 100, focusedAt: 500, busy: false },
    { id: 2, connectedAt: 300, focusedAt: 0, busy: false },
    { id: 3, connectedAt: 900, focusedAt: 0, busy: true },
  ];
  assert.equal(P.pickPort(tabs).id, 1, "focus beats a later connection");
  assert.equal(P.pickPort([{ id: 2, connectedAt: 300, focusedAt: 0, busy: false }]).id, 2);
  assert.equal(P.pickPort([{ id: 3, connectedAt: 900, focusedAt: 0, busy: true }]), null, "busy tabs are skipped");
  assert.equal(P.pickPort([]), null);
});

// --- the client, against the mock bridge ---------------------------------------

const FLOW_JOB = {
  id: "job1",
  site: "flow",
  kind: "image",
  prompt: "a red fox",
  model: "",
  aspectRatio: "16:9",
  resolution: "",
  duration: 0,
  count: 1,
  reference: null,
};

test("a fake job runs next, progress, file and done in order", async () => {
  const bridge = await startMockBridge();
  try {
    bridge.addJob(FLOW_JOB);
    const reply = await P.poll(bridge.settings, "flow", 0);
    assert.equal(reply.kind, "job");
    assert.deepEqual(reply.job, FLOW_JOB);

    await P.postProgress(bridge.settings, "job1", "rendering", 0.5);
    const saved = await P.postFile(bridge.settings, "job1", {
      bytes: PNG,
      mediaType: "image/png",
      name: "flow-1",
      index: 1,
    });
    assert.equal(saved, "1-file.png");
    await P.postDone(bridge.settings, "job1");

    assert.deepEqual(
      bridge.calls.map((call) => `${call.method} ${call.path}`),
      [
        "GET /v1/jobs/next?site=flow&wait=0",
        "POST /v1/jobs/job1/progress",
        "POST /v1/jobs/job1/file",
        "POST /v1/jobs/job1/done",
      ],
    );
    assert.ok(bridge.calls.slice(1).every((call) => call.token === TOKEN), "every call carries the token");
    assert.deepEqual(JSON.parse(bridge.calls[1].body), { message: "rendering", fraction: 0.5 });
    assert.equal(bridge.calls[2].contentType, "image/png");
    assert.equal(bridge.calls[2].fileName, "flow-1");
    assert.equal(bridge.calls[2].index, "1");
    assert.ok(bridge.calls[2].body.equals(PNG), "the file arrives byte for byte");
  } finally {
    await bridge.close();
  }
});

test("an empty poll is none, and a failed job reports its reason", async () => {
  const bridge = await startMockBridge({ states: { job2: "taken" } });
  try {
    assert.deepEqual(await P.poll(bridge.settings, "chatgpt-web", 0), { kind: "none" });
    await P.postFail(bridge.settings, "job2", "Flow refused the prompt");
    const fail = bridge.calls.at(-1);
    assert.equal(fail.path, "/v1/jobs/job2/fail");
    assert.deepEqual(JSON.parse(fail.body), { error: "Flow refused the prompt" });
  } finally {
    await bridge.close();
  }
});

test("a wrong token is unauthorized, and the state check reads the job", async () => {
  const bridge = await startMockBridge({ states: { job1: "cancelled" } });
  try {
    await assert.rejects(
      P.poll({ ...bridge.settings, token: "wrong" }, "flow", 0),
      (error) => error instanceof P.ConcatError && error.kind === "unauthorized",
    );
    assert.equal(await P.jobState(bridge.settings, "job1"), "cancelled");
    assert.equal(await P.jobState(bridge.settings, "nobody"), "gone");
  } finally {
    await bridge.close();
  }
});

test("a post to a cancelled job is refused as a conflict with the reason", async () => {
  const bridge = await startMockBridge({ states: { job1: "cancelled" } });
  try {
    await assert.rejects(
      P.postFile(bridge.settings, "job1", { bytes: PNG, mediaType: "image/png", name: "x", index: 1 }),
      (error) => error.kind === "conflict" && error.status === 409 && /cancelled/.test(error.message),
    );
  } finally {
    await bridge.close();
  }
});

test("hello identifies Concat and refuses another program on the port", async () => {
  const bridge = await startMockBridge();
  try {
    const info = await P.hello(bridge.settings, { signal: AbortSignal.timeout(3000) });
    assert.deepEqual(info, { version: "0.1.0-test", protocol: 1 });
  } finally {
    await bridge.close();
  }
  const impostor = http.createServer((req, res) => {
    res.writeHead(200, { "Content-Type": "application/json" });
    res.end(JSON.stringify({ app: "something-else" }));
  });
  await new Promise((resolve) => impostor.listen(0, "127.0.0.1", resolve));
  try {
    await assert.rejects(
      P.hello({ port: impostor.address().port, token: TOKEN }),
      (error) => error.kind === "http" && /not as Concat/.test(error.message),
    );
  } finally {
    await new Promise((done) => impostor.close(done));
  }
});

test("nothing listening is unreachable, and a stopped request is aborted", async () => {
  const closed = http.createServer();
  await new Promise((resolve) => closed.listen(0, "127.0.0.1", resolve));
  const port = closed.address().port;
  await new Promise((done) => closed.close(done));
  await assert.rejects(
    P.poll({ port, token: TOKEN }, "flow", 0),
    (error) => error instanceof P.ConcatError && error.kind === "unreachable",
  );

  const controller = new AbortController();
  controller.abort();
  await assert.rejects(
    P.poll({ port, token: TOKEN }, "flow", 0, { signal: controller.signal }),
    (error) => error.kind === "aborted",
  );
});

test("a job with no id in the answer is refused", async () => {
  const bogus = http.createServer((req, res) => {
    res.writeHead(200, { "Content-Type": "application/json" });
    res.end(JSON.stringify({ site: "flow" }));
  });
  await new Promise((resolve) => bogus.listen(0, "127.0.0.1", resolve));
  try {
    await assert.rejects(
      P.poll({ port: bogus.address().port, token: TOKEN }, "flow", 0),
      (error) => error.kind === "http" && /without an id/.test(error.message),
    );
  } finally {
    await new Promise((done) => bogus.close(done));
  }
});
