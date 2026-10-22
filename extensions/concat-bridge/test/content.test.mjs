// Smoke tests for the content scripts (sites/common.js, flow.js, chatgpt.js).
//
// They are loaded into a Node vm context with a stub of the few browser and
// chrome.* objects they touch on load and in connectSite. This checks the
// port protocol (hello, focus, job, progress, file, fileUrl, done, fail, cancel,
// selfcheck, reconnect) and that each site script fails a job cleanly on a page
// with no prompt box. It does NOT check the live Flow or ChatGPT DOM; that is
// only possible in the browser.
import { test } from "node:test";
import assert from "node:assert/strict";
import vm from "node:vm";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const sitesDir = join(here, "..", "sites");
const plain = (value) => JSON.parse(JSON.stringify(value));

function fakePort() {
  const port = {
    posted: [],
    messageListeners: [],
    disconnectListeners: [],
    postMessage(message) {
      port.posted.push(message);
    },
    onMessage: { addListener: (fn) => port.messageListeners.push(fn) },
    onDisconnect: { addListener: (fn) => port.disconnectListeners.push(fn) },
    receive(message) {
      for (const fn of port.messageListeners) fn(message);
    },
    disconnect() {
      for (const fn of port.disconnectListeners) fn();
    },
  };
  return port;
}

/** A vm context with the browser and chrome.* surface the scripts need. */
function pageContext() {
  const ports = [];
  const runtimeListeners = [];
  const context = {
    console,
    setTimeout,
    clearTimeout,
    AbortController,
    AbortSignal,
    location: { pathname: "/fx/tools/flow" },
    window: { addEventListener() {} },
    document: {
      visibilityState: "visible",
      addEventListener() {},
      // An empty page: no prompt box, no media, no progress bars.
      querySelectorAll: () => [],
      querySelector: () => null,
    },
    chrome: {
      runtime: {
        lastError: undefined,
        connect() {
          const port = fakePort();
          ports.push(port);
          return port;
        },
        onMessage: { addListener: (fn) => runtimeListeners.push(fn) },
      },
    },
  };
  vm.createContext(context);
  return { context, ports, runtimeListeners };
}

function load(context, file) {
  vm.runInContext(readFileSync(join(sitesDir, file), "utf8"), context, { filename: file });
}

/** Asks the page for its self-check the way the popup does. */
function selfcheck(runtimeListeners) {
  return new Promise((resolve) => {
    const answered = runtimeListeners.some((fn) => fn({ type: "selfcheck" }, {}, resolve) === true);
    assert.ok(answered, "the page answers the self-check");
  });
}

const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

test("common.js connects, runs a job, and reports each step in order", async () => {
  const { context, ports, runtimeListeners } = pageContext();
  load(context, "common.js");
  const CB = context.ConcatBridge;
  CB.connectSite(
    "flow",
    async (job, ctx) => {
      ctx.progress("rendering", 0.5);
      ctx.file(1, "flow-1", "image/png", "AAAA");
      ctx.fileUrl(2, "https://example.test/two.png");
    },
    () => ({ promptBox: "not found" }),
  );
  const port = ports[0];
  assert.equal(port.posted[0].type, "hello");
  assert.equal(port.posted[0].site, "flow");
  port.receive({ type: "job", job: { id: "j1", kind: "image", prompt: "x" } });
  await wait(20);
  assert.deepEqual(
    plain(port.posted.map((message) => message.type)),
    ["hello", "focus", "progress", "file", "fileUrl", "done"],
  );
  assert.deepEqual(plain(port.posted[2]), { type: "progress", id: "j1", message: "rendering", fraction: 0.5 });
  assert.deepEqual(plain(port.posted[3]), {
    type: "file",
    id: "j1",
    index: 1,
    name: "flow-1",
    mediaType: "image/png",
    base64: "AAAA",
  });

  const reply = await selfcheck(runtimeListeners);
  assert.equal(reply.ok, true);
  assert.equal(reply.site, "flow");
  assert.deepEqual(plain(reply.report), { promptBox: "not found" });
});

test("a cancel stops the running job, which then reports it was cancelled", async () => {
  const { context, ports } = pageContext();
  load(context, "common.js");
  const CB = context.ConcatBridge;
  let sawAbort = false;
  CB.connectSite("chatgpt-web", async (job, ctx) => {
    try {
      await CB.sleep(10000, ctx.signal);
    } catch (error) {
      sawAbort = ctx.signal.aborted;
      throw error;
    }
  });
  const port = ports[0];
  port.receive({ type: "job", job: { id: "c1", kind: "image" } });
  await wait(20);
  port.receive({ type: "cancel", id: "c1" });
  await wait(20);
  assert.equal(sawAbort, true);
  const fail = plain(port.posted.at(-1));
  assert.deepEqual(fail, { type: "fail", id: "c1", error: "cancelled" });
});

test("a lost connection aborts the job and the script reconnects", async () => {
  const { context, ports } = pageContext();
  load(context, "common.js");
  const CB = context.ConcatBridge;
  let aborted = false;
  CB.connectSite("flow", async (job, ctx) => {
    try {
      await CB.sleep(10000, ctx.signal);
    } catch {
      aborted = ctx.signal.aborted;
      throw new Error("aborted");
    }
  });
  ports[0].receive({ type: "job", job: { id: "d1", kind: "image" } });
  await wait(20);
  ports[0].disconnect();
  assert.equal(aborted, false, "the job is still running at the moment of the drop");
  await wait(20);
  assert.equal(aborted, true, "the drop aborts the running job");
  await wait(1200);
  assert.equal(ports.length, 2, "the script reconnected after its first retry delay");
  assert.equal(ports[1].posted[0].type, "hello");
});

test("flow.js on a page with no prompt box fails the job with the reason", async () => {
  const { context, ports } = pageContext();
  load(context, "common.js");
  load(context, "flow.js");
  const port = ports[0];
  assert.equal(port.posted[0].site, "flow", "flow.js says hello as the flow site");
  port.receive({ type: "job", job: { id: "f1", kind: "image", prompt: "a fox" } });
  await wait(20);
  const fail = plain(port.posted.at(-1));
  assert.equal(fail.type, "fail");
  assert.equal(fail.id, "f1");
  assert.match(fail.error, /prompt box was not found/);
});

test("chatgpt.js on a page with no message box fails the job with the reason", async () => {
  const { context, ports, runtimeListeners } = pageContext();
  load(context, "common.js");
  load(context, "chatgpt.js");
  const port = ports[0];
  assert.equal(port.posted[0].site, "chatgpt-web");
  port.receive({ type: "job", job: { id: "g1", kind: "image", prompt: "a fox", count: 1 } });
  await wait(20);
  const fail = plain(port.posted.at(-1));
  assert.equal(fail.type, "fail");
  assert.match(fail.error, /message box was not found/);

  const reply = await selfcheck(runtimeListeners);
  assert.equal(reply.report.promptBox, "not found");
  assert.equal(reply.report.assistantReplies, 0);
});

test("a video job sent to the ChatGPT page is refused, not attempted", async () => {
  const { context, ports } = pageContext();
  load(context, "common.js");
  load(context, "chatgpt.js");
  ports[0].receive({ type: "job", job: { id: "v1", kind: "video", prompt: "x", count: 1 } });
  await wait(20);
  const fail = plain(ports[0].posted.at(-1));
  assert.deepEqual(fail, { type: "fail", id: "v1", error: "ChatGPT on the web makes pictures only" });
});
