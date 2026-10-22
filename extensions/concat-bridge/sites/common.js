// Concat Bridge: helpers shared by the two site scripts (flow.js and chatgpt.js).
//
// Content scripts are classic scripts and cannot import modules, so everything
// is exposed as one object on globalThis.ConcatBridge. Keeping to one global
// avoids clashes with the page's own names.
//
// Every site-specific selector lives in the SELECTORS object at the top of the
// site file, not here. The helpers below are generic DOM and port plumbing.
(() => {
  "use strict";
  if (globalThis.ConcatBridge) return;

  const LOG = "[Concat Bridge]";
  // Chrome limits one port message to 64 MiB and base64 adds a third to the
  // size, so a larger file goes by URL and the background downloads it instead.
  const MAX_INLINE_BYTES = 32 * 1024 * 1024;

  const log = (...args) => console.log(LOG, ...args);
  const messageOf = (error) => (error instanceof Error ? error.message : String(error));

  class CancelledError extends Error {
    constructor(message = "cancelled") {
      super(message);
      this.name = "CancelledError";
    }
  }

  class TooLargeError extends Error {
    constructor(bytes) {
      super(`the file is ${Math.round(bytes / 1048576)} MB, too large to pass through the page`);
      this.name = "TooLargeError";
    }
  }

  /** Throws the abort reason (or a cancel) when the signal has been aborted. */
  function throwIfAborted(signal) {
    if (signal?.aborted) {
      throw signal.reason instanceof Error ? signal.reason : new CancelledError();
    }
  }

  /** Resolves after `ms`; rejects at once if the signal aborts first. */
  function sleep(ms, signal) {
    return new Promise((resolve, reject) => {
      throwIfAborted(signal);
      const onAbort = () => {
        clearTimeout(timer);
        reject(signal.reason instanceof Error ? signal.reason : new CancelledError());
      };
      const timer = setTimeout(() => {
        signal?.removeEventListener("abort", onAbort);
        resolve();
      }, ms);
      signal?.addEventListener("abort", onAbort, { once: true });
    });
  }

  /**
   * Calls `check` until it returns a truthy value, or the timeout passes.
   * Pages render late, so each step waits for the element it needs.
   */
  async function waitFor(check, { timeout = 10000, interval = 250, signal, label = "the page" } = {}) {
    const deadline = Date.now() + timeout;
    for (;;) {
      throwIfAborted(signal);
      const value = await check();
      if (value) return value;
      if (Date.now() >= deadline) throw new Error(`timed out waiting for ${label}`);
      await sleep(interval, signal);
    }
  }

  function textOf(el) {
    return (el?.textContent || "").replace(/\s+/g, " ").trim();
  }

  function isVisible(el) {
    if (!el || !el.isConnected) return false;
    const style = getComputedStyle(el);
    if (style.display === "none" || style.visibility === "hidden" || style.opacity === "0") {
      return false;
    }
    const rect = el.getBoundingClientRect();
    return rect.width > 0 && rect.height > 0;
  }

  function isEnabled(el) {
    return !el.disabled && el.getAttribute("aria-disabled") !== "true";
  }

  /**
   * React keeps its own copy of a textarea's value and ignores plain assignments.
   * Setting the value through the native prototype setter, then firing input and
   * change, makes React read it the way it would for typing.
   */
  function setNativeValue(textarea, text) {
    const setter = Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, "value").set;
    textarea.focus();
    setter.call(textarea, text);
    textarea.dispatchEvent(new Event("input", { bubbles: true }));
    textarea.dispatchEvent(new Event("change", { bubbles: true }));
  }

  /**
   * Replaces the content of a contenteditable editor (ProseMirror, Lexical, and
   * the like). execCommand is the path these editors understand; when a page
   * refuses it, the paragraphs are built directly and an input event announces
   * them. Returns true when execCommand did the work.
   */
  function insertIntoContentEditable(el, text) {
    el.focus();
    window.getSelection().selectAllChildren(el);
    let inserted = false;
    try {
      inserted = document.execCommand("insertText", false, text);
    } catch {
      inserted = false;
    }
    const probe = text.replace(/\s+/g, " ").trim().slice(0, 30);
    if (inserted && (probe === "" || textOf(el).includes(probe))) return true;
    el.replaceChildren(
      ...text.split("\n").map((line) => {
        const paragraph = document.createElement("p");
        if (line) paragraph.textContent = line;
        else paragraph.appendChild(document.createElement("br"));
        return paragraph;
      }),
    );
    el.dispatchEvent(new InputEvent("input", { bubbles: true, inputType: "insertText", data: text }));
    return false;
  }

  /** Puts `text` into a textarea or a contenteditable, whichever `box` is. */
  function setPrompt(box, text) {
    if (box instanceof HTMLTextAreaElement) setNativeValue(box, text);
    else insertIntoContentEditable(box, text);
  }

  /**
   * Presses a key the way a keyboard does. Editors listen for keydown, and some
   * for keypress as well, so all three are sent.
   */
  function pressKey(el, key) {
    const keyCode = { Enter: 13, Escape: 27 }[key] ?? 0;
    for (const type of ["keydown", "keypress", "keyup"]) {
      el.dispatchEvent(
        new KeyboardEvent(type, { key, code: key, keyCode, which: keyCode, bubbles: true, cancelable: true }),
      );
    }
  }

  /**
   * Presses the way a person does. Frameworks often react to the pointer
   * sequence rather than to a bare click, so the whole sequence is sent.
   */
  function clickLikeUser(el) {
    el.scrollIntoView?.({ block: "center" });
    const rect = el.getBoundingClientRect();
    const init = {
      bubbles: true,
      cancelable: true,
      composed: true,
      view: window,
      button: 0,
      clientX: rect.left + rect.width / 2,
      clientY: rect.top + rect.height / 2,
    };
    const pointer = { ...init, pointerId: 1, pointerType: "mouse", isPrimary: true };
    el.dispatchEvent(new PointerEvent("pointerdown", pointer));
    el.dispatchEvent(new MouseEvent("mousedown", init));
    el.dispatchEvent(new PointerEvent("pointerup", pointer));
    el.dispatchEvent(new MouseEvent("mouseup", init));
    el.dispatchEvent(new MouseEvent("click", init));
  }

  /**
   * The words a control announces: its aria-label, its title, and its visible text.
   * Material Symbols icons show their ligature name as text (e.g. "arrow_forward"),
   * so that is matched too. Long text is skipped, because a big container's text
   * would match words it merely contains.
   */
  function labelOf(el) {
    const text = textOf(el);
    return [el.getAttribute("aria-label"), el.getAttribute("title"), text.length <= 160 ? text : ""]
      .filter(Boolean)
      .join(" ");
  }

  const CLICKABLE =
    "button, [role='button'], [role='option'], [role='menuitem'], [role='menuitemradio'], [role='combobox'], [role='tab'], [role='radio']";

  /**
   * The first visible control whose label matches. `patterns` is tried in order,
   * so an earlier pattern beats a later one wherever it sits on the page.
   * Patterns must not carry the g flag, which would make `test` stateful.
   */
  function findButtonByText(patterns, root = document, { enabledOnly = false } = {}) {
    const candidates = [...root.querySelectorAll(CLICKABLE)].filter(
      (el) => isVisible(el) && (!enabledOnly || isEnabled(el)),
    );
    const labelled = candidates.map((el) => [el, labelOf(el)]);
    for (const pattern of patterns) {
      for (const [el, label] of labelled) {
        if (pattern.test(label)) return el;
      }
    }
    return null;
  }

  /** The source address of an img, video or source element. */
  function srcOf(el) {
    return el.currentSrc || el.src || el.getAttribute("src") || "";
  }

  /** The larger of the natural and the drawn size, so a thumbnail that is really large still counts. */
  function sizeOf(el) {
    const box = (el.tagName === "SOURCE" ? el.parentElement : el) ?? el;
    const rect = box.getBoundingClientRect();
    return Math.max(
      el.naturalWidth || 0,
      el.naturalHeight || 0,
      el.videoWidth || 0,
      el.videoHeight || 0,
      rect.width,
      rect.height,
    );
  }

  /**
   * The media that are on the page now, as a set of addresses. Taken before a job
   * is submitted, so only media that appear afterwards count as its results.
   * Only media at least 200 px in size count, so icons and avatars are ignored.
   */
  function mediaSnapshot(root = document) {
    const seen = new Set();
    for (const el of root.querySelectorAll("img, video, source")) {
      const src = srcOf(el);
      if (src && sizeOf(el) >= 200) seen.add(src);
    }
    return seen;
  }

  function blobToBase64(blob) {
    return new Promise((resolve, reject) => {
      const reader = new FileReader();
      reader.onload = () => {
        const text = String(reader.result);
        resolve(text.slice(text.indexOf(",") + 1));
      };
      reader.onerror = () => reject(reader.error || new Error("could not read the download"));
      reader.readAsDataURL(blob);
    });
  }

  /**
   * Downloads a file the way the page would (with its cookies) and returns it as
   * base64. Throws when the download fails or is too large, so the caller can
   * fall back to handing Concat the address.
   */
  async function fetchAsBase64(url, { signal, maxBytes = MAX_INLINE_BYTES } = {}) {
    const response = await fetch(url, { credentials: "include", signal });
    if (!response.ok) {
      response.body?.cancel().catch(() => {});
      throw new Error(`the download answered ${response.status}`);
    }
    const declared = Number(response.headers.get("content-length"));
    if (declared > maxBytes) {
      response.body?.cancel().catch(() => {});
      throw new TooLargeError(declared);
    }
    const blob = await response.blob();
    if (blob.size > maxBytes) throw new TooLargeError(blob.size);
    return {
      mediaType: blob.type || response.headers.get("content-type") || "application/octet-stream",
      base64: await blobToBase64(blob),
      size: blob.size,
    };
  }

  /**
   * Sends each result to Concat. A file the page can read goes inline; any other
   * is handed over by address, and the background downloads it itself.
   */
  async function deliverFiles(urls, ctx, { label }) {
    for (let i = 0; i < urls.length; i++) {
      const index = i + 1;
      ctx.progress(`${label}: saving ${index} of ${urls.length}`, null);
      try {
        const got = await fetchAsBase64(urls[i], { signal: ctx.signal });
        ctx.file(index, `${label.toLowerCase()}-${index}`, got.mediaType, got.base64);
      } catch (error) {
        throwIfAborted(ctx.signal);
        log(`file ${index} is handed over by address: ${messageOf(error)}`);
        ctx.fileUrl(index, urls[i]);
      }
    }
  }

  /**
   * Wires a site script to the extension. Opens a long-lived Port named
   * "concat-site", says which site this is, and then runs one job at a time:
   * `runJob(job, ctx)` resolves once its files are posted, and the job is then
   * marked done. `selfcheck()` answers the popup's "Check this page" request.
   *
   * The Port is reopened if the background worker restarts. A job whose Port
   * drops is aborted, because nothing could receive its files anymore.
   */
  function connectSite(site, runJob, selfcheck) {
    let port = null;
    let current = null; // { id, controller } for the job running in this tab
    let retryMs = 1000;
    let stopped = false;

    function post(message) {
      if (!port) return false;
      try {
        port.postMessage(message);
        return true;
      } catch {
        port = null;
        return false;
      }
    }

    function connect() {
      if (stopped) return;
      try {
        port = chrome.runtime.connect({ name: "concat-site" });
      } catch {
        // The extension was reloaded or removed, so this copy of the script is orphaned.
        stopped = true;
        log("the extension was reloaded; reload this page to use Concat Bridge");
        return;
      }
      port.onMessage.addListener(onMessage);
      port.onDisconnect.addListener(() => {
        void chrome.runtime.lastError; // read it so Chrome does not log it as unchecked
        port = null;
        if (current) current.controller.abort(new Error("the connection to Concat Bridge was lost"));
        setTimeout(connect, retryMs);
        retryMs = Math.min(retryMs * 2, 10000);
      });
      retryMs = 1000;
      post({ type: "hello", site });
      if (document.visibilityState === "visible") post({ type: "focus" });
    }

    // The worker sends a job to the tab most recently focused, so each focus is reported.
    const reportFocus = () => {
      if (document.visibilityState === "visible") post({ type: "focus" });
    };
    window.addEventListener("focus", reportFocus);
    document.addEventListener("visibilitychange", reportFocus);

    function onMessage(message) {
      if (!message) return;
      if (message.type === "job") {
        startJob(message.job);
      } else if (message.type === "cancel" && current && current.id === message.id) {
        current.controller.abort(new CancelledError());
      }
    }

    async function startJob(job) {
      if (current) {
        post({ type: "fail", id: job.id, error: "this tab is already making a job" });
        return;
      }
      const controller = new AbortController();
      const run = { id: job.id, controller };
      current = run;
      const ctx = {
        signal: controller.signal,
        progress(message, fraction = null) {
          post({
            type: "progress",
            id: job.id,
            message: String(message),
            fraction: typeof fraction === "number" ? fraction : null,
          });
        },
        file(index, name, mediaType, base64) {
          post({ type: "file", id: job.id, index, name, mediaType, base64 });
        },
        fileUrl(index, url) {
          post({ type: "fileUrl", id: job.id, index, url });
        },
      };
      log(`job ${job.id} (${job.kind}) started on ${site}`);
      try {
        await runJob(job, ctx);
        throwIfAborted(controller.signal);
        post({ type: "done", id: job.id });
        log(`job ${job.id} finished`);
      } catch (error) {
        if (controller.signal.aborted) {
          post({ type: "fail", id: job.id, error: "cancelled" });
          log(`job ${job.id} was cancelled`);
        } else {
          log(`job ${job.id} failed: ${messageOf(error)}`);
          post({ type: "fail", id: job.id, error: messageOf(error) });
        }
      } finally {
        if (current === run) current = null;
      }
    }

    // The popup asks the active tab for a self-check.
    chrome.runtime.onMessage.addListener((message, sender, sendResponse) => {
      if (!message || message.type !== "selfcheck" || typeof selfcheck !== "function") return false;
      Promise.resolve()
        .then(() => selfcheck())
        .then(
          (report) => sendResponse({ ok: true, site, report }),
          (error) => sendResponse({ ok: false, site, error: messageOf(error) }),
        );
      return true; // the answer is sent later
    });

    connect();
  }

  globalThis.ConcatBridge = {
    MAX_INLINE_BYTES,
    CancelledError,
    TooLargeError,
    log,
    messageOf,
    throwIfAborted,
    sleep,
    waitFor,
    textOf,
    isVisible,
    isEnabled,
    setNativeValue,
    insertIntoContentEditable,
    setPrompt,
    pressKey,
    clickLikeUser,
    labelOf,
    findButtonByText,
    srcOf,
    sizeOf,
    mediaSnapshot,
    blobToBase64,
    fetchAsBase64,
    deliverFiles,
    connectSite,
  };
})();
