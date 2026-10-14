// The wire between the host and a process app (D53 §1), as hive-apps'
// protocol.rs defines it. These types mirror that file field for field; a
// change there is a change here, and the Rust side's unit tests on the
// envelope are the contract's tests. The SDK adds nothing to it.

export type InvokeKind = "tool" | "route" | "event" | "cron";

export type Trust = "trusted" | "untrusted";

export interface CallerFacts {
  /** Who authored the request. May be an AI acting for the principal. */
  actor: string;
  principal_kind: "user" | "org";
  principal_id: string;
}

export interface RouteFacts {
  method: string;
  path: string;
  query?: string;
}

/** What the host sends to POST /invoke. */
export interface InvokeRequest {
  kind: InvokeKind;
  /** The tool, route function, subscription or cron entry the manifest named. */
  name: string;
  install: string;
  caller: CallerFacts;
  trust: Trust;
  data: unknown;
  route?: RouteFacts;
}

/** What the app answers with 200. The host folds `trust`: it may only lower
 *  it, and a missing or unknown word is read as untrusted (invariant 12). */
export interface InvokeResponse {
  trust: Trust;
  data: unknown;
}

export const INVOKE_PATH = "/invoke";
export const READY_PATH = "/ready";

/** The variables the launcher sets (hive-harness app.rs). */
export const ENV = {
  socket: "HIVE_APP_SOCKET",
  entry: "HIVE_APP_ENTRY",
  install: "HIVE_APP_INSTALL",
  credential: "HIVE_APP_CREDENTIAL",
  daemonSocket: "HIVE_SANDBOX_API_SOCKET",
} as const;
