import { appendFileSync, createReadStream, existsSync, statSync } from "node:fs";
import { createServer } from "node:http";
import { basename, join, resolve } from "node:path";

function fail(message) {
  process.stderr.write(`${message}\n`);
  process.exit(1);
}

function parsePort(value, label) {
  const port = Number(value);
  if (!Number.isInteger(port) || port < 1024 || port > 65535) {
    fail(`${label} must be an unprivileged TCP port from 1024 through 65535.`);
  }
  return port;
}

function logRequest(logPath, request, pathname, status) {
  appendFileSync(
    logPath,
    `${JSON.stringify({
      timestamp_utc: new Date().toISOString(),
      method: request.method,
      path: pathname,
      status,
    })}\n`,
    "utf8",
  );
}

function serve(rootArgument, artifactArgument, portArgument, logArgument) {
  const root = resolve(rootArgument);
  const artifactName = basename(artifactArgument);
  const port = parsePort(portArgument, "HTTP port");
  const logPath = resolve(logArgument);
  const allowed = new Map([
    ["/latest.json", join(root, "latest.json")],
    [`/${artifactName}`, join(root, artifactName)],
  ]);

  for (const filePath of allowed.values()) {
    if (!existsSync(filePath) || !statSync(filePath).isFile()) {
      fail(`Required loopback fixture is missing: ${filePath}`);
    }
  }

  const server = createServer((request, response) => {
    let pathname;
    try {
      pathname = decodeURIComponent(new URL(request.url ?? "/", "http://127.0.0.1").pathname);
    } catch {
      response.writeHead(400).end();
      logRequest(logPath, request, "<invalid-url>", 400);
      return;
    }

    const filePath = allowed.get(pathname);
    if (request.method !== "GET" || !filePath) {
      response.writeHead(404, { "Cache-Control": "no-store" }).end();
      logRequest(logPath, request, pathname, 404);
      return;
    }

    const contentType = pathname === "/latest.json" ? "application/json" : "application/octet-stream";
    response.writeHead(200, {
      "Cache-Control": "no-store",
      "Content-Length": statSync(filePath).size,
      "Content-Type": contentType,
      "X-Content-Type-Options": "nosniff",
    });
    createReadStream(filePath).pipe(response);
    logRequest(logPath, request, pathname, 200);
  });

  server.on("error", (error) => fail(`Loopback updater server failed: ${error.message}`));
  server.listen(port, "127.0.0.1", () => {
    process.stdout.write(`READY http://127.0.0.1:${port}/latest.json\n`);
  });
}

async function waitForPage(debugPort, deadline) {
  const endpoint = `http://127.0.0.1:${debugPort}/json/list`;
  let lastError = "no WebView page was returned";
  while (Date.now() < deadline) {
    try {
      const response = await fetch(endpoint, { cache: "no-store" });
      if (response.ok) {
        const pages = await response.json();
        const page = pages.find(
          (candidate) =>
            typeof candidate.webSocketDebuggerUrl === "string" &&
            /^(?:https?:\/\/tauri\.localhost\/|tauri:\/\/localhost\/)/.test(candidate.url ?? ""),
        );
        if (page) return page;
        lastError = `debug endpoint returned ${pages.length} page(s), but no Pusula WebView`;
      } else {
        lastError = `debug endpoint returned HTTP ${response.status}`;
      }
    } catch (error) {
      lastError = error instanceof Error ? error.message : String(error);
    }
    await new Promise((resolvePromise) => setTimeout(resolvePromise, 200));
  }
  throw new Error(`Timed out waiting for the isolated Pusula WebView: ${lastError}`);
}

async function observe(debugPortArgument, timeoutArgument) {
  const debugPort = parsePort(debugPortArgument, "WebView debug port");
  const timeoutSeconds = Number(timeoutArgument);
  if (!Number.isInteger(timeoutSeconds) || timeoutSeconds < 20 || timeoutSeconds > 120) {
    fail("Observation timeout must be an integer from 20 through 120 seconds.");
  }
  const deadline = Date.now() + timeoutSeconds * 1000;
  const watchdog = setTimeout(
    () => fail("The isolated updater observation exceeded its hard timeout."),
    timeoutSeconds * 1000 + 2000,
  );
  const page = await waitForPage(debugPort, deadline);
  const socket = new WebSocket(page.webSocketDebuggerUrl);
  const pending = new Map();
  const consoleMessages = [];
  let nextId = 1;
  let rejectionResolve;
  const rejectionMessage = new Promise((resolvePromise) => {
    rejectionResolve = resolvePromise;
  });

  const send = (method, params = {}) => {
    const id = nextId++;
    return new Promise((resolvePromise, rejectPromise) => {
      pending.set(id, { resolve: resolvePromise, reject: rejectPromise });
      socket.send(JSON.stringify({ id, method, params }));
    });
  };

  socket.addEventListener("message", (event) => {
    const message = JSON.parse(String(event.data));
    if (message.id) {
      const waiter = pending.get(message.id);
      if (!waiter) return;
      pending.delete(message.id);
      if (message.error) waiter.reject(new Error(message.error.message));
      else waiter.resolve(message.result);
      return;
    }

    if (message.method === "Runtime.consoleAPICalled") {
      const rendered = (message.params.args ?? [])
        .map((argument) => argument.value ?? argument.description ?? argument.type)
        .join(" ");
      consoleMessages.push({ type: message.params.type, text: rendered });
      if (rendered.includes("Pusula update failed during downloading")) rejectionResolve(rendered);
    }
  });

  await new Promise((resolvePromise, rejectPromise) => {
    socket.addEventListener("open", resolvePromise, { once: true });
    socket.addEventListener("error", () => rejectPromise(new Error("Could not attach to the Pusula WebView.")), {
      once: true,
    });
  });
  await send("Runtime.enable");

  const setupResult = await send("Runtime.evaluate", {
    expression: `new Promise((resolve, reject) => {
      window.__PUSULA_INVALID_SIGNATURE_HARNESS_CONFIRM_CALLED__ = false;
      window.confirm = () => {
        window.__PUSULA_INVALID_SIGNATURE_HARNESS_CONFIRM_CALLED__ = true;
        return false;
      };
      const deadline = Date.now() + 10000;
      const attempt = () => {
        const modal = document.getElementById('pusula-data-modal');
        const button = document.getElementById('pusula-empty-start-button');
        if (modal?.dataset.mode === 'first-run' && typeof button?.onclick === 'function') {
          button.click();
          setTimeout(() => {
            button.click();
            resolve(true);
          }, 200);
          return;
        }
        if (Date.now() >= deadline) {
          reject(new Error('The isolated first-run screen did not become ready.'));
          return;
        }
        setTimeout(attempt, 100);
      };
      attempt();
    })`,
    awaitPromise: true,
    returnByValue: true,
  });
  if (setupResult.exceptionDetails || setupResult.result?.value !== true) {
    throw new Error("Could not initialize the isolated first-run profile.");
  }

  const remaining = Math.max(1, deadline - Date.now());
  const warning = await Promise.race([
    rejectionMessage,
    new Promise((_, rejectPromise) =>
      setTimeout(
        () => rejectPromise(new Error("The app did not report updater rejection before the observation timeout.")),
        remaining,
      ),
    ),
  ]);
  const stateResult = await send("Runtime.evaluate", {
    expression: `({
      confirmationCalled: window.__PUSULA_INVALID_SIGNATURE_HARNESS_CONFIRM_CALLED__ === true,
      systemStatus: document.getElementById('pusula-system-status')?.textContent ?? '',
      title: document.title,
      url: location.href
    })`,
    returnByValue: true,
  });
  socket.close();
  clearTimeout(watchdog);

  process.stdout.write(
    `${JSON.stringify({
      rejection_warning: warning,
      confirmation_called: stateResult.result.value.confirmationCalled,
      system_status: stateResult.result.value.systemStatus,
      page_title: stateResult.result.value.title,
      page_url: stateResult.result.value.url,
      console_messages: consoleMessages,
    })}\n`,
  );
}

const [mode, ...argumentsList] = process.argv.slice(2);
if (mode === "serve" && argumentsList.length === 4) {
  serve(...argumentsList);
} else if (mode === "observe" && argumentsList.length === 2) {
  observe(...argumentsList).catch((error) => fail(error instanceof Error ? error.message : String(error)));
} else {
  fail(
    "Usage: Invoke-InvalidUpdaterRuntime.mjs serve <root> <artifact-name> <port> <log> | observe <debug-port> <timeout-seconds>",
  );
}
