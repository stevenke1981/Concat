# Concat Bridge

A Chrome extension (Manifest V3, plain JavaScript, no build step, no dependencies)
that lets the Concat desktop editor make pictures and videos with **Google Flow**
and pictures with **ChatGPT on the web**, using your own signed-in browser session.

Concat has no API for either site. Instead, this extension drives the page you
already have open, the way you would: it types the prompt, presses the button,
waits for the result, and downloads it. The files go to Concat's asset library.

## What it does

- Concat sends a generation job to a server on `127.0.0.1` (default port 47615).
- A background service worker long-polls that server for jobs, one loop per site.
- A job goes to a content script in a Flow project tab or a chatgpt.com tab. The
  script runs the page, reports progress, and posts each file back to Concat.
- A job can be cancelled from Concat; the tab is told to stop.
- Cancelling a job stops its active downloads and uploads. Closing a tab stops
  its transfers and reports the job as failed.

## Install

1. Open `chrome://extensions`.
2. Turn on **Developer mode** (top right).
3. Click **Load unpacked** and choose this folder (`extensions/concat-bridge`).

## Pairing

1. In Concat, open **Settings, AI & cloud, Browser bridge** and turn it on.
2. Copy the token Concat shows.
3. Click the Concat Bridge toolbar icon. Paste the token into **Token**, check the
   **Port** (47615 unless you changed it in Concat), and click **Save**.
4. The popup shows **Connected, Concat x.y.z** when Concat answers.

## Usage

1. Keep a signed-in **Flow project** open in a tab, for example
   `https://labs.google/fx/tools/flow/project/<id>`, and/or a **chatgpt.com** tab.
2. In Concat, go to **Library, AI** and generate. Jobs go to the tab of the
   matching site. The tab must stay open until the job finishes.
3. The popup shows how many tabs each site has and whether one is busy.

Flow takes prompts only inside a project. If the mode picker cannot be switched,
the job reports "switch the mode to ... by hand" and continues.

## When a page changes

Pages change often. Every site-specific selector lives in one `SELECTORS` object
at the top of its site file (`sites/flow.js` or `sites/chatgpt.js`). Each entry is
an ordered list of fallbacks, so a broken step is usually a one-line fix:

1. Open the site, open the page's DevTools, and find the element that no longer matches.
2. Add a selector for it to the top of the matching list in `SELECTORS`.
3. Open `chrome://extensions` and click the reload arrow on Concat Bridge.
4. Reload the site tab. Click **Check this page** in the popup to see what the
   page currently has.

## Privacy

The extension talks only to `127.0.0.1` (Concat, on the port you set) and to the
sites it runs on: `labs.google`, `chatgpt.com`, and the Google and OpenAI hosts
where their generated files are served. It stores only the port and the pairing
token, in the browser's extension storage. It does not send anything anywhere else.
Your prompts and the generated files go to the site you use and to Concat, as they
would if you did it by hand.

## Caveats

- Google Flow and ChatGPT change their pages without notice. The selectors were
  written without the live page at hand and are **not verified against it**. The
  popup's **Check this page** button reports what the page currently has. Expect
  to adjust selectors after the first real use.
- Automating a site you use may be against its terms of service. You are
  responsible for using it in line with those terms and your plan's limits.
- The reference picture of a job is not attached in this version; the job is made
  from the prompt alone and the progress message says so.
- Only one job runs per tab at a time. A job whose tab is closed fails with
  "the tab was closed".

## Files

| File | Role |
|---|---|
| `manifest.json` | MV3 manifest: permissions, content scripts, popup, icons |
| `background.js` | Service worker: all HTTP to Concat, job routing, cancel, status |
| `lib/protocol.js` | Pure protocol logic: settings, requests, client, reducers |
| `sites/common.js` | Helpers shared by the site scripts, and the port wiring |
| `sites/flow.js` | Google Flow: selectors, prompt, mode, wait for results |
| `sites/chatgpt.js` | ChatGPT web: selectors, prompt, wait for the reply's pictures |
| `popup.html`, `popup.js`, `popup.css` | Pairing, status, per-site tabs, self-check |
| `icons/` | Icons; regenerate with `node tools/make-icons.mjs` |
| `tools/make-icons.mjs` | Writes the three PNG icons with Node's built-in zlib |
| `test/` | Tests (see below) |

## Tests

No dependencies are needed. From this folder:

```
npm test          # node --test "test/*.test.mjs"
npm run check     # node --check on every script
```

The tests cover the protocol (including a mock of Concat's bridge over HTTP), the
service worker driven through a stubbed `chrome` API, and the content scripts
loaded into a `vm` context with a stub page. The worker tests include cancellation
during stalled downloads and uploads, and closing a tab during a download.
They do **not** exercise the live Flow or ChatGPT pages; that can only be checked
in the browser.

## Protocol

The wire protocol is the one in `src/crates/concat-host/src/bridge.rs` (protocol 1):
`GET /v1/hello`, `GET /v1/jobs/next?site=&wait=`, `POST /v1/jobs/<id>/progress|file|done|fail`,
and `GET /v1/jobs/<id>`. Every route but `hello` needs the `X-Concat-Token` header.
