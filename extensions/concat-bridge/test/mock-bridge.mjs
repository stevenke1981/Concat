// A stand-in for Concat's bridge for the tests. It answers the routes the way
// bridge.rs does: a `next` poll with a wait holds the request open until a job
// is added or the wait ends, a cancelled job refuses posts with 409, and a wrong
// token gets 401. Every request is recorded in `calls`, so a test can check the
// order and content of what the extension sent.
import http from "node:http";

export const TOKEN = "0123456789abcdef0123456789abcdef";

/**
 * Starts the mock on 127.0.0.1 with a free port. `states` gives the job states
 * the mock reports for known job ids; a job added with `addJob` is known too.
 */
export function startMockBridge({ states = {} } = {}) {
  const queue = [];
  const holding = new Set(); // `next` polls waiting for a job
  const known = new Map(Object.entries(states));
  const calls = [];

  const server = http.createServer(async (req, res) => {
    const chunks = [];
    for await (const chunk of req) chunks.push(chunk);
    const body = Buffer.concat(chunks);
    const url = new URL(req.url, "http://127.0.0.1");
    calls.push({
      method: req.method,
      path: `${url.pathname}${url.search}`,
      token: req.headers["x-concat-token"],
      contentType: req.headers["content-type"],
      fileName: req.headers["x-file-name"],
      index: req.headers["x-index"],
      body,
    });
    const reply = (status, json) => {
      if (json === undefined) return res.writeHead(status).end();
      res.writeHead(status, { "Content-Type": "application/json" });
      res.end(JSON.stringify(json));
    };

    if (url.pathname === "/v1/hello") {
      return reply(200, { app: "concat", version: "0.1.0-test", protocol: 1 });
    }
    if (req.headers["x-concat-token"] !== TOKEN) return reply(401, { error: "unauthorized" });

    if (url.pathname === "/v1/jobs/next") {
      const wait = Math.min(Number(url.searchParams.get("wait") ?? 0), 25);
      if (queue.length === 0 && wait > 0) {
        await hold(wait, res);
      }
      if (queue.length === 0) return reply(204);
      return reply(200, queue.shift());
    }

    const match = /^\/v1\/jobs\/([^/]+)(?:\/(progress|file|done|fail))?$/.exec(url.pathname);
    if (!match) return reply(404, { error: "no such route" });
    const [, id, action] = match;
    if (!known.has(id)) return reply(404, { error: "no such job" });
    if (!action) return reply(200, { state: known.get(id) });
    if (known.get(id) === "cancelled") return reply(409, { error: "cancelled" });
    if (action === "file") return reply(200, { saved: "1-file.png" });
    return reply(204);
  });

  // Holds a poll for up to `seconds`, or until a job arrives or the client goes away.
  function hold(seconds, res) {
    return new Promise((resolve) => {
      const done = () => {
        clearTimeout(timer);
        holding.delete(done);
        resolve();
      };
      const timer = setTimeout(done, seconds * 1000);
      holding.add(done);
      res.once("close", done);
    });
  }

  return new Promise((resolve) => {
    server.listen(0, "127.0.0.1", () => {
      const port = server.address().port;
      resolve({
        port,
        settings: { port, token: TOKEN },
        calls,
        /** Queues a job for the next poll, waking a held one. */
        addJob(job) {
          if (!known.has(job.id)) known.set(job.id, "taken");
          queue.push(job);
          for (const wake of holding) wake();
        },
        /** Sets the state the mock reports for a job id. */
        setState(id, state) {
          known.set(id, state);
        },
        close() {
          server.closeAllConnections();
          return new Promise((done) => server.close(done));
        },
      });
    });
  });
}
