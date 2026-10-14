// The app's side of the dispatch, driven without a socket: `handle` is what
// `defineApp` puts behind Bun.serve, so what is asserted here is the
// contract an app author sees, and what the host's dispatcher (hive-apps)
// expects back.

import { describe, expect, test } from "bun:test";
import { AppError, handle, untrusted, type AppDefinition, type InvokeRequest } from "../src/index";

const caller = { actor: "11111111-1111-1111-1111-111111111111", principal_kind: "user" as const, principal_id: "11111111-1111-1111-1111-111111111111" };

function invoke(body: Partial<InvokeRequest> & { kind: InvokeRequest["kind"]; name: string }): Request {
  const req: InvokeRequest = {
    install: "22222222-2222-2222-2222-222222222222",
    caller,
    trust: "trusted",
    data: null,
    ...body,
  };
  return new Request("http://app/invoke", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(req),
  });
}

const app: AppDefinition = {
  tools: {
    add: (data) => {
      const { a, b } = data as { a: number; b: number };
      return { sum: a + b };
    },
    whoami: (_data, ctx) => ({ kind: ctx.kind, name: ctx.name, actor: ctx.caller.actor, trust: ctx.trust, install: ctx.install }),
    fetched: () => untrusted({ title: "from the web" }),
    refuse: () => {
      throw new AppError("no such thing", 404);
    },
    explode: () => {
      throw new Error("a bug");
    },
    nothing: () => undefined,
  },
  routes: {
    hello: (_data, ctx) => ({ method: ctx.route?.method, path: ctx.route?.path }),
  },
  on: {
    "journal.entry.created": (data) => ({ saw: data }),
  },
};

describe("handle", () => {
  test("/ready answers while handlers are registered", async () => {
    const res = await handle(app, new Request("http://app/ready"));
    expect(res.status).toBe(200);
  });

  test("a tool call reaches its handler and the answer is the envelope, trusted by default", async () => {
    const res = await handle(app, invoke({ kind: "tool", name: "add", data: { a: 2, b: 3 } }));
    expect(res.status).toBe(200);
    expect(await res.json()).toEqual({ trust: "trusted", data: { sum: 5 } });
  });

  test("the context carries who called, which install, and the input's trust", async () => {
    const res = await handle(app, invoke({ kind: "tool", name: "whoami", trust: "untrusted" }));
    const body = (await res.json()) as { data: Record<string, unknown> };
    expect(body.data).toEqual({
      kind: "tool",
      name: "whoami",
      actor: caller.actor,
      trust: "untrusted",
      install: "22222222-2222-2222-2222-222222222222",
    });
  });

  test("untrusted() lowers the answer's trust", async () => {
    const res = await handle(app, invoke({ kind: "tool", name: "fetched" }));
    expect(await res.json()).toEqual({ trust: "untrusted", data: { title: "from the web" } });
  });

  test("a handler that returns nothing answers null, still an envelope", async () => {
    const res = await handle(app, invoke({ kind: "tool", name: "nothing" }));
    expect(await res.json()).toEqual({ trust: "trusted", data: null });
  });

  test("routes get the HTTP facts; events get their data", async () => {
    const res = await handle(
      app,
      invoke({ kind: "route", name: "hello", route: { method: "GET", path: "/hello" } }),
    );
    expect((await res.json()).data).toEqual({ method: "GET", path: "/hello" });
    const ev = await handle(app, invoke({ kind: "event", name: "journal.entry.created", data: { id: 7 } }));
    expect((await ev.json()).data).toEqual({ saw: { id: 7 } });
  });

  test("an unknown handler is 404 and names the kind and the name", async () => {
    const res = await handle(app, invoke({ kind: "tool", name: "missing" }));
    expect(res.status).toBe(404);
    expect((await res.json()).error).toContain('no tool named "missing"');
    const wrongKind = await handle(app, invoke({ kind: "cron", name: "add" }));
    expect(wrongKind.status).toBe(404);
  });

  test("an AppError is its status and message; any other throw is a 500 with the message", async () => {
    const res = await handle(app, invoke({ kind: "tool", name: "refuse" }));
    expect(res.status).toBe(404);
    expect((await res.json()).error).toBe("no such thing");
    const boom = await handle(app, invoke({ kind: "tool", name: "explode" }));
    expect(boom.status).toBe(500);
    expect((await boom.json()).error).toBe("a bug");
  });

  test("a body that is not the envelope is 400, and other paths are 404", async () => {
    const bad = await handle(
      app,
      new Request("http://app/invoke", { method: "POST", body: "not json" }),
    );
    expect(bad.status).toBe(400);
    const nope = await handle(app, new Request("http://app/elsewhere"));
    expect(nope.status).toBe(404);
  });
});
