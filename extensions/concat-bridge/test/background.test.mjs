// Tests for background.js, the service worker, run against the mock bridge.
// Only the chrome.* surface the worker uses is stubbed: event registration,
// storage, and the port a content script opens. Everything Concat would see goes
// over real HTTP to the mock, so the routing, queueing, cancel, disconnect and
// status logic are exercised end to end. Run with: npm test in extensions/concat-bridge.
import { test } from "node:test";
import assert from "node:assert/strict";
import { startMockBridge, TOKEN } from "./mock-bridge.mjs";

const PNG = Buffer.from("89504e470d0a1a0a00000000", "hex");
const JOB = {
  id: "x",
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

const storage = { port: 0, token: TOKEN };
const listeners = { connect: [], message: [], storage: [], alarm: [] };

globalThis.chrome = {
  runtime: {
    onConnect: { addListener: (fn) => listeners.connect.push(fn) },
    onMessage: { addListener: (fn) => listeners.message.push(fn) },
  },
  storage: {
    local: {
      get: async (defaults) => ({ ...defaults, port: storage.port, token: storage.token }),
    },
    onChanged: { addListener: (fn) => listeners.storage.push(fn) },
  },
  alarms: {
    create() {},
    onAlarm: { addListener: (fn) => listeners.alarm.push(fn) },
  },
};

// Importing the worker registers its listeners on the stub above.
await import("../background.js");

/** A content script's port, as the worker sees it. */
function fakePage() {
  const page = { posted: [], messageListeners: [], disconnectListeners: [] };
  page.port = {
    name: "concat-site",
    sender: { tab: { id: 1 } },
    onMessage: { addListener: (fn) => page.messageListeners.push(fn) },
    onDisconnect: { addListener: (fn) => page.disconnectListeners.push(fn) },
    postMessage: (message) => page.posted.push(message),
  };
  // What the content script sends to the worker.
  page.send = (message) => {
    for (const fn of page.messageListeners) fn(message);
  };
  // The tab closes, or the page goes away.
  page.disconnect = () => {
    for (const fn of page.disconnectListeners) fn();
  };
  return page;
}

/** Opens a content script's port and says which site it is for. */
function connect(site) {
  const page = fakePage();
  for (const fn of listeners.connect) fn(page.port);
  page.send({ type: "hello", site });
  return page;
}

/** The popup's status request. */
function status() {
  return new Promise((resolve) => {
    for (const fn of listeners.message) fn({ type: "status" }, {}, resolve);
  });
}

/** Waits until `check` is true, failing the test after `ms`. */
async function until(check, ms = 9000) {
  const deadline = Date.now() + ms;
  while (!(await check())) {
    if (Date.now() > deadline) assert.fail("timed out waiting for the condition");
    await new Promise((resolve) => setTimeout(resolve, 25));
  }
}

/** Holds one transfer open until it is aborted or the test releases it. */
function stalledFetch(url) {
  const realFetch = globalThis.fetch;
  let request;
  let release;
  globalThis.fetch = (address, options) => {
    if (address !== url) return realFetch(address, options);
    request = options;
    return new Promise((resolve, reject) => {
      release = () => resolve(new Response(PNG, { headers: { "Content-Type": "image/png" } }));
      request?.signal?.addEventListener("abort", () => reject(request.signal.reason), { once: true });
    });
  };
  return {
    get request() { return request; },
    restore() {
      globalThis.fetch = realFetch;
      release?.();
    },
  };
}

test("a job goes from the bridge to the page, and its file and done come back", async () => {
  const bridge = await startMockBridge();
  storage.port = bridge.port;
  const page = connect("flow");
  try {
    bridge.addJob({ ...JOB, id: "a1" });
    await until(() => page.posted.some((message) => message.type === "job"));
    const job = page.posted.find((message) => message.type === "job").job;
    assert.equal(job.id, "a1");
    assert.equal(job.site, "flow");

    page.send({ type: "progress", id: "a1", message: "rendering", fraction: 0.5 });
    page.send({
      type: "file",
      id: "a1",
      index: 1,
      name: "flow-1",
      mediaType: "image/png",
      base64: PNG.toString("base64"),
    });
    page.send({ type: "done", id: "a1" });
    await until(() => bridge.calls.some((call) => call.path === "/v1/jobs/a1/done"));

    const posts = bridge.calls
      .filter((call) => call.method === "POST" && call.path.startsWith("/v1/jobs/a1/"))
      .map((call) => call.path.split("/").pop());
    assert.deepEqual(posts, ["progress", "file", "done"], "files are posted before done");
    const file = bridge.calls.find((call) => call.path === "/v1/jobs/a1/file");
    assert.ok(file.body.equals(PNG), "the file arrives byte for byte");
    assert.equal(file.contentType, "image/png");
    assert.equal(file.fileName, "flow-1");

    const current = await status();
    assert.equal(current.concat, "ok");
    assert.equal(current.version, "0.1.0-test");
    assert.deepEqual(current.sites.flow, { tabs: 1, busy: false });
    assert.equal(current.lastError, "");
  } finally {
    page.disconnect();
    await bridge.close();
  }
});

test("a cancel in Concat reaches the page, and the page's stop is not an error", async () => {
  const bridge = await startMockBridge();
  storage.port = bridge.port;
  const page = connect("chatgpt-web");
  try {
    bridge.addJob({ ...JOB, id: "b1", site: "chatgpt-web" });
    await until(() => page.posted.some((message) => message.type === "job"));
    bridge.setState("b1", "cancelled");
    await until(() => page.posted.some((message) => message.type === "cancel" && message.id === "b1"));
    page.send({ type: "fail", id: "b1", error: "cancelled" });
    await until(() => bridge.calls.some((call) => call.path === "/v1/jobs/b1/fail"));
    const current = await status();
    assert.equal(current.lastError, "", "a cancel is not shown as an error");
  } finally {
    page.disconnect();
    await bridge.close();
  }
});

test("cancelling a job aborts its stalled download and frees the tab", async () => {
  const bridge = await startMockBridge();
  storage.port = bridge.port;
  const page = connect("flow");
  const downloadUrl = "https://example.test/cancel-stalled-download.png";
  const download = stalledFetch(downloadUrl);
  try {
    bridge.addJob({ ...JOB, id: "cancel-download" });
    await until(() => page.posted.some((message) => message.type === "job"));
    page.send({ type: "fileUrl", id: "cancel-download", index: 1, url: downloadUrl });
    await until(() => download.request);
    bridge.setState("cancel-download", "cancelled");
    await until(() => page.posted.some((message) => message.type === "cancel"));
    assert.equal(download.request.signal?.aborted, true, "cancellation aborts the in-flight file download");

    page.send({ type: "fail", id: "cancel-download", error: "cancelled" });
    await until(() => bridge.calls.some((call) => call.path === "/v1/jobs/cancel-download/fail"));
    assert.equal((await status()).sites.flow.busy, false, "the cancelled download releases the tab");
    assert.equal((await status()).lastError, "", "the cancel is not shown as a download failure");
    assert.equal(bridge.calls.some((call) => call.path === "/v1/jobs/cancel-download/file"), false);

    bridge.addJob({ ...JOB, id: "after-cancel-download" });
    await until(() => page.posted.some((message) => message.job?.id === "after-cancel-download"));
    page.send({ type: "done", id: "after-cancel-download" });
    await until(() => bridge.calls.some((call) => call.path === "/v1/jobs/after-cancel-download/done"));
  } finally {
    download.restore();
    page.disconnect();
    await bridge.close();
  }
});

test("cancelling an upload discards a queued done and releases the tab", async () => {
  const bridge = await startMockBridge();
  storage.port = bridge.port;
  const page = connect("flow");
  const upload = stalledFetch(`http://127.0.0.1:${bridge.port}/v1/jobs/cancel-upload/file`);
  try {
    bridge.addJob({ ...JOB, id: "cancel-upload" });
    await until(() => page.posted.some((message) => message.type === "job"));
    page.send({ type: "file", id: "cancel-upload", index: 1, mediaType: "image/png", base64: PNG.toString("base64") });
    page.send({ type: "done", id: "cancel-upload" });
    await until(() => upload.request);
    bridge.setState("cancel-upload", "cancelled");
    await until(() => page.posted.some((message) => message.type === "cancel"));
    assert.equal(upload.request.signal?.aborted, true, "cancellation aborts the in-flight upload");
    await until(() => bridge.calls.some((call) => call.path === "/v1/jobs/cancel-upload/fail"));
    assert.equal(bridge.calls.some((call) => call.path === "/v1/jobs/cancel-upload/done"), false);
    const current = await status();
    assert.equal(current.sites.flow.busy, false);
    assert.equal(current.lastError, "");
  } finally {
    upload.restore();
    page.disconnect();
    await bridge.close();
  }
});

test("closing a tab aborts its stalled download and reports the close promptly", async () => {
  const bridge = await startMockBridge();
  storage.port = bridge.port;
  const page = connect("flow");
  const downloadUrl = "https://example.test/closed-tab-download.png";
  const download = stalledFetch(downloadUrl);
  try {
    bridge.addJob({ ...JOB, id: "close-download" });
    await until(() => page.posted.some((message) => message.type === "job"));
    page.send({ type: "fileUrl", id: "close-download", index: 1, url: downloadUrl });
    await until(() => download.request);
    page.disconnect();
    assert.equal(download.request.signal?.aborted, true);
    await until(() => bridge.calls.some((call) => call.path === "/v1/jobs/close-download/fail"), 1000);
    const fail = bridge.calls.find((call) => call.path === "/v1/jobs/close-download/fail");
    assert.deepEqual(JSON.parse(fail.body), { error: "the tab was closed" });
    assert.equal(bridge.calls.some((call) => call.path === "/v1/jobs/close-download/file"), false);
  } finally {
    download.restore();
    page.disconnect();
    await bridge.close();
  }
});

test("a tab closed in the middle of a job fails the job with that reason", async () => {
  const bridge = await startMockBridge();
  storage.port = bridge.port;
  const page = connect("flow");
  bridge.addJob({ ...JOB, id: "c1" });
  await until(() => page.posted.some((message) => message.type === "job"));
  page.disconnect();
  await until(() => bridge.calls.some((call) => call.path === "/v1/jobs/c1/fail"));
  try {
    const fail = bridge.calls.find((call) => call.path === "/v1/jobs/c1/fail");
    assert.deepEqual(JSON.parse(fail.body), { error: "the tab was closed" });
    const current = await status();
    assert.match(current.lastError, /Flow: the tab was closed/);
    assert.equal(current.sites.flow.tabs, 0);
  } finally {
    await bridge.close();
  }
});

test("a job goes to the tab focused most recently, not the one connected last", async () => {
  const bridge = await startMockBridge();
  storage.port = bridge.port;
  const first = connect("flow");
  await new Promise((resolve) => setTimeout(resolve, 10));
  const second = connect("flow");
  await new Promise((resolve) => setTimeout(resolve, 10));
  first.send({ type: "focus" });
  try {
    bridge.addJob({ ...JOB, id: "d1" });
    await until(() => first.posted.some((message) => message.type === "job"));
    await new Promise((resolve) => setTimeout(resolve, 200));
    assert.equal(
      second.posted.some((message) => message.type === "job"),
      false,
      "the other tab gets no job",
    );
    first.send({ type: "done", id: "d1" });
    await until(() => bridge.calls.some((call) => call.path === "/v1/jobs/d1/done"));
  } finally {
    first.disconnect();
    second.disconnect();
    await bridge.close();
  }
});

test("a wrong token shows as unpaired, and the right one clears it", async () => {
  const bridge = await startMockBridge();
  storage.port = bridge.port;
  storage.token = TOKEN;
  const page = connect("flow");
  try {
    await until(async () => (await status()).concat === "ok");
    storage.token = "wrong-token";
    for (const fn of listeners.storage) fn({ token: { newValue: "wrong-token" } }, "local");
    await until(async () => (await status()).concat === "unpaired");
    storage.token = TOKEN;
    for (const fn of listeners.storage) fn({ token: { newValue: TOKEN } }, "local");
    await until(async () => (await status()).concat === "ok");
  } finally {
    page.disconnect();
    await bridge.close();
  }
});
