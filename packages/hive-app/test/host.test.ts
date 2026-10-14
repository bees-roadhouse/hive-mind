// The app's side of the API, against a fake daemon that answers the MCP
// route the way hive-httpapi's mcp.rs does (content text, structuredContent,
// isError, _meta.trust). What is asserted: the bearer is the install
// credential and nothing else, the JSON-RPC shape, the fold of the host's
// trust word, the 401 after revocation, and the collection verbs' tool names
// and argument shapes (hive-manifest's qualified_tool_name and crud_schema).

import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { AppError, Host } from "../src/index";

interface Seen {
  auth: string | null;
  body: { jsonrpc: string; id: number; method: string; params: unknown };
}

const seen: Seen[] = [];
let revoked = false;
let server: ReturnType<typeof Bun.serve>;

beforeAll(() => {
  server = Bun.serve({
    port: 0,
    hostname: "127.0.0.1",
    fetch: async (req) => {
      const url = new URL(req.url);
      if (url.pathname !== "/mcp" || req.method !== "POST") {
        return new Response("not found", { status: 404 });
      }
      const auth = req.headers.get("authorization");
      const body = (await req.json()) as Seen["body"];
      seen.push({ auth, body });
      if (revoked || auth !== "Bearer hs_test_token") {
        return new Response(JSON.stringify({ error: "unauthenticated" }), { status: 401 });
      }
      const result = (r: unknown) => Response.json({ jsonrpc: "2.0", id: body.id, result: r });
      if (body.method === "tools/list") {
        return result({
          tools: [{ name: "notes.items.list", description: "List items", inputSchema: { type: "object" } }],
        });
      }
      if (body.method === "tools/call") {
        const p = body.params as { name: string; arguments: Record<string, unknown> };
        if (p.name === "notes.items.create") {
          return result({
            content: [{ type: "text", text: JSON.stringify({ id: "doc-1", doc: p.arguments.doc }) }],
            structuredContent: { id: "doc-1", doc: p.arguments.doc },
            isError: false,
            _meta: { trust: "trusted", taintedBy: "" },
          });
        }
        if (p.name === "notes.items.get") {
          return result({
            content: [{ type: "text", text: JSON.stringify({ id: p.arguments.id, title: "from a feed" }) }],
            isError: false,
            _meta: { trust: "untrusted", taintedBy: "fetch" },
          });
        }
        if (p.name === "notes.items.delete") {
          return result({ content: [{ type: "text", text: "not yours" }], isError: true });
        }
        return Response.json({ jsonrpc: "2.0", id: body.id, error: { code: -32602, message: `unknown tool: ${p.name}` } });
      }
      return Response.json({ jsonrpc: "2.0", id: body.id, error: { code: -32601, message: "method not found" } });
    },
  });
});

afterAll(() => server.stop(true));

function host(): Host {
  return new Host({ url: `http://127.0.0.1:${server.port}`, credential: "hs_test_token" });
}

describe("Host", () => {
  test("the credential is the bearer and the call is JSON-RPC 2.0 to /mcp", async () => {
    const tools = await host().tools();
    expect(tools.map((t) => t.name)).toEqual(["notes.items.list"]);
    const last = seen.at(-1)!;
    expect(last.auth).toBe("Bearer hs_test_token");
    expect(last.body.jsonrpc).toBe("2.0");
    expect(last.body.method).toBe("tools/list");
  });

  test("a collection's verbs are the generated tools with their argument shapes", async () => {
    const items = host().collection("notes", "items");
    const created = await items.create({ title: "x" });
    expect(created).toEqual({ trust: "trusted", data: { id: "doc-1", doc: { title: "x" } } });
    const call = seen.at(-1)!.body.params as { name: string; arguments: unknown };
    expect(call.name).toBe("notes.items.create");
    expect(call.arguments).toEqual({ doc: { title: "x" } });
  });

  test("the host's trust word is read as it is and anything else is untrusted", async () => {
    const got = await host().collection("notes", "items").get("doc-2");
    expect(got.trust).toBe("untrusted");
    expect(got.data).toEqual({ id: "doc-2", title: "from a feed" });
  });

  test("a tool refusal is an error with the host's message, and an unknown tool names itself", async () => {
    await expect(host().collection("notes", "items").delete("doc-1")).rejects.toThrow("not yours");
    await expect(host().call("notes.nope", {})).rejects.toThrow("unknown tool: notes.nope");
  });

  test("a revoked credential is the one 401, named", async () => {
    revoked = true;
    try {
      const err = await host()
        .tools()
        .then(() => null, (e: unknown) => e);
      expect(err).toBeInstanceOf(AppError);
      expect((err as AppError).status).toBe(401);
      expect((err as AppError).message).toContain("refused");
    } finally {
      revoked = false;
    }
  });

  test("without a credential there is no Host", () => {
    const saved = process.env.HIVE_APP_CREDENTIAL;
    delete process.env.HIVE_APP_CREDENTIAL;
    try {
      expect(() => new Host({ url: "http://127.0.0.1:1" })).toThrow("HIVE_APP_CREDENTIAL is not set");
    } finally {
      if (saved !== undefined) process.env.HIVE_APP_CREDENTIAL = saved;
    }
  });
});
