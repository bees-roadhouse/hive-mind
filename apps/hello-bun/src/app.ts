// The reference process app (D53 §3, scenario 1): the smallest real Bun app
// on @hive/app, and the checked-in acceptance test of the runtime (D24's
// rule for guests, kept for processes: the app tests the host). A tool, a
// route, a subscription and a document collection, as the manifest beside
// this file declares them.

import { AppError, Host, defineApp, type Context } from "@hive/app";

const APP = "hello_bun";

/** The host, made on first use so the app starts (and answers /ready)
 *  before the daemon's socket is needed. */
let host: Host | undefined;
function hive(): Host {
  host ??= new Host();
  return host;
}

function sandbox(ctx: Context) {
  return {
    install: ctx.install,
    caller: ctx.caller,
    trust: ctx.trust,
    has_credential: Boolean(process.env.HIVE_APP_CREDENTIAL),
    entry: process.env.HIVE_APP_ENTRY ?? null,
    bun: Bun.version,
  };
}

defineApp({
  tools: {
    add: (data) => {
      const { a, b } = (data ?? {}) as { a?: unknown; b?: unknown };
      if (typeof a !== "number" || typeof b !== "number") {
        throw new AppError("add wants {a: number, b: number}", 400);
      }
      return { sum: a + b };
    },
    whoami: (_data, ctx) => sandbox(ctx),
    // The write that scenario 1 asserts: it goes through the host with the
    // install's credential, so the row lands with the install's actor as
    // author and the owner as principal, pinned by the host and not by
    // anything this file says.
    remember: async (data) => {
      const { title } = (data ?? {}) as { title?: unknown };
      if (typeof title !== "string" || title.length === 0) {
        throw new AppError("remember wants {title: string}", 400);
      }
      const created = await hive().collection(APP, "notes").create({ title });
      return created.data;
    },
  },
  routes: {
    greet: (data) => {
      const { params } = (data ?? {}) as { params?: { name?: string } };
      return { greeting: `hello, ${params?.name ?? "there"}` };
    },
  },
  on: {
    "hello_bun.notes.created": (data) => ({ acknowledged: data }),
  },
});
