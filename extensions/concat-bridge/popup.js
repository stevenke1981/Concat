// Concat Bridge popup: pairing settings, the status Concat reports, and a self-check
// of the active tab. The popup never talks to Concat itself; it asks the background
// worker, which owns every connection to Concat.
import { DEFAULT_PORT, normalizeSettings, parsePort } from "./lib/protocol.js";

const $ = (id) => document.getElementById(id);

const CONCAT_TEXT = {
  ok: "Connected",
  offline: "Concat is not running",
  unpaired: "Not paired",
};
const CONCAT_HINT = {
  ok: "Concat answers on this port.",
  offline:
    "Start Concat, and check the port. In Concat: Settings, AI and cloud, Browser bridge.",
  unpaired:
    "Copy the token from Concat (Settings, AI and cloud, Browser bridge), paste it below, and Save.",
};
const SITE_TEXT = {
  tabs0: "no tab open",
  idle: (n) => `${n} tab${n === 1 ? "" : "s"}, idle`,
  busy: (n) => `${n} tab${n === 1 ? "" : "s"}, busy`,
};

function sendToBackground(message) {
  return chrome.runtime.sendMessage(message).catch((error) => ({ error: String(error?.message ?? error) }));
}

async function loadSettings() {
  const stored = await chrome.storage.local.get({ port: DEFAULT_PORT, token: "" });
  $("port").value = String(stored.port ?? DEFAULT_PORT);
  $("token").value = stored.token ?? "";
}

async function save(event) {
  event.preventDefault();
  const port = parsePort($("port").value);
  if (port === null) {
    $("save-note").textContent = "The port must be a number from 1 to 65535.";
    return;
  }
  const settings = normalizeSettings({ port, token: $("token").value });
  await chrome.storage.local.set(settings);
  $("save-note").textContent = "Saved. Checking Concat…";
  await refresh();
  setTimeout(() => {
    if ($("save-note").textContent.startsWith("Saved")) $("save-note").textContent = "";
  }, 2500);
}

function renderStatus(status) {
  if (status?.error) {
    $("concat").textContent = "Could not read the status";
    $("concat-hint").textContent = status.error;
    return;
  }
  const state = status.concat in CONCAT_TEXT ? status.concat : "offline";
  $("concat").textContent =
    state === "ok" && status.version ? `${CONCAT_TEXT.ok}, Concat ${status.version}` : CONCAT_TEXT[state];
  $("concat").dataset.state = state;
  $("concat-hint").textContent = CONCAT_HINT[state];

  for (const site of ["flow", "chatgpt-web"]) {
    const row = status.sites?.[site] ?? { tabs: 0, busy: false };
    $(site).textContent =
      row.tabs === 0 ? SITE_TEXT.tabs0 : row.busy ? SITE_TEXT.busy(row.tabs) : SITE_TEXT.idle(row.tabs);
  }
  $("last-error").textContent = status.lastError || "None";
  $("last-error").classList.toggle("has-error", Boolean(status.lastError));
  if (status.version) $("version").textContent = `v${status.version}`;
}

async function refresh() {
  renderStatus(await sendToBackground({ type: "status" }));
}

// Each row of the self-check is shown as a term and its value.
function renderCheck(report) {
  const out = $("check-out");
  out.replaceChildren();
  for (const [key, value] of Object.entries(report)) {
    const label = key.replace(/([A-Z])/g, " $1").replace(/^./, (c) => c.toUpperCase());
    const term = document.createElement("dt");
    term.textContent = label;
    const detail = document.createElement("dd");
    detail.textContent = String(value);
    out.append(term, detail);
  }
  out.hidden = false;
}

// The self-check's own messages go in the same box as its results, so the
// two-second status refresh does not overwrite them.
async function check() {
  $("check").disabled = true;
  try {
    const [tab] = await chrome.tabs.query({ active: true, lastFocusedWindow: true });
    if (!tab?.id) {
      renderCheck({ Result: "No active tab to check." });
      return;
    }
    const reply = await chrome.tabs.sendMessage(tab.id, { type: "selfcheck" });
    if (!reply) {
      renderCheck({ Result: "This page did not answer." });
      return;
    }
    if (!reply.ok) {
      renderCheck({ Result: `The page's script failed: ${reply.error}` });
      return;
    }
    renderCheck(reply.report);
  } catch {
    renderCheck({
      Result:
        "This tab has no Concat Bridge script. Open a Flow project or chatgpt.com in the active tab, then reload it.",
    });
  } finally {
    $("check").disabled = false;
  }
}

$("pair-form").addEventListener("submit", save);
$("toggle-token").addEventListener("click", () => {
  const shown = $("token").type === "text";
  $("token").type = shown ? "password" : "text";
  $("toggle-token").textContent = shown ? "Show" : "Hide";
});
$("check").addEventListener("click", check);

await loadSettings();
await refresh();
// The status changes while the popup is open (a job starts, Concat starts), so it is re-read.
setInterval(refresh, 2000);
