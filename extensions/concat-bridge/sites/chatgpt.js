// Concat Bridge: ChatGPT on the web (https://chatgpt.com).
//
// A picture job is typed into the chat box as "Create an image: <prompt>", sent,
// and the reply's pictures are downloaded with the signed-in session. ChatGPT
// has no API for this in the browser, so the page is driven the way a person
// drives it. Every site-specific selector is in SELECTORS below, each an ordered
// list of fallbacks.
//
// Not verified against the live page when this was written. The selfcheck from
// the popup reports which of these elements the page currently has.
(() => {
  "use strict";
  const CB = globalThis.ConcatBridge;
  const SITE = "chatgpt-web";

  const SELECTORS = {
    // The message box: a ProseMirror contenteditable in current layouts, a textarea in older ones.
    promptBox: [
      "#prompt-textarea",
      '[contenteditable="true"][data-virtualkeyboard]',
      "form div[contenteditable='true']",
    ],
    sendButton: ['button[data-testid="send-button"]', "#composer-submit-button", 'button[aria-label*="Send" i]'],
    // Shown while a reply is being written; the send button takes its place.
    stopButton: ['button[data-testid="stop-button"]', 'button[aria-label*="Stop" i]'],
    assistantTurn: ['[data-message-author-role="assistant"]'],
    // Pictures in a reply. Named ones count at any size; the others must be large,
    // because ChatGPT also shows small icons from the same hosts.
    generatedImage: [
      'img[alt*="Generated image" i]',
      'img[src*="/backend-api/estuary/content"]',
      'img[src*="oaiusercontent"]',
    ],
    // The reply's text, inside the turn; falls back to the whole turn.
    replyText: [".markdown"],
  };

  const WAIT_MS = 5 * 60_000;

  function findPromptBox() {
    for (const selector of SELECTORS.promptBox) {
      const el = [...document.querySelectorAll(selector)].find(CB.isVisible);
      if (el) return el;
    }
    return null;
  }

  /** The form the message box sits in; the send button is looked for there first. */
  function composerOf(box) {
    return (box && box.closest("form")) || document;
  }

  function findSend(box) {
    const scope = composerOf(box);
    for (const selector of SELECTORS.sendButton) {
      const el = [...scope.querySelectorAll(selector)].find(CB.isVisible);
      if (el) return el;
    }
    return null;
  }

  function isDrawing() {
    const scope = composerOf(findPromptBox());
    return SELECTORS.stopButton.some((selector) => [...scope.querySelectorAll(selector)].some(CB.isVisible));
  }

  function lastAssistantTurn() {
    const turns = [...document.querySelectorAll(SELECTORS.assistantTurn.join(","))];
    return turns[turns.length - 1] ?? null;
  }

  function generatedImagesIn(turn) {
    const found = [];
    for (const selector of SELECTORS.generatedImage) {
      for (const img of turn.querySelectorAll(selector)) {
        const src = CB.srcOf(img);
        if (!src || src.startsWith("data:") || found.includes(src)) continue;
        const named = /generated image/i.test(img.alt || "");
        if (!named && !(img.naturalWidth >= 256)) continue;
        found.push(src);
      }
    }
    return found;
  }

  function replyTextOf(turn) {
    for (const selector of SELECTORS.replyText) {
      const el = turn.querySelector(selector);
      if (el) return CB.textOf(el);
    }
    return CB.textOf(turn);
  }

  /**
   * The text the page is asked to send. The wording is what the image request
   * is recognised by, so it is kept exactly.
   */
  function buildPrompt(job, count) {
    let text = `Create an image: ${job.prompt}`;
    if (job.aspectRatio) text += ` Aspect ratio ${job.aspectRatio}.`;
    if (count > 1) text += ` Make ${count} images.`;
    return text;
  }

  async function sendMessage(box, ctx) {
    const button = await CB.waitFor(
      () => {
        const candidate = findSend(box);
        return candidate && CB.isEnabled(candidate) ? candidate : null;
      },
      { timeout: 5000, interval: 200, signal: ctx.signal, label: "the send button" },
    ).catch(() => {
      CB.throwIfAborted(ctx.signal);
      return null;
    });
    if (button) {
      CB.clickLikeUser(button);
      return;
    }
    box.focus();
    CB.pressKey(box, "Enter");
  }

  /**
   * Waits for the reply to finish: it must be a new turn, no stop button shown,
   * and the reply's pictures and text must have stayed the same for three
   * seconds. ChatGPT swaps blurry previews for the final file, so the three
   * seconds keep the preview from being taken as the result.
   */
  async function waitForReply(previous, ctx) {
    const deadline = Date.now() + WAIT_MS;
    let signature = "";
    let stableSince = Date.now();
    let lastNote = 0;
    for (;;) {
      CB.throwIfAborted(ctx.signal);
      const turn = lastAssistantTurn();
      const drawing = isDrawing();
      if (turn && turn !== previous) {
        const images = generatedImagesIn(turn);
        const text = replyTextOf(turn);
        const sig = `${images.join("\n")}\u0000${text}`;
        if (sig !== signature) {
          signature = sig;
          stableSince = Date.now();
        }
        if (!drawing && Date.now() - stableSince >= 3000 && (images.length || text)) {
          return { images, text };
        }
      }
      if (drawing && Date.now() - lastNote >= 15000) {
        lastNote = Date.now();
        ctx.progress("ChatGPT is drawing…", null);
      }
      if (Date.now() > deadline) throw new Error("ChatGPT did not finish within 5 minutes");
      await CB.sleep(1000, ctx.signal);
    }
  }

  async function runJob(job, ctx) {
    if (job.kind !== "image") throw new Error("ChatGPT on the web makes pictures only");
    const count = Math.max(1, Number(job.count) || 1);
    const box = findPromptBox();
    if (!box) throw new Error("ChatGPT's message box was not found: keep a chatgpt.com chat open in this tab");
    if (job.reference) {
      ctx.progress("ChatGPT: the reference picture was not attached (not supported in this version); making from the prompt", null);
    }
    // The reply is recognised as new by its turn element, so the last one is noted first.
    const previous = lastAssistantTurn();
    CB.setPrompt(box, buildPrompt(job, count));
    await sendMessage(box, ctx);
    ctx.progress("ChatGPT is drawing…", null);
    const reply = await waitForReply(previous, ctx);
    if (reply.images.length === 0) {
      const said = reply.text ? `: ${reply.text.slice(0, 200)}` : "";
      throw new Error(`ChatGPT answered without a picture${said}`);
    }
    await CB.deliverFiles(reply.images, ctx, { label: "ChatGPT" });
  }

  /** What the popup's "Check this page" shows: which elements this page has. */
  function selfcheck() {
    const box = findPromptBox();
    const send = box ? findSend(box) : null;
    const turn = lastAssistantTurn();
    return {
      promptBox: box ? `found (${box.tagName.toLowerCase()})` : "not found",
      sendButton: send ? (CB.isEnabled(send) ? "found, enabled" : "found, disabled") : "not found",
      replyInProgress: isDrawing() ? "yes" : "no",
      assistantReplies: document.querySelectorAll(SELECTORS.assistantTurn.join(",")).length,
      picturesInLastReply: turn ? generatedImagesIn(turn).length : 0,
    };
  }

  CB.connectSite(SITE, runJob, selfcheck);
})();
