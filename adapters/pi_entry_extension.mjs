import net from "node:net";
import { randomUUID } from "node:crypto";

const PROTOCOL_VERSION = 2;
const ADAPTER_IDENTITY = "tyrion-pi-entry";
const ADAPTER_VERSION = "1.0.0";
const RESPONSE_LIMIT_BYTES = 16 * 1024 * 1024;
const REQUEST_TIMEOUT_MS = 5_000;

export default function tyrionPiEntry(pi) {
  const state = {
    attachment: undefined,
    attachmentToken: undefined,
    commissionId: process.env.TYRION_PI_COMMISSION_ID || undefined,
    cursor: Number.parseInt(process.env.TYRION_PI_LAST_EVENT_SEQUENCE ?? "0", 10),
    pollInFlight: false,
    pollTimer: undefined,
    pollWarningVisible: false,
    socketPath: process.env.TYRION_PI_SOCKET,
  };

  pi.registerCommand("tyrion-status", {
    description: "Show the attached Tyrion Commission",
    handler: async (_arguments, context) => {
      ensureAttached(state);
      const commission = state.commissionId
        ? await authenticatedRequest(state, {
            type: "inspect_commission",
            commission_id: state.commissionId,
          })
        : undefined;
      renderVisible(pi, context, state, commission);
    },
  });

  pi.registerCommand("tyrion-propose", {
    description: "Create a Tyrion Commission Proposal from JSON",
    handler: async (argumentsText, context) => {
      ensureAttached(state);
      const proposal = parseJsonArgument(argumentsText, "Commission Proposal JSON");
      const data = await authenticatedRequest(
        state,
        { type: "create_proposal", proposal },
        `pi-propose-${randomUUID()}`,
      );
      const projection = commissionProjection(data);
      state.commissionId = projection.commission.id;
      updateCursorFromCommission(state, projection);
      renderVisible(pi, context, state, projection);
      startPolling(pi, context, state);
    },
  });

  pi.registerCommand("tyrion-accept", {
    description: "Accept the current Tyrion Commission Proposal",
    handler: async (argumentsText, context) => {
      ensureAttached(state);
      const commissionId = selectedCommissionId(state, argumentsText);
      const current = commissionProjection(
        await authenticatedRequest(state, {
          type: "inspect_commission",
          commission_id: commissionId,
        }),
      );
      const data = await authenticatedRequest(
        state,
        { type: "accept_commission", commission_id: commissionId },
        `pi-accept-${randomUUID()}`,
        current.commission.revision,
      );
      const projection = commissionProjection(data);
      updateCursorFromCommission(state, projection);
      renderVisible(pi, context, state, projection);
      startPolling(pi, context, state);
    },
  });

  pi.registerCommand("tyrion-replay", {
    description: "Replay unseen durable Tyrion Commission events",
    handler: async (argumentsText, context) => {
      ensureAttached(state);
      const commissionId = selectedCommissionId(state);
      const afterSequence = argumentsText.trim()
        ? parseNonnegativeInteger(argumentsText.trim(), "event replay cursor")
        : state.cursor;
      const replay = await authenticatedRequest(state, {
        type: "replay_events",
        commission_id: commissionId,
        after_sequence: afterSequence,
      });
      applyReplayState(state, replay);
      renderReplay(pi, context, state, replay);
      renderMaterialNotifications(pi, context, replay.material_notifications ?? []);
      const projection = await inspectSelectedCommission(state);
      renderVisible(pi, context, state, projection);
    },
  });

  pi.registerCommand("tyrion-downgrade", {
    description: "Downgrade this Pi Attachment to Limited or Observer mode",
    handler: async (argumentsText, context) => {
      ensureAttached(state);
      const mode = argumentsText.trim();
      const capabilities = downgradeCapabilities(mode);
      const commissionId = selectedCommissionId(state);
      const current = commissionProjection(
        await authenticatedRequest(state, {
          type: "inspect_commission",
          commission_id: commissionId,
        }),
      );
      const data = await authenticatedRequest(
        state,
        {
          type: "update_attachment_capabilities",
          commission_id: commissionId,
          capabilities,
        },
        `pi-downgrade-${randomUUID()}`,
        current.commission.revision,
      );
      state.attachment = data.attachment;
      const projection = commissionProjection(data);
      updateCursorFromCommission(state, projection);
      renderVisible(pi, context, state, projection);
    },
  });

  pi.registerCommand("tyrion-takeover", {
    description: "Explicitly take Active Attachment control of the current Commission",
    handler: async (_argumentsText, context) => {
      ensureAttached(state);
      const commissionId = selectedCommissionId(state);
      const current = commissionProjection(
        await authenticatedRequest(state, {
          type: "inspect_commission",
          commission_id: commissionId,
        }),
      );
      await authenticatedRequest(
        state,
        { type: "take_control", commission_id: commissionId },
        `pi-takeover-${randomUUID()}`,
        current.commission.revision,
        current.commission.control_revision,
      );
      const projection = commissionProjection(
        await authenticatedRequest(state, {
          type: "inspect_commission",
          commission_id: commissionId,
        }),
      );
      state.attachment.role = "active";
      updateCursorFromCommission(state, projection);
      renderVisible(pi, context, state, projection);
    },
  });

  registerCommissionStateCommand(
    pi,
    state,
    "tyrion-pause",
    "pause_commission",
    "Pause new Tyrion Worker dispatch",
  );
  registerCommissionStateCommand(
    pi,
    state,
    "tyrion-resume",
    "resume_commission",
    "Resume safe Tyrion Worker dispatch",
  );
  registerCommissionStateCommand(
    pi,
    state,
    "tyrion-cancel",
    "cancel_commission",
    "Cancel the current Tyrion Commission while retaining history",
  );
  registerWorkerCommand(
    pi,
    state,
    "tyrion-steer",
    "steer_worker",
    "Send an immutable-scope clarification to a Tyrion Worker",
    "clarification",
  );
  registerWorkerCommand(
    pi,
    state,
    "tyrion-interrupt",
    "interrupt_worker",
    "Semantically interrupt a Tyrion Worker",
    "reason",
  );
  registerWorkerCommand(
    pi,
    state,
    "tyrion-retry",
    "retry_worker",
    "Retry an interrupted Tyrion Worker",
  );

  pi.registerCommand("tyrion-propose-operation", {
    description: "Propose an operation and render its Approval Gate",
    handler: async (argumentsText, context) => {
      ensureAttached(state);
      const commissionId = selectedCommissionId(state);
      const operation = parseJsonArgument(argumentsText, "operation JSON");
      const current = await inspectSelectedCommission(state);
      const data = await authenticatedRequest(
        state,
        { type: "propose_operation", commission_id: commissionId, operation },
        `pi-propose-operation-${randomUUID()}`,
        current.commission.revision,
      );
      const projection = commissionProjection(data);
      updateCursorFromCommission(state, projection);
      renderVisible(pi, context, state, projection);
    },
  });

  pi.registerCommand("tyrion-gate", {
    description: "Inspect a Tyrion Approval Gate without approving it",
    handler: async (argumentsText, context) => {
      ensureAttached(state);
      const approvalGateId = requiredArgument(argumentsText, "Approval Gate id");
      const projection = await inspectSelectedCommission(state);
      const gate = (projection.approval_gates ?? []).find(
        (candidate) => candidate.id === approvalGateId,
      );
      if (!gate) throw new Error(`Approval Gate ${approvalGateId} is not in this Commission`);
      const content = [
        "APPROVAL REQUIRED: use the independent Principal control path.",
        JSON.stringify(gate, undefined, 2),
      ].join("\n");
      context.ui.notify(content, "warning");
      pi.sendMessage({
        customType: "tyrion-approval-gate",
        content,
        display: true,
        details: { gate },
      });
    },
  });

  pi.registerTool({
    name: "tyrion_entry",
    label: "Tyrion Entry Session",
    description: "Inspect the current Tyrion attachment and Commission state.",
    promptSnippet: "Inspect the durable Tyrion Commission attached to this Pi session",
    promptGuidelines: [
      "Use tyrion_entry when the Principal asks about the attached Tyrion Commission.",
    ],
    parameters: {
      type: "object",
      properties: {
        action: { type: "string", enum: ["status"] },
      },
      required: ["action"],
      additionalProperties: false,
    },
    async execute(_toolCallId, parameters, _signal, _onUpdate, context) {
      ensureAttached(state);
      if (parameters.action !== "status") {
        throw new Error(`unsupported Tyrion Entry action: ${parameters.action}`);
      }
      const projection = state.commissionId ? await inspectSelectedCommission(state) : undefined;
      synchronizeAttachment(state, projection);
      const rendered = renderCommission(state, projection);
      return {
        content: [{ type: "text", text: rendered }],
        details: attachmentDetails(state),
      };
    },
  });

  pi.on("session_start", async (_event, context) => {
    const launchToken = process.env.TYRION_PI_LAUNCH_TOKEN;
    delete process.env.TYRION_PI_LAUNCH_TOKEN;
    if (!launchToken || !state.socketPath) {
      failClosed(pi, context, "explicit Tyrion Pi launch context is missing");
      return;
    }

    try {
      const capabilities = parseCapabilities(process.env.TYRION_PI_CAPABILITIES);
      const command = {
        type: "connect_attachment",
        launch_token: launchToken,
        handshake: {
          adapter: {
            harness: "pi",
            adapter_identity: ADAPTER_IDENTITY,
            adapter_version: ADAPTER_VERSION,
          },
          adapter_protocol_version: PROTOCOL_VERSION,
          native_session_id: context.sessionManager.getSessionId(),
          capabilities,
        },
        ...(state.commissionId
          ? {
              replay: {
                commission_id: state.commissionId,
                last_event_sequence: state.cursor,
              },
            }
          : {}),
      };
      const data = await sendRequest(state.socketPath, {
        protocol_version: PROTOCOL_VERSION,
        idempotency_key: `pi-connect-${randomUUID()}`,
        command,
      });
      state.attachment = data.attachment;
      if (data.commission_role) state.attachment.role = data.commission_role;
      state.attachmentToken = data.attachment_session_token;
      if (data.commission_id) state.commissionId = data.commission_id;
      if (data.replay?.next_event_sequence !== undefined) {
        applyReplayState(state, data.replay);
        renderReplay(pi, context, state, data.replay);
        renderMaterialNotifications(pi, context, data.replay.material_notifications ?? []);
      }
      const projection = state.commissionId
        ? commissionProjection(
            await authenticatedRequest(state, {
              type: "inspect_commission",
              commission_id: state.commissionId,
            }),
          )
        : undefined;
      renderVisible(pi, context, state, projection);
      startPolling(pi, context, state);
    } catch (error) {
      failClosed(pi, context, errorMessage(error));
    }
  });

  pi.on("session_shutdown", async () => {
    if (state.pollTimer) clearInterval(state.pollTimer);
    state.pollTimer = undefined;
  });
}

function registerWorkerCommand(pi, state, commandName, commandType, description, textField) {
  pi.registerCommand(commandName, {
    description,
    handler: async (argumentsText, context) => {
      ensureAttached(state);
      const [workerHandle, remainder] = splitRequiredArgument(
        argumentsText,
        "Worker handle",
        textField,
      );
      const commissionId = selectedCommissionId(state);
      const current = await inspectSelectedCommission(state);
      const command = {
        type: commandType,
        commission_id: commissionId,
        worker_handle: workerHandle,
        ...(textField ? { [textField]: remainder } : {}),
      };
      const data = await authenticatedRequest(
        state,
        command,
        `pi-${commandType}-${randomUUID()}`,
        current.commission.revision,
      );
      const projection = commissionProjection(data);
      updateCursorFromCommission(state, projection);
      renderVisible(pi, context, state, projection);
    },
  });
}

function registerCommissionStateCommand(pi, state, commandName, commandType, description) {
  pi.registerCommand(commandName, {
    description,
    handler: async (_argumentsText, context) => {
      ensureAttached(state);
      const commissionId = selectedCommissionId(state);
      const current = commissionProjection(
        await authenticatedRequest(state, {
          type: "inspect_commission",
          commission_id: commissionId,
        }),
      );
      const data = await authenticatedRequest(
        state,
        { type: commandType, commission_id: commissionId },
        `pi-${commandType}-${randomUUID()}`,
        current.commission.revision,
      );
      const projection = commissionProjection(data);
      updateCursorFromCommission(state, projection);
      renderVisible(pi, context, state, projection);
    },
  });
}

function startPolling(pi, context, state) {
  if (state.pollTimer || !state.commissionId) return;
  const configured = Number.parseInt(process.env.TYRION_PI_POLL_INTERVAL_MS ?? "250", 10);
  const interval = Number.isSafeInteger(configured) && configured >= 50 ? configured : 250;
  state.pollTimer = setInterval(() => {
    void pollEvents(pi, context, state);
  }, interval);
  state.pollTimer.unref?.();
}

async function pollEvents(pi, context, state) {
  if (state.pollInFlight || !state.commissionId || !state.attachmentToken) return;
  state.pollInFlight = true;
  try {
    const priorCursor = state.cursor;
    const replay = await authenticatedRequest(state, {
      type: "replay_events",
      commission_id: state.commissionId,
      after_sequence: priorCursor,
    });
    const unseenEvents = (replay.events ?? []).filter(
      (event) => Number.isSafeInteger(event.sequence) && event.sequence > state.cursor,
    );
    const unseenMaterial = (replay.material_notifications ?? []).filter(
      (event) => Number.isSafeInteger(event.sequence) && event.sequence > state.cursor,
    );
    applyReplayState(state, replay);
    renderMaterialNotifications(pi, context, unseenMaterial);
    if (unseenEvents.length) {
      const projection = commissionProjection(
        await authenticatedRequest(state, {
          type: "inspect_commission",
          commission_id: state.commissionId,
        }),
      );
      renderVisible(pi, context, state, projection);
    }
    state.pollWarningVisible = false;
  } catch (error) {
    if (!state.pollWarningVisible) {
      const content = `[Tyrion: Disconnected] Event replay unavailable: ${errorMessage(error)}`;
      context.ui.setStatus("tyrion", "Tyrion: Disconnected");
      context.ui.notify(content, "error");
      pi.sendMessage({
        customType: "tyrion-notification",
        content,
        display: true,
        details: { connected: false },
      });
      state.pollWarningVisible = true;
    }
  } finally {
    state.pollInFlight = false;
  }
}

function renderMaterialNotifications(pi, context, events) {
  for (const event of events) {
    const content = materialNotification(event);
    context.ui.notify(content, notificationLevel(event));
    pi.sendMessage({
      customType: "tyrion-notification",
      content,
      display: true,
      details: { event },
    });
  }
}

function materialNotification(event) {
  switch (event.type) {
    case "approval_gate_opened":
      return `APPROVAL REQUIRED: inspect the open Tyrion Approval Gate through independent Principal control. Event ${event.sequence}.`;
    case "assignment_blocked":
      return `BLOCKER: Tyrion work needs attention. Event ${event.sequence}.`;
    case "commission_verified_complete":
      return `Verified Complete. Tyrion Commission event ${event.sequence}.`;
    case "commission_cancelled":
      return `Tyrion Commission cancelled. Event ${event.sequence}.`;
    default:
      return `Tyrion material update: ${event.type}. Event ${event.sequence}.`;
  }
}

function notificationLevel(event) {
  return ["approval_gate_opened", "assignment_blocked"].includes(event.type)
    ? "warning"
    : "info";
}

function parseCapabilities(encoded) {
  if (!encoded) throw new Error("Pi capability manifest is missing");
  const capabilities = JSON.parse(encoded);
  if (!Array.isArray(capabilities) || capabilities.some((value) => typeof value !== "string")) {
    throw new Error("Pi capability manifest is invalid");
  }
  return capabilities;
}

function parseJsonArgument(encoded, name) {
  if (!encoded.trim()) throw new Error(`${name} is required`);
  try {
    return JSON.parse(encoded);
  } catch (_error) {
    throw new Error(`${name} is invalid`);
  }
}

function parseNonnegativeInteger(encoded, name) {
  const value = Number.parseInt(encoded, 10);
  if (!Number.isSafeInteger(value) || value < 0 || String(value) !== encoded) {
    throw new Error(`${name} must be a nonnegative integer`);
  }
  return value;
}

function requiredArgument(encoded, name) {
  const value = encoded.trim();
  if (!value) throw new Error(`${name} is required`);
  return value;
}

function splitRequiredArgument(encoded, firstName, remainderName) {
  const value = encoded.trim();
  const separator = value.indexOf(" ");
  if (separator === -1) {
    if (remainderName) throw new Error(`${firstName} and ${remainderName} are required`);
    if (!value) throw new Error(`${firstName} is required`);
    return [value, ""];
  }
  const first = value.slice(0, separator).trim();
  const remainder = value.slice(separator + 1).trim();
  if (!first || (remainderName && !remainder)) {
    throw new Error(`${firstName}${remainderName ? ` and ${remainderName}` : ""} are required`);
  }
  return [first, remainder];
}

function selectedCommissionId(state, argument = "") {
  const requested = argument.trim();
  if (requested) state.commissionId = requested;
  if (!state.commissionId) throw new Error("No Tyrion Commission is selected");
  return state.commissionId;
}

function downgradeCapabilities(mode) {
  if (mode === "limited") {
    return [
      "proposal_creation",
      "commission_acceptance",
      "commission_inspection",
      "event_replay",
      "control_takeover",
      "persistent_mode_display",
      "worker_steering",
      "worker_interruption",
    ];
  }
  if (mode === "observer") {
    return ["commission_inspection", "event_replay", "persistent_mode_display"];
  }
  throw new Error("Attachment downgrade mode must be limited or observer");
}

function ensureAttached(state) {
  if (!state.attachment || !state.attachmentToken) {
    throw new Error("Pi is not attached to Tyrion");
  }
}

async function inspectSelectedCommission(state) {
  return commissionProjection(
    await authenticatedRequest(state, {
      type: "inspect_commission",
      commission_id: selectedCommissionId(state),
    }),
  );
}

function renderVisible(pi, context, state, projection) {
  synchronizeAttachment(state, projection);
  const modeTag = state.attachment?.mode_tag ?? "Tyrion: Disconnected";
  context.ui.setStatus("tyrion", modeTag);
  context.ui.setWidget("tyrion", [renderCommission(state, projection)]);
  pi.sendMessage({
    customType: "tyrion-commission",
    content: renderCommission(state, projection),
    display: true,
    details: {
      ...attachmentDetails(state),
      commission: projection,
    },
  });
}

function renderReplay(pi, context, state, replay) {
  const modeTag = state.attachment?.mode_tag ?? "Tyrion: Disconnected";
  const events = replay.events ?? [];
  const content = [
    `[${modeTag}] Durable event replay`,
    ...(events.length
      ? events.map((event) => `${event.sequence}. ${event.type}`)
      : ["No unseen events."]),
  ].join("\n");
  context.ui.setStatus("tyrion", modeTag);
  pi.sendMessage({
    customType: "tyrion-events",
    content,
    display: true,
    details: { replay },
  });
}

function renderCommission(state, projection) {
  const modeTag = state.attachment?.mode_tag ?? "Tyrion: Disconnected";
  const role = state.attachment?.role ?? "unlinked";
  const capabilityLimits = (state.attachment?.missing_capabilities ?? []).map(
    (missing) =>
      `- ${missing.capability}: ${missing.effect}${missing.alternative ? ` ${missing.alternative}` : ""}`,
  );
  if (!projection) {
    return [
      `[${modeTag}] Attached. No Commission selected.`,
      `Attachment: ${role}`,
      ...(capabilityLimits.length ? ["Capability limits", ...capabilityLimits] : []),
    ].join("\n");
  }
  const commission = projection.commission;
  const criteria = (projection.criteria ?? []).map(
    (criterion) => `- ${criterion.id}: ${criterion.status} - ${criterion.description}`,
  );
  const assignments = (projection.assignments ?? []).map(
    (assignment) =>
      `- ${assignment.id}: ${assignment.status} | ${assignment.goal ?? "goal unavailable"}`,
  );
  const workers = (projection.workers ?? []).map((worker) => {
    const configuration = worker.configuration ?? {};
    const adapter = configuration.adapter?.kind ?? "unknown-adapter";
    return [
      `- ${worker.handle}: ${worker.status} | configuration ${configuration.id ?? "unknown"} (${adapter})`,
      `  controls: ${formatList(worker.available_controls)}`,
      `  activity: ${worker.latest_meaningful_activity ?? "unavailable"}`,
      `  usage: ${formatJson(worker.usage)}`,
      `  routing: ${formatJson(worker.routing_rationale)}`,
    ].join("\n");
  });
  const results = (projection.results ?? []).map(
    (result) =>
      `- ${result.id}: ${result.status} | output ${result.output || "none"} | artifacts ${formatJson(result.artifacts)} | known effects ${formatJson(result.known_effects)}`,
  );
  const evidence = (projection.evidence ?? []).map(
    (record) =>
      `- ${record.id}: criterion ${record.criterion_id} | ${record.outcome} | ${record.verifier_type} | observed ${formatJson(record.observed)}`,
  );
  const gates = (projection.approval_gates ?? [])
    .filter((gate) => gate.status === "open")
    .map(
      (gate) =>
        `APPROVAL REQUIRED: inspect ${gate.id} through the independent Principal control path before authorizing operation ${gate.operation_request_id}.`,
    );
  const blockers = (projection.blockers ?? []).map(
    (blocker) => `- Blocked: ${blocker.requirement}`,
  );
  const resumableBlocker = projection.recovery?.resumable_blocker;
  const blockerDecision = resumableBlocker
    ? [
        `BLOCKER: ${resumableBlocker.exact_next_requirement}`,
        `Passed criteria: ${formatList(resumableBlocker.passed_criteria)}`,
        `Unresolved criteria: ${formatList(resumableBlocker.unresolved_criteria)}`,
      ]
    : [];
  return [
    `[${modeTag}] ${commission.goal}`,
    ...gates,
    ...blockerDecision,
    `Attachment: ${role}`,
    ...(capabilityLimits.length ? ["Capability limits", ...capabilityLimits] : []),
    `Status: ${commission.status} | revision ${commission.revision} | control revision ${commission.control_revision}`,
    `Authority Envelope: ${formatJson(commission.authority)}`,
    `Resource ceilings: ${formatJson(commission.resource_ceilings)}`,
    ...(blockers.length ? ["Blockers", ...blockers] : []),
    "Acceptance criteria",
    ...(criteria.length ? criteria : ["- None"]),
    "Assignments",
    ...(assignments.length ? assignments : ["- None"]),
    "Workers",
    ...(workers.length ? workers : ["- None"]),
    "Results",
    ...(results.length ? results : ["- None"]),
    "Evidence",
    ...(evidence.length ? evidence : ["- None"]),
    `Verification: ${formatJson(projection.verification)}`,
    `Completion briefing: ${formatJson(projection.briefing)}`,
  ].join("\n");
}

function formatList(values) {
  return Array.isArray(values) && values.length ? values.join(", ") : "none";
}

function formatJson(value) {
  return value === undefined || value === null ? "none" : JSON.stringify(value);
}

function synchronizeAttachment(state, projection) {
  if (!state.attachment || !projection) return;
  const linked = (projection.attachments ?? []).find(
    (attachment) => attachment.id === state.attachment.id,
  );
  if (!linked) return;
  state.attachment.role = linked.role;
  state.attachment.mode = linked.mode;
  state.attachment.mode_tag = modeTag(linked.mode);
}

function applyReplayState(state, replay) {
  const replayIsCurrent =
    Number.isSafeInteger(replay.next_event_sequence) && replay.next_event_sequence >= state.cursor;
  if (Number.isSafeInteger(replay.next_event_sequence)) {
    state.cursor = Math.max(state.cursor, replay.next_event_sequence);
  }
  if (!state.attachment || !replayIsCurrent) return;
  if (typeof replay.commission_role === "string") {
    state.attachment.role = replay.commission_role;
  }
  if (typeof replay.attachment_mode === "string") {
    state.attachment.mode = replay.attachment_mode;
  }
  if (typeof replay.mode_tag === "string") {
    state.attachment.mode_tag = replay.mode_tag;
  }
  if (Array.isArray(replay.missing_capabilities)) {
    state.attachment.missing_capabilities = replay.missing_capabilities;
  }
}

function modeTag(mode) {
  return {
    full: "Tyrion: Full",
    limited: "Tyrion: Limited",
    observer: "Tyrion: Observer",
  }[mode] ?? "Tyrion: Disconnected";
}

function attachmentDetails(state) {
  return {
    attachment: state.attachment,
    commission_id: state.commissionId,
    event_cursor: state.cursor,
  };
}

function failClosed(pi, context, reason) {
  const message = `[Tyrion: Disconnected] Attachment failed: ${reason}`;
  context.ui.setStatus("tyrion", "Tyrion: Disconnected");
  context.ui.setWidget("tyrion", [message]);
  context.ui.notify(message, "error");
  pi.sendMessage({
    customType: "tyrion-commission",
    content: message,
    display: true,
    details: { connected: false },
  });
  process.exitCode = 2;
  context.shutdown();
}

async function sendRequest(socketPath, request) {
  const response = await new Promise((resolve, reject) => {
    const socket = net.createConnection(socketPath);
    let encoded = "";
    let settled = false;
    const timeout = setTimeout(() => finish(new Error("Tyrion daemon request timed out")), REQUEST_TIMEOUT_MS);

    function finish(error, value) {
      if (settled) return;
      settled = true;
      clearTimeout(timeout);
      socket.destroy();
      if (error) reject(error);
      else resolve(value);
    }

    socket.setEncoding("utf8");
    socket.on("connect", () => socket.end(`${JSON.stringify(request)}\n`));
    socket.on("data", (chunk) => {
      encoded += chunk;
      if (Buffer.byteLength(encoded, "utf8") > RESPONSE_LIMIT_BYTES) {
        finish(new Error("Tyrion daemon response exceeded the Pi Entry limit"));
      }
    });
    socket.on("end", () => {
      try {
        finish(undefined, JSON.parse(encoded));
      } catch (_error) {
        finish(new Error("Tyrion daemon returned an invalid response"));
      }
    });
    socket.on("error", (error) => finish(error));
  });

  if (response.protocol_version !== PROTOCOL_VERSION) {
    throw new Error(`Tyrion protocol ${response.protocol_version} is incompatible`);
  }
  if (!response.ok) {
    throw new Error(response.error?.message ?? "Tyrion daemon rejected the request");
  }
  return response.data;
}

function authenticatedRequest(state, command, idempotencyKey, expectedRevision, expectedControlRevision) {
  ensureAttached(state);
  return sendRequest(state.socketPath, {
    protocol_version: PROTOCOL_VERSION,
    attachment_token: state.attachmentToken,
    ...(idempotencyKey ? { idempotency_key: idempotencyKey } : {}),
    ...(expectedRevision === undefined ? {} : { expected_revision: expectedRevision }),
    ...(expectedControlRevision === undefined
      ? {}
      : { expected_control_revision: expectedControlRevision }),
    command,
  });
}

function commissionProjection(data) {
  const projection =
    typeof data?.commission?.id === "string"
      ? data
      : typeof data?.commission?.commission?.id === "string"
        ? data.commission
        : undefined;
  if (!projection) {
    throw new Error("Tyrion daemon returned an incomplete Commission projection");
  }
  return projection;
}

function updateCursorFromCommission(state, projection) {
  const events = projection.events;
  if (!Array.isArray(events) || events.length === 0) return;
  const lastSequence = events.at(-1)?.sequence;
  if (Number.isSafeInteger(lastSequence)) state.cursor = Math.max(state.cursor, lastSequence);
}

function errorMessage(error) {
  return error instanceof Error ? error.message : String(error);
}
