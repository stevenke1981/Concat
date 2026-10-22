# AI generation

The Library's **AI** page makes pictures and videos from a text prompt. Every
result goes into the **Assets** shelf, the same place your own imported
pictures and videos go. This page covers the four providers, how to connect
each one, how to keep the shelf in your own Google Drive, and what to do when
something fails.

## 1. Overview

On the **AI** page you pick a provider, choose a picture or a video, pick a
model, set the aspect ratio (and the length in seconds, for a video), write
the prompt and press **Generate**. Running jobs are listed on the page, and
**Cancel** stops one.

When a job finishes, its files appear in the **Assets** shelf. From there you
can:

- preview the asset;
- add it at the playhead with **+**;
- drag it onto the timeline.

The shelf is Concat's asset library. It is kept across projects, in the app
data folder under `assets/`, with an index file `assets.json` beside the
copies. A project does not point into the library. When you add an asset, its
file is first copied into the project's own folder, the same way your imported
media is handled.

The shelf also has **Import** (to bring in your own files), **Rename**,
**Remove**, and **Sync with Google Drive** (section 5).

## 2. Providers

| Provider | Makes | Who pays | What you need |
|---|---|---|---|
| OpenRouter | pictures and videos | pay per use, with your own OpenRouter key | a key (section 3) |
| ChatGPT (Codex CLI) | pictures | your ChatGPT Plus or Pro plan quota | Node.js and Codex CLI (section 4) |
| ChatGPT web | pictures | your ChatGPT account, in your own browser | the bridge and the extension (section 6) |
| Google Flow | pictures and videos | your Flow plan and credits, in your own browser | the bridge and the extension (section 6) |

The OpenRouter defaults are `google/gemini-3.1-flash-image` for pictures and
`google/veo-3.1-fast` for videos. The model list is fetched live from
OpenRouter, so newer models appear without an update to Concat.

ChatGPT and ChatGPT web make pictures only. Flow and OpenRouter make both.

## 3. Setting up OpenRouter

1. Create an account at <https://openrouter.ai> and make a key at
   <https://openrouter.ai/keys>.
2. Add credits to the account first. Generation is pay-per-use.
3. In Concat, open **Settings → AI & cloud**. Paste the key into the OpenRouter
   key field. The default picture and video model fields can stay as they are,
   or be set to other models from the live list.

The key is stored in Concat's `settings.json`, in the config folder (on Windows
that is under `%APPDATA%`). It is plain text on your disk and is not encrypted.
Concat sends it only to OpenRouter's own API, never to any other host. Anyone
who can read that file can read the key, so do not share the file. If the key
leaks, revoke it at <https://openrouter.ai/keys> and make a new one.

## 4. Setting up ChatGPT (subscription)

This route uses OpenAI's own command line, Codex CLI, signed in with your
ChatGPT account. Concat never sees your OpenAI credentials: Codex keeps them.

1. Install Node.js (the current LTS release).
2. In a terminal, install the CLI:

   ```text
   npm i -g @openai/codex
   ```

3. Run `codex login` and choose **Sign in with ChatGPT**. Finish the sign-in in
   the browser.
4. In Concat, open **Settings → AI & cloud**. The ChatGPT row shows the status.
   After you change anything, press **Check again**.

The status reads one of these:

| Status | Meaning |
|---|---|
| Missing | The Codex CLI was not found, or it did not run. |
| Not logged in | Codex is installed. Run `codex login`. |
| Logged in with an API key | Pictures work, but they are billed to the OpenAI API, not your plan. |
| Ready | Signed in with ChatGPT. Pictures count against your subscription. |

Each picture is one `codex exec` run. Concat sends the instruction on stdin
and asks Codex's `$imagegen` skill for the picture. Concat then takes the new
picture out of the run's scratch folder and removes that folder.

This route depends on the way Codex CLI works today. If OpenAI changes the
CLI or its image skill, pictures may stop until Concat is updated.

## 5. Google Drive sync

Drive sync keeps the Assets shelf in a folder in your own Google Drive, so a
second computer, or a fresh install, can get the same pictures and videos.
Concat ships no Google client. You make your own OAuth client, once.

1. Open the Google Cloud Console at <https://console.cloud.google.com> and
   create a new project.
2. Enable the **Google Drive API** for that project.
3. Open the **OAuth consent screen**. Choose **External**, fill in the name and
   your email, and add your own Google account under **Test users**.
4. Open **Credentials**, choose **Create credentials → OAuth client ID**, and
   set the application type to **Desktop app**.
5. Copy the **client ID** and the **client secret**.
6. In Concat, open **Settings → AI & cloud**. Paste them into the Google Drive
   client ID and client secret fields, then press **Connect**.
7. Your browser opens Google's consent page. Approve it. Concat receives the
   answer on a port on your own computer and then continues.
8. Back in Concat, your account address appears under the connection.

Then press **Sync with Google Drive** on the Assets shelf.

What the sync does:

- **Scope.** Concat asks for `drive.file`. Google then shows Concat only the
  files it created. Nothing else in your Drive is readable by it.
- **Folder.** Concat makes a folder named **Concat Library** in your Drive.
- **Two-way.** Each local asset that the folder does not hold is uploaded.
  Each Concat file in the folder that the library does not hold is downloaded.
- **Metadata.** Every uploaded file carries its library details (name, prompt,
  provider, model and SHA-256) as Drive `appProperties`. A new install can
  rebuild the shelf from the folder alone.
- **Checks.** A downloaded file is checked against its SHA-256 before it joins
  the library. A file that does not match is refused and listed in the sync
  report, with its name. The rest of the sync goes on.
- **Auto-sync.** The auto-sync switch in Settings runs a sync after each
  generation.

**Disconnect** deletes the saved sign-in (`google-drive.json` in the config
folder). It does not tell Google. To cut Concat's access completely, open
<https://myaccount.google.com/permissions> and remove Concat there. Files
already in the **Concat Library** folder stay in your Drive.

## 6. The browser bridge (Google Flow and ChatGPT web)

Google Flow and ChatGPT's web page have no API. The bridge lets Concat use
them through your own browser, where you are already signed in. A small Chrome
extension, **Concat Bridge**, takes each job from Concat, drives the page, and
sends the finished files back.

How it works:

- Concat listens on `127.0.0.1:47615`. Only programs on your own computer can
  reach it.
- The extension asks Concat for jobs. When one arrives, the extension works the
  page you have open, saves each picture or video it makes, and posts it back.
- The files go into the Assets shelf. A job waits in the queue until the
  extension takes it.

Install the extension:

1. In Chrome, open `chrome://extensions`.
2. Turn on **Developer mode**.
3. Click **Load unpacked** and choose the `extensions/concat-bridge` folder in
   the Concat source tree.

Pair it with Concat:

1. In Concat, open **Settings → AI & cloud** and turn on the **Browser bridge**
   switch.
2. Copy the pairing token with its **Copy** button. Note the port (47615 unless
   you changed it).
3. Click the Concat Bridge icon in Chrome. In its popup, paste the token and
   the port.

Keep the pages open and signed in:

- Google Flow: open <https://labs.google/fx/tools/flow> and open a project.
- ChatGPT web: open <https://chatgpt.com>.

The connection dots in Settings show each site's state. A dot lights when the
extension has checked in for that site recently. If a dot stays dark, check
that the extension is loaded and paired, and that the site's tab is open.

Security:

- The bridge listens on `127.0.0.1` only. It is not reachable from your
  network.
- Every request except the version check needs the pairing token. A wrong or
  missing token gets `401`. The server answers any origin, because the token,
  not the origin, is what keeps a web page out.
- If you think the token has leaked, press **New token** in Settings. The old
  token stops working at once, so paste the new one into the extension popup.

Please note:

- The bridge drives the layout of Flow and chatgpt.com. When those sites
  change, automation can break until the extension is updated.
- Automating a website may be subject to that website's terms of use. Use the
  bridge on your own account, and at your own discretion.

## 7. The bridge protocol (for developers)

HTTP/1.1 on `127.0.0.1:<port>`, JSON bodies in UTF-8. Every route except
`GET /v1/hello` and `OPTIONS` needs the header `X-Concat-Token: <token>`.

| Route | Purpose | Answer |
|---|---|---|
| `GET /v1/hello` | Version check (no token) | `200 {"app","version","protocol":1}` |
| `GET /v1/jobs/next?site=flow\|chatgpt-web&wait=<secs ≤25>` | Long-poll for the oldest queued job of that site; marks the site as connected | `200` job JSON, or `204` if none came in time |
| `POST /v1/jobs/<id>/progress` | Report `{"message","fraction"}` (fraction `0..1` or `null`) | `204` |
| `POST /v1/jobs/<id>/file` | Send one file as raw bytes; headers `Content-Type`, optional `X-File-Name` and `X-Index` | `200 {"saved":"<name>"}`; `413` above 1 GiB |
| `POST /v1/jobs/<id>/done` | The job is finished; its files were already posted | `204` |
| `POST /v1/jobs/<id>/fail` | Body `{"error":"..."}` | `204` |
| `GET /v1/jobs/<id>` | Job state: `queued`, `taken`, `done`, `failed` or `cancelled` | `200 {"state":"..."}` |

Other answers: a wrong or missing token gets `401 {"error":"unauthorized"}`.
An unknown job gets `404 {"error":"no such job"}`, and an unknown route gets
`404`. Every answer carries the CORS headers, and `OPTIONS` gets `204`.

The job JSON has `id`, `site`, `kind` (`image` or `video`), `prompt`, `model`,
`aspectRatio`, `resolution`, `duration`, `count`, and `reference` (`null`, or
`{"name","mediaType","base64"}` for a reference image).

## 8. Troubleshooting

**"port 47615 is in use".** Another program holds the port, often a second
Concat window. Close it. Or change the port in Settings and enter the same port
in the extension popup.

**The extension says "not paired".** The token or the port in the popup does
not match Concat. Copy the token again from **Settings → AI & cloud** and paste
it into the popup. A regenerated token replaces the old one.

**Codex not found on PATH (Windows).** The npm install puts a `codex.cmd` shim
in the npm global folder (usually `%APPDATA%\npm`). Concat looks for `codex.exe`
and `codex.cmd` on `PATH`. Open a new terminal after installing, run `where
codex` to check, and make sure that folder is on `PATH`. Then press **Check
again**.

**OpenRouter answers 402.** The account is out of credits. Add credits at
<https://openrouter.ai> and generate again. The error text says
`Insufficient credits`.

**A video model refuses the request.** Some video models are limited to
accounts that have been given access. Choose another model from the live list,
or check the model's page on OpenRouter.

**Drive says "access blocked" or "has not completed verification".** Your
Google account is not a test user of the OAuth client. Open the OAuth consent
screen in the Google Cloud Console, add your account under **Test users**, and
press **Connect** again.

**Google Flow or ChatGPT web stops producing files.** The site's layout has
probably changed. Check that both the extension and the page are current, and
that the page is open and signed in. See the note in section 6.
