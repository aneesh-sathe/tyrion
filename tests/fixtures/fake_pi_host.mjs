#!/usr/bin/env node

import { pathToFileURL } from "node:url";

const extensionIndex = process.argv.findIndex(
  (argument) => argument === "--extension" || argument === "-e",
);
if (extensionIndex < 0 || !process.argv[extensionIndex + 1]) {
  process.stderr.write("fake Pi requires --extension\n");
  process.exit(2);
}

const extensionPath = process.argv[extensionIndex + 1];
const extension = await import(pathToFileURL(extensionPath).href);
const handlers = new Map();
const commands = new Map();
const tools = new Map();
const messages = [];
let shutdownRequested = false;

const api = {
  on(event, handler) {
    const registered = handlers.get(event) ?? [];
    registered.push(handler);
    handlers.set(event, registered);
  },
  registerCommand(name, command) {
    commands.set(name, command);
  },
  registerTool(tool) {
    tools.set(tool.name, tool);
  },
  sendMessage(message) {
    messages.push({ role: "custom", ...message });
  },
};

const ui = {
  notify(message, level) {
    messages.push({
      role: "custom",
      customType: "fake-pi-notification",
      content: message,
      details: { level },
    });
  },
  setStatus(key, value) {
    messages.push({
      role: "custom",
      customType: "fake-pi-status",
      content: value ?? "",
      details: { key },
    });
  },
  setWidget(key, value) {
    messages.push({
      role: "custom",
      customType: "fake-pi-widget",
      content: Array.isArray(value) ? value.join("\n") : "",
      details: { key },
    });
  },
};

const context = {
  cwd: process.cwd(),
  hasUI: true,
  mode: "rpc",
  sessionManager: {
    getSessionId() {
      return "fake-pi-session";
    },
    getSessionFile() {
      return undefined;
    },
  },
  shutdown() {
    shutdownRequested = true;
  },
  ui,
};

await extension.default(api);
for (const handler of handlers.get("session_start") ?? []) {
  await handler({ reason: "startup" }, context);
}

let input = "";
process.stdin.setEncoding("utf8");
process.stdin.on("data", async (chunk) => {
  input += chunk;
  while (input.includes("\n")) {
    const newline = input.indexOf("\n");
    const line = input.slice(0, newline);
    input = input.slice(newline + 1);
    if (!line) continue;

    const request = JSON.parse(line);
    try {
      if (request.type === "get_state") {
        respond(request, "get_state", {
          sessionId: "fake-pi-session",
          isStreaming: false,
        });
      } else if (request.type === "get_messages") {
        respond(request, "get_messages", { messages });
      } else if (request.type === "get_commands") {
        respond(request, "get_commands", {
          commands: [...commands.entries()].map(([name, command]) => ({
            name,
            description: command.description,
            source: "extension",
          })),
        });
      } else if (request.type === "prompt") {
        const match = request.message.match(/^\/([^\s]+)(?:\s+([\s\S]*))?$/);
        const command = match ? commands.get(match[1]) : undefined;
        if (!command) throw new Error(`unknown extension command: ${request.message}`);
        await command.handler(match[2] ?? "", context);
        respond(request, "prompt");
      } else if (request.type === "invoke_tool") {
        const tool = tools.get(request.name);
        if (!tool) throw new Error(`unknown extension tool: ${request.name}`);
        const result = await tool.execute(
          request.id ?? "fake-tool-call",
          request.arguments ?? {},
          undefined,
          undefined,
          context,
        );
        respond(request, "invoke_tool", result);
      } else {
        throw new Error(`unsupported fake Pi request: ${request.type}`);
      }
    } catch (error) {
      process.stdout.write(
        `${JSON.stringify({
          id: request.id,
          type: "response",
          command: request.type,
          success: false,
          error: error instanceof Error ? error.message : String(error),
        })}\n`,
      );
    }

    if (shutdownRequested) process.exit(process.exitCode ?? 0);
  }
});

process.stdin.on("end", async () => {
  for (const handler of handlers.get("session_shutdown") ?? []) {
    await handler({ reason: "quit" }, context);
  }
});

function respond(request, command, data) {
  process.stdout.write(
    `${JSON.stringify({
      id: request.id,
      type: "response",
      command,
      success: true,
      ...(data === undefined ? {} : { data }),
    })}\n`,
  );
}
