/** Credential-free result of one explicitly requested model refresh. */
export interface ModelRefreshSourceResult {
	source: string;
	status: "updated" | "unchanged" | "skipped" | "stale" | "failed";
	modelCount?: number;
	reason?:
		| "offline"
		| "unconfigured"
		| "auth-rejected"
		| "network"
		| "invalid-catalog"
		| "cancelled"
		| "superseded"
		| "unsupported"
		| "cache-write"
		| "worker-unavailable";
}

export interface ModelRefreshReport {
	startedAt: number;
	finishedAt: number;
	sources: ModelRefreshSourceResult[];
	workers?: { total: number; succeeded: number; failed: number };
}

/** Sources consumed by a resident worker from validated local cache snapshots. */
export interface ModelRefreshApplyReport {
	applied: string[];
	skipped: string[];
	missing: string[];
}
