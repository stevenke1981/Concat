// Concat Bridge service worker: the only part of the extension that talks to Concat.
//
// The content scripts run inside pages we do not control (Flow, chatgpt.com),
// where a request to 127.0.0.1 is subject to the page's CORS and Private Network
// Access rules. This worker has host permission for 127.0.0.1, so the content
// scripts hand it their work over a Port and it does the HTTP. Every wire call
// goes through lib/protocol.js, which the tests also run.

import {
  DEFAULT_PORT,
  SITES,
  base64ToBytes,
  eventForError,
  hello,
  initialStatus,
  isEndedState,
  isSite,
  messageOf,
  normalizeSettings,
  pickPort,
  poll,
  postDone,
  postFail,
  postFile,
  postProgress,
  reduceStatus,
  retryDelay,
  summarizeSites,
  jobState,
  ConcatError,
} from "./lib/protocol.js";

// These must match sites/common.js.
const PORT_NAME = "concat-site";
const SITE_LABEL = { flow: "Flow", "chatgpt-web": "ChatGPT" };

// Long-poll length; the bridge answers 204 after this when no job came.
const LONG_POLL_SECONDS = 20;
// A running job is checked for a cancel this often.
const CANCEL_CHECK_MS = 5000;
// After a cancel, the tab gets this long to stop before the job is released.
const CANCEL_GRACE_MS = 15000;
// A site whose tabs are all busy re-checks this often.
const IDLE_RECHECK_MS = 30000;
// Chrome 120+ allows 30 s alarms. The alarm wakes this worker, which restarts
// any poll loop it lost, so a job is not waiting for a tab reload.
const KEEPALIVE_ALARM = "concat-keepalive";
const KEEPALIVE_MINUTES = 0.5;

const log = (...args) => console.log("[Concat Bridge]", ...args);
const warn = (...args) => console.warn("[Concat Bridge]", ...args);

// Everything below lives in memory. The worker can be stopped at any time; the
// content scripts reconnect, and that rebuilds the state.
let status = initialStatus();
let nextPageId = 1;
const pages = new Map(); // page id -> { id, port, site, connectedAt, focusedAt, job }
const loops = new Map(); // site -> { site, stopped, controller, wake }
const runs = new Map(); // job id -> run

async function currentSettings() {
  const stored = await chrome.storage.local.get({ port: DEFAULT_PORT, token: "" });
  return normalizeSettings(stored);
}

function apply(event) {
  status = reduceStatus(status, event);
}

function pagesFor(site) {
  return [...pages.values()].filter((page) => page.site === site);
}

// --- connections from the content scripts ----------------------------------

chrome.runtime.onConnect.addListener((port) => {
  if (port.name !== PORT_NAME) return;
  const page = {
    id: nextPageId++,
    port,
    site: null,
    connectedAt: Date.now(),
    focusedAt: 0,
    job: null,
  };
  pages.set(page.id, page);
  port.onMessage.addListener((message) => onPageMessage(page, message));
  port.onDisconnect.addListener(() => onPageGone(page));
});

function onPageMessage(page, message) {
  if (!message || typeof message !== "object") return;
  if (message.type === "hello") {
    if (!isSite(message.site)) {
      warn("a page said hello for an unknown site:", message.site);
      return;
    }
    page.site = message.site;
    page.connectedAt = Date.now();
    kick();
    return;
  }
  // Nothing else is expected from a page before it says which site it is.
  if (!page.site) return;
  if (message.type === "focus") {
    page.focusedAt = Date.now();
    return;
  }
  const run = page.job;
  if (!run || run.id !== message.id) {
    warn(`ignored "${message.type}" for a job that is not running in this tab`);
    return;
  }
  switch (message.type) {
    case "progress":
      enqueue(run, () => reportProgress(run, message));
      break;
    case "file":
      enqueue(run, () => saveFile(run, message.index, message.name, message.mediaType, message.base64));
      break;
    case "fileUrl":
      enqueue(run, () => saveFromUrl(run, message.index, message.url));
      break;
    case "done":
      enqueue(run, () => endRun(run, {}));
      break;
    case "fail":
      // A page that stopped because of a cancel is not a failure to report.
      enqueue(run, () =>
        endRun(run, {
          error: String(message.error || "the page failed"),
          silent: run.cancelRequested,
        }),
      );
      break;
    default:
      break;
  }
}

function onPageGone(page) {
  pages.delete(page.id);
  const run = page.job;
  if (run && !run.ended) {
    run.terminalError = "the tab was closed";
    run.controller.abort();
    enqueue(run, () => endRun(run));
  }
  kick();
}

// --- a job's steps, in order ------------------------------------------------

// Steps for one job run one after another. A file posted before "done" is
// therefore saved before the job ends, whatever order the messages arrived in.
function enqueue(run, step) {
  run.queue = run.queue.then(async () => {
    if (run.ended) return;
    try {
      await step();
    } catch (error) {
      // Cancel and disconnect abort transfers first, so their terminal step can
      // run next instead of waiting behind a stalled download or upload.
      if (run.controller.signal.aborted) return;
      // A post refused as a conflict means Concat already ended the job (a cancel).
      // That is what the person asked for, so it is not shown as an error.
      const expected = error instanceof ConcatError && error.kind === "conflict";
      warn(`job ${run.id}: ${messageOf(error)}`);
      await endRun(run, { error: messageOf(error), silent: expected });
    }
  });
}

async function reportProgress(run, message) {
  try {
    await postProgress(await currentSettings(), run.id, message.message, message.fraction, {
      signal: run.controller.signal,
    });
  } catch (error) {
    // A progress note is not worth failing a job for; the file or done step decides.
    warn(`progress for job ${run.id} was not delivered: ${messageOf(error)}`);
  }
}

async function saveFile(run, index, name, mediaType, base64) {
  const bytes = base64ToBytes(base64);
  await postFile(
    await currentSettings(),
    run.id,
    {
      bytes,
      mediaType: mediaType || "",
      name: name || `${run.site}-${index}`,
      index: Number.isInteger(index) ? index : undefined,
    },
    { signal: run.controller.signal },
  );
}

// The page could not hold a large file (or could not read it), so it gives us a
// link instead. The fetch uses the browser's cookies for that host, which the
// page itself could not pass on in a message.
async function saveFromUrl(run, index, url) {
  if (typeof url !== "string" || !/^https:\/\//i.test(url)) {
    throw new Error("the page gave a file address the bridge cannot fetch");
  }
  const response = await fetch(url, { credentials: "include", signal: run.controller.signal });
  if (!response.ok) {
    throw new Error(`the file's address answered ${response.status}`);
  }
  const blob = await response.blob();
  await postFile(
    await currentSettings(),
    run.id,
    {
      bytes: new Uint8Array(await blob.arrayBuffer()),
      mediaType: blob.type || response.headers.get("content-type") || "",
      name: `${run.site}-${index}`,
      index: Number.isInteger(index) ? index : undefined,
    },
    { signal: run.controller.signal },
  );
}

/**
 * Ends a job once, by telling Concat it is done or failed. Afterwards the tab is
 * free and the site's poll loop may take another job.
 */
async function endRun(run, { error = "", silent = false } = {}) {
  if (run.ended) return;
  // A close may arrive while a transfer and its following done are queued.
  if (run.terminalError) error = run.terminalError;
  if (run.cancelRequested) {
    error = "cancelled";
    silent = true;
  }
  run.ended = true;
  run.controller.abort();
  clearInterval(run.watch);
  clearTimeout(run.graceTimer);
  runs.delete(run.id);
  if (run.page.job === run) run.page.job = null;
  const label = SITE_LABEL[run.site] ?? run.site;
  if (error && !silent) apply({ type: "error", message: `${label}: ${error}` });
  try {
    const settings = await currentSettings();
    if (error) {
      await postFail(settings, run.id, error);
    } else {
      await postDone(settings, run.id);
      apply({ type: "clear-error" });
    }
  } catch (cause) {
    // Concat refuses a post for a job it already ended, such as one cancelled
    // from Concat. That is expected, not an error to show.
    if (!(cause instanceof ConcatError && cause.kind === "conflict")) {
      warn(`job ${run.id} did not end cleanly: ${messageOf(cause)}`);
      apply(eventForError(cause));
    }
  }
  kick();
}

// --- handing jobs to tabs ----------------------------------------------------

function post(page, message) {
  try {
    page.port.postMessage(message);
    return true;
  } catch (error) {
    warn("a tab's connection is gone:", messageOf(error));
    return false;
  }
}

/** Gives a job to the best free tab for its site. Returns the run, or null when none is free. */
function dispatch(site, job) {
  const candidates = pagesFor(site).map((page) => ({
    page,
    busy: page.job !== null,
    focusedAt: page.focusedAt,
    connectedAt: page.connectedAt,
  }));
  const chosen = pickPort(candidates);
  if (!chosen) return null;
  const page = chosen.page;
  const run = {
    id: job.id,
    site,
    page,
    ended: false,
    cancelRequested: false,
    terminalError: "",
    controller: new AbortController(),
    queue: Promise.resolve(),
    watch: null,
    graceTimer: null,
    checking: false,
  };
  runs.set(job.id, run);
  page.job = run;
  run.watch = setInterval(() => checkJob(run), CANCEL_CHECK_MS);
  if (!post(page, { type: "job", job })) {
    enqueue(run, () => endRun(run, { error: "the tab was closed" }));
  }
  return run;
}

/** Concat may cancel a job while the page works on it; the page is told to stop. */
async function checkJob(run) {
  if (run.ended || run.checking || run.cancelRequested) return;
  run.checking = true;
  try {
    const state = await jobState(await currentSettings(), run.id);
    if (isEndedState(state)) requestCancel(run);
  } catch (error) {
    // One failed check is not a reason to stop; the next check tries again.
    warn(`could not check job ${run.id}: ${messageOf(error)}`);
  } finally {
    run.checking = false;
  }
}

function requestCancel(run) {
  if (run.ended || run.cancelRequested) return;
  run.cancelRequested = true;
  run.controller.abort();
  post(run.page, { type: "cancel", id: run.id });
  // If the page never reports back, release the job anyway so the tab is not stuck.
  run.graceTimer = setTimeout(
    () => enqueue(run, () => endRun(run, { error: "cancelled", silent: true })),
    CANCEL_GRACE_MS,
  );
}

// --- one long-poll loop per site with a connected tab ------------------------

/** Makes sure each site with a tab has a poll loop, and stops loops whose tabs are gone. */
function kick() {
  for (const site of SITES) {
    const loop = loops.get(site);
    if (pagesFor(site).length === 0) {
      stopLoop(site);
    } else if (loop) {
      wakeLoop(loop);
    } else {
      startLoop(site);
    }
  }
}

function startLoop(site) {
  const loop = { site, stopped: false, controller: null, wake: null };
  loops.set(site, loop);
  runLoop(loop)
    .catch((error) => warn(`poll loop for ${site} stopped: ${messageOf(error)}`))
    .finally(() => {
      if (loops.get(site) === loop) loops.delete(site);
    });
}

function stopLoop(site) {
  const loop = loops.get(site);
  if (!loop) return;
  loops.delete(site);
  loop.stopped = true;
  loop.controller?.abort();
  wakeLoop(loop);
}

function wakeLoop(loop) {
  const wake = loop.wake;
  loop.wake = null;
  wake?.();
}

/** Waits `ms`, or less if the loop is woken. */
function pause(loop, ms) {
  return new Promise((resolve) => {
    const finish = () => {
      clearTimeout(timer);
      loop.wake = null;
      resolve();
    };
    const timer = setTimeout(finish, ms);
    loop.wake = finish;
  });
}

async function runLoop(loop) {
  while (!loop.stopped) {
    const pagesNow = pagesFor(loop.site);
    if (pagesNow.length === 0) return;
    if (pagesNow.every((page) => page.job !== null)) {
      // Every tab is busy. Taking a job now would leave it with nowhere to go, so
      // the queue is left alone until a tab frees up.
      await pause(loop, IDLE_RECHECK_MS);
      continue;
    }
    loop.controller = new AbortController();
    let reply;
    try {
      reply = await poll(await currentSettings(), loop.site, LONG_POLL_SECONDS, {
        signal: loop.controller.signal,
      });
    } catch (error) {
      loop.controller = null;
      if (loop.stopped) return;
      apply(eventForError(error));
      await pause(loop, retryDelay(error?.kind));
      continue;
    }
    loop.controller = null;
    apply({ type: "poll-ok" });
    // A job that arrived is handled even when the loop was stopped meanwhile:
    // Concat has already handed it over, so dropping it here would strand it.
    if (reply.kind === "job") accept(loop.site, reply.job);
  }
}

function accept(site, job) {
  if (job.site !== site) {
    failUntaken(job.id, "the bridge sent a job for another site");
    return;
  }
  if (!dispatch(site, job)) {
    failUntaken(job.id, "the tab was closed before the job could start");
  }
}

async function failUntaken(id, reason) {
  try {
    await postFail(await currentSettings(), id, reason);
  } catch (error) {
    warn(`could not fail job ${id}: ${messageOf(error)}`);
  }
  apply({ type: "error", message: reason });
}

// Changing the port or the token invalidates the polls in flight, so they are
// stopped and begun again with the new settings. The state is reset first: a
// long poll with a good token is held open, so "unpaired" would otherwise
// linger until the poll returns.
chrome.storage.onChanged.addListener((changes, area) => {
  if (area !== "local" || !("port" in changes || "token" in changes)) return;
  apply({ type: "settings-changed" });
  for (const site of [...loops.keys()]) stopLoop(site);
  kick();
  checkConcat();
});

chrome.alarms.create(KEEPALIVE_ALARM, { periodInMinutes: KEEPALIVE_MINUTES });
chrome.alarms.onAlarm.addListener((alarm) => {
  if (alarm.name === KEEPALIVE_ALARM) kick();
});

// --- status for the popup ----------------------------------------------------

/** Asks Concat who answers (no token needed) and records it. */
async function checkConcat() {
  try {
    const info = await hello(await currentSettings(), { signal: AbortSignal.timeout(3000) });
    apply({ type: "hello-ok", version: info.version });
  } catch {
    apply({ type: "hello-fail" });
  }
}

async function snapshot() {
  await checkConcat();
  const entries = [...pages.values()].map((page) => ({ site: page.site, busy: page.job !== null }));
  return { ...status, sites: summarizeSites(entries) };
}

chrome.runtime.onMessage.addListener((message, sender, sendResponse) => {
  if (!message || message.type !== "status") return false;
  snapshot().then(sendResponse, (error) => sendResponse({ ...status, lastError: messageOf(error) }));
  return true; // the answer is sent later
});

log("service worker started");
