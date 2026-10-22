// Concat Bridge: Google Flow (https://labs.google/fx/tools/flow/project/<id>).
//
// Flow has no public API for this, so a job is done the way a person would do
// it: set the prompt, press the button, wait for the new pictures or videos to
// finish, and download them with the signed-in session. The page is a React app
// that changes often, which is why every site-specific selector is in SELECTORS
// below. Each entry is an ordered list of fallbacks; when a step breaks, the fix
// is usually one line there.
//
// Not verified against the live page when this was written. The selfcheck from
// the popup reports which of these elements the page currently has.
(() => {
  "use strict";
  const CB = globalThis.ConcatBridge;
  const SITE = "flow";

  const SELECTORS = {
    // The prompt box: Flow's own id first, then wording in placeholder or label.
    // When none matches, the largest visible textarea is taken, then any contenteditable.
    promptBox: [
      "textarea#PINHOLE_TEXT_AREA_ELEMENT_ID",
      'textarea[placeholder*="prompt" i]',
      'textarea[aria-label*="prompt" i]',
      'textarea[placeholder*="describe" i]',
      'textarea[placeholder*="generate" i]',
    ],
    promptFallback: ['[contenteditable="true"][role="textbox"]', '[contenteditable="true"]'],
    // Labels that name a mode on the mode picker.
    modeNames: [/text to video/i, /frames to video/i, /ingredients to video/i, /create image/i],
    // The mode wanted for each kind of job, in order of preference.
    imageMode: [/create image/i, /\bimage\b/i],
    videoMode: [/text to video/i],
    // The settings panel (aspect ratio and the like).
    settings: [/^tune$/i, /\bsettings\b/i],
    // The submit button. Searched inside the prompt box's form or container first.
    submit: [/arrow_forward/i, /^(send|create|generate)$/i, /\b(create|generate)\b/i],
    // Where results appear.
    videoSrc: ["video[src]", "video source[src]"],
    imageSrc: ["img"],
    // A result still being made shows a bar or a percentage in its card.
    progressBar: ['[role="progressbar"]'],
    // Flow takes prompts only inside a project.
    projectPath: /\/project\//,
  };

  // How long a job may take before it is failed.
  const WAIT_MS = { video: 10 * 60_000, image: 4 * 60_000 };
  // Smaller pictures are thumbnails or icons, not results.
  const MIN_RESULT_PX = 256;

  const escapeRegExp = (text) => text.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");

  function firstVisible(selector) {
    return [...document.querySelectorAll(selector)].find(CB.isVisible) ?? null;
  }

  function area(el) {
    const rect = el.getBoundingClientRect();
    return rect.width * rect.height;
  }

  function findPromptBox() {
    for (const selector of SELECTORS.promptBox) {
      const el = firstVisible(selector);
      if (el) return el;
    }
    // Flow changes its markup often, so the largest visible textarea is the next guess.
    const textareas = [...document.querySelectorAll("textarea")].filter(CB.isVisible);
    if (textareas.length) return textareas.sort((a, b) => area(b) - area(a))[0];
    for (const selector of SELECTORS.promptFallback) {
      const el = firstVisible(selector);
      if (el) return el;
    }
    return null;
  }

  /** The element that holds the prompt box and its submit button. */
  function scopeOf(box) {
    return box.closest("form") || box.closest('[class*="prompt" i]') || box.parentElement?.parentElement || document;
  }

  function findSubmit(box, enabledOnly) {
    return CB.findButtonByText(SELECTORS.submit, scopeOf(box), { enabledOnly });
  }

  function currentModeButton() {
    return CB.findButtonByText(SELECTORS.modeNames);
  }

  function modeMatches(button, patterns) {
    return Boolean(button) && patterns.some((pattern) => pattern.test(CB.labelOf(button)));
  }

  /**
   * Switches the mode picker to the mode the job needs. Best effort: when the
   * picker cannot be found or switched, the person is told to do it by hand and
   * the job goes on.
   */
  async function chooseMode(kind, ctx) {
    const wanted = kind === "video" ? SELECTORS.videoMode : SELECTORS.imageMode;
    const wantedName = kind === "video" ? "Text to Video" : "Create Image";
    const trigger = currentModeButton();
    if (modeMatches(trigger, wanted)) return true;
    const byHand = () => {
      ctx.progress(`Flow: switch the mode to ${wantedName} by hand`, null);
      return false;
    };
    if (!trigger) return byHand();
    CB.clickLikeUser(trigger);
    const option = await CB.waitFor(() => CB.findButtonByText(wanted, document, { enabledOnly: true }), {
      timeout: 3000,
      interval: 200,
      signal: ctx.signal,
      label: "the mode list",
    }).catch(() => {
      CB.throwIfAborted(ctx.signal);
      return null;
    });
    if (!option) {
      CB.pressKey(document.activeElement || document.body, "Escape");
      return byHand();
    }
    CB.clickLikeUser(option);
    const switched = await CB.waitFor(() => modeMatches(currentModeButton(), wanted), {
      timeout: 3000,
      interval: 200,
      signal: ctx.signal,
      label: "the mode to change",
    }).catch(() => {
      CB.throwIfAborted(ctx.signal);
      return false;
    });
    return switched ? true : byHand();
  }

  /**
   * Sets what can be set easily (the aspect ratio). Everything else stays as the
   * page has it, and the person is told so. Never fails the job.
   */
  async function applySettings(job, ctx) {
    const notes = [];
    try {
      if (job.aspectRatio) {
        const panel = CB.findButtonByText(SELECTORS.settings);
        if (panel) {
          CB.clickLikeUser(panel);
          await CB.sleep(400, ctx.signal);
          const ratio = new RegExp(`^\\s*${escapeRegExp(job.aspectRatio)}\\s*$`);
          const option = CB.findButtonByText([ratio]);
          if (option) {
            CB.clickLikeUser(option);
            await CB.sleep(300, ctx.signal);
          } else {
            notes.push(`aspect ratio ${job.aspectRatio} was not found on the page`);
          }
          CB.pressKey(document.activeElement || document.body, "Escape");
        } else {
          notes.push("the settings panel was not found");
        }
      }
    } catch (error) {
      CB.throwIfAborted(ctx.signal);
      notes.push(`settings could not be set (${CB.messageOf(error)})`);
    }
    if (job.model || job.resolution || job.duration) {
      notes.push("model, resolution and duration are not set by Concat in this version");
    }
    if (notes.length) {
      ctx.progress(`Flow: ${notes.join("; ")}; the page's own settings were used`, null);
    }
  }

  function isResultImage(el, src) {
    if (src.startsWith("data:") || /\.svg(\?|$)/i.test(src)) return false;
    if (/avatar/i.test(`${src} ${el.alt || ""}`)) return false;
    return el.complete && el.naturalWidth >= MIN_RESULT_PX;
  }

  /** The element's card: a few levels up, enough to hold its progress bar. */
  function cardOf(el) {
    const base = el.tagName === "SOURCE" ? el.parentElement : el;
    return base?.parentElement?.parentElement ?? base ?? el;
  }

  /** The results that were not on the page before the job was submitted. */
  function resultsSince(kind, before) {
    const found = [];
    const seen = new Set();
    const selectors = kind === "video" ? SELECTORS.videoSrc : SELECTORS.imageSrc;
    for (const selector of selectors) {
      for (const el of document.querySelectorAll(selector)) {
        const src = CB.srcOf(el);
        if (!src || seen.has(src) || before.has(src)) continue;
        if (kind === "image" && !isResultImage(el, src)) continue;
        seen.add(src);
        found.push({ src, card: cardOf(el) });
      }
    }
    return found;
  }

  /** A percentage the page shows on its own line, as in "42%", inside `root`. */
  function percentIn(root) {
    const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
    for (let node = walker.nextNode(); node; node = walker.nextNode()) {
      const match = /^\s*(\d{1,3})\s*%\s*$/.exec(node.nodeValue || "");
      if (match && Number(match[1]) <= 100 && CB.isVisible(node.parentElement)) {
        return Number(match[1]);
      }
    }
    return null;
  }

  function inProgress(card) {
    return Boolean(card.querySelector(SELECTORS.progressBar.join(","))) || percentIn(card) !== null;
  }

  /**
   * Submits the prompt. Presses the submit button once it is enabled; when none
   * is found, Enter is what the prompt box itself responds to.
   */
  async function submit(box, ctx) {
    const button = await CB.waitFor(() => findSubmit(box, true), {
      timeout: 5000,
      interval: 200,
      signal: ctx.signal,
      label: "the submit button",
    }).catch(() => {
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
   * Waits for the new results to finish. A result counts as finished when it is
   * on the page, no bar or percentage shows in its card, and the set has stayed
   * the same for three seconds. When fewer results than asked for have appeared,
   * the set must stay the same for twenty seconds before it is taken as final.
   */
  async function waitForResults(kind, before, count, ctx) {
    const timeout = WAIT_MS[kind];
    const deadline = Date.now() + timeout;
    let shownPercent = -1;
    let signature = "";
    let stableSince = 0;
    for (;;) {
      CB.throwIfAborted(ctx.signal);
      const found = resultsSince(kind, before);
      const percent = percentIn(document.querySelector("main") || document.body);
      if (percent !== null && percent !== shownPercent) {
        shownPercent = percent;
        ctx.progress(`Flow: ${percent}%`, percent / 100);
      }
      const busy = found.some((item) => inProgress(item.card));
      if (found.length && !busy) {
        const sig = found.map((item) => item.src).join("\n");
        if (sig !== signature) {
          signature = sig;
          stableSince = Date.now();
        }
        const stable = Date.now() - stableSince;
        if (stable >= 3000 && (found.length >= count || stable >= 20000)) return found;
      } else {
        signature = "";
      }
      if (Date.now() > deadline) {
        throw new Error(`Flow did not finish within ${Math.round(timeout / 60000)} minutes`);
      }
      await CB.sleep(1000, ctx.signal);
    }
  }

  async function runJob(job, ctx) {
    const kind = job.kind === "video" ? "video" : "image";
    const count = Math.max(1, Number(job.count) || 1);
    if (job.reference) {
      ctx.progress("Flow: the reference picture was not attached (not supported in this version); making from the prompt", null);
    }
    const box = findPromptBox();
    if (!box) throw new Error("Flow's prompt box was not found: open a Flow project in this tab");
    await chooseMode(kind, ctx);
    await applySettings(job, ctx);
    CB.setPrompt(box, job.prompt);
    // Taken before submitting, so only media that appear afterwards are results.
    const before = CB.mediaSnapshot(document);
    ctx.progress(`Flow: submitting the ${kind}`, null);
    await submit(box, ctx);
    const found = await waitForResults(kind, before, count, ctx);
    await CB.deliverFiles(
      found.slice(0, count).map((item) => item.src),
      ctx,
      { label: "Flow" },
    );
  }

  /** What the popup's "Check this page" shows: which elements this page has. */
  function selfcheck() {
    const box = findPromptBox();
    const mode = currentModeButton();
    const panel = CB.findButtonByText(SELECTORS.settings);
    const submitButton = box ? findSubmit(box, false) : null;
    return {
      projectOpen: SELECTORS.projectPath.test(location.pathname) ? "yes" : "no (open a project first)",
      promptBox: box ? `found (${box.tagName.toLowerCase()})` : "not found",
      submitButton: submitButton
        ? CB.isEnabled(submitButton) ? "found, enabled" : "found, disabled"
        : "not found",
      modePicker: mode ? `found ("${CB.labelOf(mode).slice(0, 40)}")` : "not found",
      settingsButton: panel ? "found" : "not found",
      resultMedia: resultsSince("image", new Set()).length + resultsSince("video", new Set()).length,
      progressIndicators: document.querySelectorAll(SELECTORS.progressBar.join(",")).length,
    };
  }

  CB.connectSite(SITE, runJob, selfcheck);
})();
