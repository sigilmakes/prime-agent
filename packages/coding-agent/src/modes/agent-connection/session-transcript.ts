import type { AgentMessage } from "@earendil-works/pi-agent-core";
import type { KernelSentAgentMessage } from "../../core/kernel/index.js";
import {
	createBranchSummaryMessage,
	createCompactionSummaryMessage,
	createCustomMessage,
} from "../../core/messages.js";
import type {
	AgentConnectionSessionContext,
	AgentConnectionSessionEntry,
	AgentConnectionSessionTreeNode,
	AgentConnectionSnapshot,
} from "./types.js";

export function sessionContextFromSnapshot(snapshot: AgentConnectionSnapshot): AgentConnectionSessionContext {
	return (
		snapshot.sessionContext ?? {
			messages: snapshot.messages,
			thinkingLevel: snapshot.state.thinkingLevel,
			serviceTier: snapshot.state.serviceTier,
			model: snapshot.state.model
				? { provider: snapshot.state.model.provider, modelId: snapshot.state.model.id }
				: null,
		}
	);
}

function record(value: unknown): value is Record<string, unknown> {
	return typeof value === "object" && value !== null && !Array.isArray(value);
}

function receipt(value: unknown): { toolCallId: string; message: KernelSentAgentMessage } | undefined {
	if (!record(value) || typeof value.toolCallId !== "string" || !record(value.message)) return undefined;
	const { id, message, deliveryStatus, target } = value.message;
	if (
		typeof id !== "string" ||
		typeof message !== "string" ||
		(deliveryStatus !== "delivered" && deliveryStatus !== "queued") ||
		!record(target) ||
		typeof target.activeSessionId !== "string" ||
		typeof target.sessionId !== "string"
	)
		return undefined;
	return {
		toolCallId: value.toolCallId,
		message: {
			id,
			message,
			deliveryStatus,
			target: {
				activeSessionId: target.activeSessionId,
				sessionId: target.sessionId,
				...(typeof target.sessionName === "string" ? { sessionName: target.sessionName } : {}),
			},
		},
	};
}

function messageKey(message: AgentMessage): string {
	if (message.role === "toolResult") return `tool:${message.toolCallId}`;
	if (message.role === "custom")
		return JSON.stringify([message.role, message.customType, message.timestamp, message.content, message.details]);
	if (message.role === "compactionSummary") return JSON.stringify([message.role, message.timestamp, message.summary]);
	if (message.role === "branchSummary")
		return JSON.stringify([message.role, message.timestamp, message.fromId, message.summary]);
	return JSON.stringify([message.role, message.timestamp, message]);
}

/** Display-only projection. Never use this to assemble a model prompt. */
export function buildSessionTranscript(
	tree: readonly AgentConnectionSessionTreeNode[],
	leafId: string | null,
	context: AgentConnectionSessionContext,
	allowCustomTimestampDrift = false,
): AgentConnectionSessionContext {
	const byId = new Map<string, AgentConnectionSessionEntry>();
	const pending = [...tree];
	while (pending.length > 0) {
		const node = pending.pop()!;
		if (byId.has(node.entry.id)) continue;
		byId.set(node.entry.id, node.entry);
		for (const child of node.children) pending.push(child);
	}
	const branch: AgentConnectionSessionEntry[] = [];
	const seen = new Set<string>();
	let id = leafId;
	while (id !== null) {
		if (seen.has(id)) throw new Error("Session transcript has a cyclic branch");
		seen.add(id);
		const entry = byId.get(id);
		if (!entry) throw new Error("Session transcript snapshot is no longer available");
		branch.push(entry);
		id = entry.parentId;
	}
	branch.reverse();
	let modelStart = 0;
	for (let index = branch.length - 1; index >= 0; index--) {
		const entry = branch[index]!;
		if (entry.type !== "compaction") continue;
		const retainedStart = branch.findIndex((candidate) => candidate.id === entry.firstKeptEntryId);
		modelStart = retainedStart >= 0 && retainedStart < index ? retainedStart : index;
		break;
	}
	const messages: AgentMessage[] = [];
	const modelIndices = new Set<number>();
	const receipts: Array<{ toolCallId: string; message: KernelSentAgentMessage }> = [];
	for (const [branchIndex, entry] of branch.entries()) {
		const start = messages.length;
		switch (entry.type) {
			case "message":
				if (entry.message.role !== "custom" || entry.message.display) messages.push(entry.message);
				break;
			case "custom_message":
				if (entry.display)
					messages.push(
						createCustomMessage(entry.customType, entry.content, true, entry.details, entry.timestamp),
					);
				break;
			case "compaction":
				messages.push(
					createCompactionSummaryMessage(
						entry.summary,
						entry.tokensBefore,
						entry.timestamp,
						entry.customInstructions,
					),
				);
				break;
			case "branch_summary":
				if (entry.summary) messages.push(createBranchSummaryMessage(entry.summary, entry.fromId, entry.timestamp));
				break;
			case "custom": {
				const sent = entry.customType === "ipython_sent_agent_message" ? receipt(entry.data) : undefined;
				if (sent) receipts.push(sent);
				break;
			}
		}
		if (branchIndex >= modelStart) {
			for (let index = start; index < messages.length; index++) modelIndices.add(index);
		}
	}

	// Only model-eligible occurrences can match the compacted live context.
	// Compacted-away duplicates must not swallow an unpersisted latest outcome.
	const indices = new Map<string, number[]>();
	const customIndices = new Map<string, number[]>();
	const customKey = (message: Extract<AgentMessage, { role: "custom" }>) =>
		JSON.stringify([message.customType, message.content, message.details]);
	for (const [index, message] of messages.entries()) {
		if (!modelIndices.has(index)) continue;
		const key = messageKey(message);
		const queue = indices.get(key) ?? [];
		queue.push(index);
		indices.set(key, queue);
		if (allowCustomTimestampDrift && message.role === "custom") {
			const key = customKey(message);
			const queue = customIndices.get(key) ?? [];
			queue.push(index);
			customIndices.set(key, queue);
		}
	}
	for (const queue of indices.values()) queue.reverse();
	for (const queue of customIndices.values()) queue.reverse();
	const consumed = new Set<number>();
	const take = (queue: number[] | undefined): number | undefined => {
		while (queue && queue.length > 0) {
			const index = queue.pop()!;
			if (!consumed.has(index)) return index;
		}
		return undefined;
	};
	const unsaved: AgentMessage[] = [];
	for (const message of context.messages) {
		if (message.role === "custom" && !message.display) continue;
		let index = take(indices.get(messageKey(message)));
		// Raw runtime custom messages use creation time, while saved entries use
		// append time. Canonical sessionContext messages need no normalization.
		if (index === undefined && allowCustomTimestampDrift && message.role === "custom") {
			index = take(customIndices.get(customKey(message)));
		}
		if (index !== undefined) {
			consumed.add(index);
			// Live tool results contain receipts that may not yet have been persisted.
			if (message.role === "toolResult") messages[index] = message;
		} else if (message.role !== "compactionSummary" && message.role !== "branchSummary") {
			unsaved.push(message);
		}
	}
	for (const message of unsaved) {
		let index = messages.length;
		while (index > 0 && messages[index - 1]!.timestamp > message.timestamp) index--;
		messages.splice(index, 0, message);
	}
	const receiptsByCall = new Map<string, KernelSentAgentMessage[]>();
	for (const sent of receipts) {
		const list = receiptsByCall.get(sent.toolCallId) ?? [];
		list.push(sent.message);
		receiptsByCall.set(sent.toolCallId, list);
	}
	for (const [index, message] of messages.entries()) {
		if (message.role !== "toolResult" || message.toolName !== "ipython") continue;
		const sent = receiptsByCall.get(message.toolCallId);
		if (!sent) continue;
		const details = record(message.details) ? message.details : {};
		const current: unknown[] = Array.isArray(details.sentAgentMessages) ? [...details.sentAgentMessages] : [];
		for (const item of sent) {
			if (!current.some((existing) => record(existing) && existing.id === item.id)) current.push(item);
		}
		messages[index] = { ...message, details: { ...details, sentAgentMessages: current } };
	}
	return { ...context, messages };
}
