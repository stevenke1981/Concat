// Concat Bridge: the pure protocol logic, shared by background.js and the tests.
//
// Nothing in this file touches chrome.*, so `node --test` can run it. background.js
// imports these very functions, so the tests exercise the code the browser runs.
// It covers the settings, the request each bridge route needs, the client that
// sends them, and the small state reducers the popup displays.

export const DEFAULT_PORT = 47615;
export const SITE_FLOW = "flow";
export const SITE_CHATGPT_WEB = "chatgpt-web";
export const SITES = Object.freeze([SITE_FLOW, SITE_CHATGPT_WEB]);
// The bridge holds a `next` poll open for at most this long, whatever is asked.
export const MAX_WAIT_SECONDS = 25;

export function isSite(value) {
  return SITES.includes(value);
}

/** A port number from text or a number, or null when it is not a usable port. */
export function parsePort(value) {
  const port = Number(value);
  return Number.isInteger(port) && port >= 1 && port <= 65535 ? port : null;
}

/**
 * Settings as stored: a bad port falls back to the default rather than making
 * every request fail with a confusing error. The token is trimmed because it is
 * pasted from Concat's settings page.
 */
export function normalizeSettings(raw = {}) {
  return {
    port: parsePort(raw.port) ?? DEFAULT_PORT,
    token: typeof raw.token === "string" ? raw.token.trim() : "",
  };
}

export function baseUrl(port) {
  // Always the IPv4 loopback address: "localhost" may resolve to ::1 first.
  return `http://127.0.0.1:${port}`;
}

export function clampWait(seconds) {
  const whole = Math.floor(Number(seconds));
  if (!Number.isFinite(whole) || whole < 0) return 0;
  return Math.min(whole, MAX_WAIT_SECONDS);
}

/**
 * A header value must be printable ASCII: fetch throws on anything else, and a
 * Japanese or accented file name would fail every download. The bridge cleans
 * names the same way, so nothing is lost that it would keep.
 */
export function asciiHeader(text) {
  return String(text ?? "")
    .replace(/[^\x20-\x7e]/g, "_")
    .slice(0, 200);
}

function fractionOf(value) {
  return typeof value === "number" && Number.isFinite(value)
    ? Math.min(1, Math.max(0, value))
    : null;
}

/**
 * The URL, headers and body for one call to the bridge. Every route except
 * `hello` carries the pairing token, which is the only thing that keeps a
 * random web page out of the bridge.
 */
export function buildRequest(settings, call) {
  const base = baseUrl(settings.port);
  const token = settings.token;
  const jobPath = `${base}/v1/jobs/${encodeURIComponent(call.id ?? "")}`;
  const json = (url, body) => ({
    url,
    init: {
      method: "POST",
      headers: { "X-Concat-Token": token, "Content-Type": "application/json" },
      body: JSON.stringify(body),
    },
  });
  switch (call.op) {
    case "hello":
      // The one route with no token: it only says which program answers.
      return { url: `${base}/v1/hello`, init: { method: "GET" } };
    case "next":
      return {
        url: `${base}/v1/jobs/next?site=${encodeURIComponent(call.site)}&wait=${clampWait(call.wait)}`,
        init: { method: "GET", headers: { "X-Concat-Token": token } },
      };
    case "state":
      return {
        url: jobPath,
        init: { method: "GET", headers: { "X-Concat-Token": token } },
      };
    case "progress":
      return json(`${jobPath}/progress`, {
        message: String(call.message ?? ""),
        fraction: fractionOf(call.fraction),
      });
    case "file": {
      // The media type names the extension; X-Index keeps the order of a batch.
      const headers = {
        "X-Concat-Token": token,
        "Content-Type": call.mediaType || "application/octet-stream",
      };
      const name = asciiHeader(call.name);
      if (name) headers["X-File-Name"] = name;
      if (Number.isInteger(call.index) && call.index >= 0) {
        headers["X-Index"] = String(call.index);
      }
      return {
        url: `${jobPath}/file`,
        init: { method: "POST", headers, body: call.bytes },
      };
    }
    case "done":
      return {
        url: `${jobPath}/done`,
        init: { method: "POST", headers: { "X-Concat-Token": token } },
      };
    case "fail":
      return json(`${jobPath}/fail`, { error: String(call.error ?? "") });
    default:
      throw new Error(`unknown bridge call: ${call.op}`);
  }
}

/**
 * A failed call to the bridge. `kind` says what to do about it:
 * - unreachable: nothing answered (Concat is not running, or the port is wrong)
 * - aborted: the caller stopped the request
 * - unauthorized: the token does not match Concat's
 * - notfound: the job is unknown to Concat (it was forgotten or never existed)
 * - conflict: the job can no longer take this post (cancelled or finished)
 * - http: any other answer, or an answer that is not Concat's
 */
export class ConcatError extends Error {
  constructor(kind, message, status = 0) {
    super(message);
    this.name = "ConcatError";
    this.kind = kind;
    this.status = status;
  }
}

export function messageOf(error) {
  return error instanceof Error ? error.message : String(error);
}

/**
 * Sends one call and returns `{status, body}`. The fetch function is a
 * parameter so the tests can drive the same code against a local server.
 */
export async function send(settings, call, { fetchFn = globalThis.fetch, signal } = {}) {
  const { url, init } = buildRequest(settings, call);
  let response;
  try {
    response = await fetchFn(url, { ...init, signal });
  } catch (error) {
    if (signal?.aborted) {
      throw new ConcatError("aborted", "the request was stopped");
    }
    throw new ConcatError("unreachable", `Concat did not answer: ${messageOf(error)}`);
  }
  if (response.status === 204) return { status: 204, body: null };
  const text = await response.text().catch(() => "");
  let body = null;
  try {
    body = text ? JSON.parse(text) : null;
  } catch {
    body = null;
  }
  const detail = body && typeof body.error === "string" ? body.error : "";
  const status = response.status;
  if (status === 401) {
    throw new ConcatError("unauthorized", "Concat did not accept the pairing token", status);
  }
  if (status === 404) {
    throw new ConcatError("notfound", `Concat has no such job${detail ? ` (${detail})` : ""}`, status);
  }
  if (status === 409) {
    throw new ConcatError("conflict", `Concat refused: the job is ${detail || "not open"}`, status);
  }
  if (status < 200 || status >= 300) {
    throw new ConcatError("http", `Concat answered ${status}${detail ? `: ${detail}` : ""}`, status);
  }
  return { status, body };
}

/** Asks Concat who it is. Needs no token, so it also works while unpaired. */
export async function hello(settings, options = {}) {
  const { body } = await send(settings, { op: "hello" }, options);
  if (!body || body.app !== "concat") {
    throw new ConcatError("http", "the port answers, but not as Concat");
  }
  return { version: String(body.version ?? ""), protocol: Number(body.protocol) || 0 };
}

/**
 * One long poll for a job. Resolves `{kind: "none"}` when the wait ends with no
 * job (204), or `{kind: "job", job}` with the job as the bridge describes it.
 * The poll is also stopped after its wait plus a margin, so a silent bridge
 * cannot hold the loop forever.
 */
export async function poll(settings, site, wait, options = {}) {
  const margin = AbortSignal.timeout((clampWait(wait) + 15) * 1000);
  const signal =
    options.signal && typeof AbortSignal.any === "function"
      ? AbortSignal.any([options.signal, margin])
      : options.signal ?? margin;
  const { status, body } = await send(
    settings,
    { op: "next", site, wait },
    { fetchFn: options.fetchFn, signal },
  );
  if (status === 204) return { kind: "none" };
  if (!body || typeof body.id !== "string") {
    throw new ConcatError("http", "Concat sent a job without an id");
  }
  return { kind: "job", job: body };
}

/** The job's state, or "gone" when Concat no longer knows it. */
export async function jobState(settings, id, options = {}) {
  try {
    const { body } = await send(settings, { op: "state", id }, options);
    return typeof body?.state === "string" ? body.state : "unknown";
  } catch (error) {
    if (error instanceof ConcatError && error.kind === "notfound") return "gone";
    throw error;
  }
}

export async function postProgress(settings, id, message, fraction, options = {}) {
  await send(settings, { op: "progress", id, message, fraction }, options);
}

/** Posts one file; resolves to the file name Concat saved it under. */
export async function postFile(settings, id, { bytes, mediaType, name, index }, options = {}) {
  const { body } = await send(
    settings,
    { op: "file", id, bytes, mediaType, name, index },
    options,
  );
  return typeof body?.saved === "string" ? body.saved : "";
}

export async function postDone(settings, id, options = {}) {
  await send(settings, { op: "done", id }, options);
}

export async function postFail(settings, id, error, options = {}) {
  await send(settings, { op: "fail", id, error: String(error || "the page failed") }, options);
}

/** Decodes standard base64 from a content script into bytes. Throws on bad input. */
export function base64ToBytes(base64) {
  const binary = atob(String(base64 ?? ""));
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
  return bytes;
}

/** States after which a job will not change again on Concat's side. */
export function isEndedState(state) {
  return state === "done" || state === "failed" || state === "cancelled" || state === "gone";
}

/** How long a site's poll loop waits before it tries again after an error. */
export function retryDelay(kind) {
  // A wrong token is usually fixed by pasting it again, so the loop checks
  // less often than it does when Concat is simply not running.
  return kind === "unauthorized" ? 10000 : 5000;
}

// --- status shown in the popup ---------------------------------------------

/** The state the popup shows: `concat` is "ok", "offline" or "unpaired". */
export function initialStatus() {
  return { concat: "offline", version: "", lastError: "" };
}

/**
 * Folds one event into the status. Pure, so each transition can be tested.
 * Connection problems set `concat`; `lastError` is for things the person should
 * read, such as a job that failed.
 */
export function reduceStatus(status, event) {
  switch (event.type) {
    case "hello-ok":
      return {
        ...status,
        version: event.version || status.version,
        // Hello does not check the token, so an "unpaired" state stays until a poll says otherwise.
        concat: status.concat === "offline" ? "ok" : status.concat,
      };
    case "hello-fail":
      return { ...status, concat: "offline", version: "" };
    case "settings-changed":
      // The old state may describe the old token. "offline" here means "being checked",
      // and the hello that follows (or the next poll) gives the real answer.
      return { ...status, concat: "offline" };
    case "poll-ok":
      return { ...status, concat: "ok" };
    case "poll-unauthorized":
      return { ...status, concat: "unpaired" };
    case "poll-unreachable":
      return { ...status, concat: "offline" };
    case "error":
      return { ...status, lastError: String(event.message || "") };
    case "clear-error":
      return { ...status, lastError: "" };
    default:
      return status;
  }
}

/** Maps a failed poll onto the status event it causes. */
export function eventForError(error) {
  if (!(error instanceof ConcatError)) {
    return { type: "error", message: messageOf(error) };
  }
  if (error.kind === "unreachable" || error.kind === "aborted") {
    return { type: "poll-unreachable" };
  }
  if (error.kind === "unauthorized") return { type: "poll-unauthorized" };
  return { type: "error", message: error.message };
}

/** Per-site tab counts for the popup: `{flow: {tabs, busy}, "chatgpt-web": {...}}`. */
export function summarizeSites(entries) {
  const sites = Object.fromEntries(SITES.map((site) => [site, { tabs: 0, busy: false }]));
  for (const entry of entries) {
    const row = sites[entry.site];
    if (!row) continue;
    row.tabs += 1;
    row.busy = row.busy || Boolean(entry.busy);
  }
  return sites;
}

/**
 * The tab that gets a job: the most recently focused one, or the one connected
 * last when none was focused. Busy tabs are never chosen. Returns null when no
 * tab is free.
 */
export function pickPort(candidates) {
  let best = null;
  let bestAt = -Infinity;
  for (const candidate of candidates) {
    if (candidate.busy) continue;
    const at = Math.max(candidate.focusedAt || 0, candidate.connectedAt || 0);
    if (at > bestAt) {
      best = candidate;
      bestAt = at;
    }
  }
  return best;
}
