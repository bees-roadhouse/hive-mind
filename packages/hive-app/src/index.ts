// @hive/app (D53 §2): the SDK a process app is written against.
//
// Two halves, the way the runtime has two halves:
//
// - `defineApp` is the app's side of the dispatch. It serves hive-apps'
//   protocol on the one socket the host mounted (HIVE_APP_SOCKET): GET /ready
//   while handlers are registered, POST /invoke to the handler the manifest
//   named. A handler receives the data and a context (who called, the
//   install, the input's trust) and returns data; the SDK wraps it in the
//   envelope. A handler that wants to lower trust returns `untrusted(data)`;
//   nothing a handler does can raise it, and the host folds on its side too
//   (invariant 12), so this is a courtesy, not the enforcement.
// - `hive` is the app's side of the API. It reaches the host over the
//   daemon's socket (HIVE_SANDBOX_API_SOCKET, invariant 13) with the install
//   credential (HIVE_APP_CREDENTIAL) as bearer. Today that surface is the
//   daemon's MCP route: the generated CRUD tools for the app's own
//   collections (hive-manifest's qualified tool names), `tools/call` and
//   `tools/list`; `hive.collection(name)` wraps them. When D52's verbs land as
//   routes, this module swaps a transport and keeps its surface (#168).
//
// What the SDK never does: open a port, write a file it expects to find
// again, or reach anything the daemon's socket does not offer. The container
// has no network and a read-only root, so an app that tries is a loud failure
// rather than a quiet one (D53 §2).

import {
  ENV,
  INVOKE_PATH,
  READY_PATH,
  type CallerFacts,
  type InvokeKind,
  type InvokeRequest,
  type InvokeResponse,
  type RouteFacts,
  type Trust,
} from "./protocol";

export * from "./protocol";

/** What a handler learns about the call besides its data. */
export interface Context {
  kind: InvokeKind;
  name: string;
  install: string;
  caller: CallerFacts;
  /** The input's trust. A handler reading it is reading provenance; it cannot
   *  change what the host will do with the answer. */
  trust: Trust;
  route?: RouteFacts;
}

/** A handler's answer, when it wants to say its trust. Plain data means
 *  "trusted as far as I know", which the host still floors to the input's. */
export class Answer {
  constructor(
    readonly data: unknown,
    readonly trust: Trust,
  ) {}
}

/** Marks an answer as untrusted: content that came from outside, derived
 *  from a fetch, a feed or a model (invariant 9). */
export function untrusted(data: unknown): Answer {
  return new Answer(data, "untrusted");
}

export type Handler = (data: unknown, ctx: Context) => unknown | Answer | Promise<unknown | Answer>;

export interface AppDefinition {
  /** By tool name, as the manifest's `tools[].function`. */
  tools?: Record<string, Handler>;
  /** By route function name, as the manifest's `routes[].function`. */
  routes?: Record<string, Handler>;
  /** By subscription kind, as the manifest's `subscriptions[].kind`. */
  on?: Record<string, Handler>;
  /** By cron entry name. */
  cron?: Record<string, Handler>;
}

export class AppError extends Error {
  constructor(
    message: string,
    readonly status = 500,
  ) {
    super(message);
  }
}

function table(def: AppDefinition, kind: InvokeKind): Record<string, Handler> {
  switch (kind) {
    case "tool":
      return def.tools ?? {};
    case "route":
      return def.routes ?? {};
    case "event":
      return def.on ?? {};
    case "cron":
      return def.cron ?? {};
  }
}

function json(status: number, body: unknown): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

/** Dispatches one protocol request to the app's handlers. Exported so the
 *  tests drive it without a socket; `defineApp` is this behind Bun.serve. */
export async function handle(def: AppDefinition, request: Request): Promise<Response> {
  const url = new URL(request.url);
  if (request.method === "GET" && url.pathname === READY_PATH) {
    return json(200, { ready: true });
  }
  if (request.method !== "POST" || url.pathname !== INVOKE_PATH) {
    return json(404, { error: `no such path ${request.method} ${url.pathname}` });
  }
  let req: InvokeRequest;
  try {
    req = (await request.json()) as InvokeRequest;
  } catch (e) {
    return json(400, { error: `request is not the envelope: ${String(e)}` });
  }
  const handler = table(def, req.kind)[req.name];
  if (!handler) {
    return json(404, { error: `no ${req.kind} named ${JSON.stringify(req.name)}` });
  }
  const ctx: Context = {
    kind: req.kind,
    name: req.name,
    install: req.install,
    caller: req.caller,
    trust: req.trust,
    ...(req.route ? { route: req.route } : {}),
  };
  try {
    const out = await handler(req.data, ctx);
    const body: InvokeResponse =
      out instanceof Answer
        ? { trust: out.trust, data: out.data }
        : { trust: "trusted", data: out === undefined ? null : out };
    return json(200, body);
  } catch (e) {
    if (e instanceof AppError) {
      return json(e.status, { error: e.message });
    }
    return json(500, { error: e instanceof Error ? e.message : String(e) });
  }
}

export interface Served {
  stop(): void;
  readonly socket: string;
}

/** Serves the app on the socket the host mounted and returns a handle. The
 *  path comes from the environment the launcher set; a path argument is for
 *  tests. */
export function defineApp(def: AppDefinition, options: { socket?: string } = {}): Served {
  const socket = options.socket ?? process.env[ENV.socket];
  if (!socket) {
    throw new AppError(`${ENV.socket} is not set; this process is not running under the hive-mind runtime`);
  }
  const server = Bun.serve({
    unix: socket,
    fetch: (request) => handle(def, request),
  });
  return {
    socket,
    stop: () => server.stop(true),
  };
}

// ---------------------------------------------------------------------------
// The host's API, over the daemon's socket.

export interface HostOptions {
  /** The daemon's socket; defaults to HIVE_SANDBOX_API_SOCKET. */
  socket?: string;
  /** A TCP base URL in place of the socket, for tests against a fake daemon
   *  on a box without unix sockets. Production never sets it. */
  url?: string;
  /** The install credential; defaults to HIVE_APP_CREDENTIAL. */
  credential?: string;
}

interface RpcResult {
  jsonrpc: "2.0";
  id: number;
  result?: unknown;
  error?: { code: number; message: string };
}

/** The daemon as an app sees it. */
export class Host {
  private readonly socket: string | undefined;
  private readonly url: string;
  private readonly credential: string;
  private next = 1;

  constructor(options: HostOptions = {}) {
    const credential = options.credential ?? process.env[ENV.credential];
    if (!credential) throw new AppError(`${ENV.credential} is not set`);
    this.credential = credential;
    if (options.url) {
      this.url = options.url.replace(/\/$/, "");
      this.socket = undefined;
    } else {
      const socket = options.socket ?? process.env[ENV.daemonSocket];
      if (!socket) throw new AppError(`${ENV.daemonSocket} is not set`);
      this.socket = socket;
      this.url = "http://hive";
    }
  }

  /** One JSON-RPC call to the daemon's MCP route. */
  async rpc(method: string, params: unknown): Promise<unknown> {
    const id = this.next++;
    const init: RequestInit & { unix?: string } = {
      method: "POST",
      headers: {
        authorization: `Bearer ${this.credential}`,
        "content-type": "application/json",
      },
      body: JSON.stringify({ jsonrpc: "2.0", id, method, params }),
    };
    if (this.socket) init.unix = this.socket;
    const res = await fetch(`${this.url}/mcp`, init);
    if (res.status === 401) {
      throw new AppError("the install credential was refused (revoked, or the container outlived it)", 401);
    }
    if (!res.ok) {
      throw new AppError(`daemon answered ${res.status} to ${method}`, res.status);
    }
    const body = (await res.json()) as RpcResult;
    if (body.error) {
      throw new AppError(`${method}: ${body.error.message}`, 400);
    }
    return body.result;
  }

  /** Every tool this install's credential may call, with its input schema:
   *  the API schema an app's types come from (#168). */
  async tools(): Promise<Array<{ name: string; description: string; inputSchema: unknown }>> {
    const r = (await this.rpc("tools/list", {})) as { tools: Array<{ name: string; description: string; inputSchema: unknown }> };
    return r.tools;
  }

  /** Calls one tool by its qualified name and returns `{trust, data}`. */
  async call(name: string, args: unknown): Promise<{ trust: Trust; data: unknown }> {
    const r = (await this.rpc("tools/call", { name, arguments: args ?? {} })) as {
      content?: Array<{ type: string; text?: string }>;
      structuredContent?: unknown;
      isError?: boolean;
      _meta?: { trust?: string };
    };
    const text = r.content?.find((c) => c.type === "text")?.text ?? "null";
    if (r.isError) {
      throw new AppError(text, 400);
    }
    let data: unknown = r.structuredContent;
    if (data === undefined) {
      try {
        data = JSON.parse(text);
      } catch {
        data = text;
      }
    }
    // The host says the trust and the app reads it; anything but the word
    // `trusted` is untrusted, the same fold the host applies (invariant 12).
    return { trust: r._meta?.trust === "trusted" ? "trusted" : "untrusted", data };
  }

  /** The app's own collection, through its generated CRUD tools. */
  collection(app: string, name: string): Collection {
    return new Collection(this, app, name);
  }
}

/** `hive-manifest`'s generated CRUD tools for one collection, as verbs. The
 *  tool names are `<app>.<collection>.<op>` (qualified_tool_name); the
 *  arguments are the shapes `crud_schema` promises. */
export class Collection {
  constructor(
    private readonly host: Host,
    private readonly app: string,
    private readonly name: string,
  ) {}

  private tool(op: string): string {
    return `${this.app}.${this.name}.${op}`;
  }

  list(args: { limit?: number; cursor?: string; search?: string } = {}) {
    return this.host.call(this.tool("list"), args);
  }
  get(id: string) {
    return this.host.call(this.tool("get"), { id });
  }
  create(doc: unknown) {
    return this.host.call(this.tool("create"), { doc });
  }
  update(id: string, doc: unknown) {
    return this.host.call(this.tool("update"), { id, doc });
  }
  delete(id: string) {
    return this.host.call(this.tool("delete"), { id });
  }
}
