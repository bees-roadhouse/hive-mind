import { randomBytes } from 'node:crypto';

import { test as base } from '@playwright/test';

import { type Daemon, startDaemon } from './daemon';
import { createStore, EventWriter, type Store } from './db';

export {
  collectSSE,
  openSameOriginPage,
  resetSSEOpenCount,
  waitForSSEOpen,
  type CollectOptions,
  type SSEEvent,
} from './sse';
export type { Daemon } from './daemon';
export { EventWriter } from './db';

interface WorkerFixtures {
  /**
   * A store directory of this worker's own. The daemon migrates into it, so
   * workers never see each other's events ... which matters because these specs
   * assert on what a stream did NOT deliver.
   */
  store: Store;

  /**
   * A running daemon on an ephemeral port, one per worker, torn down when the
   * worker ends. Worker-scoped on purpose: booting per test would dominate the
   * runtime. The day a spec needs a daemon of its own, give it a test-scoped
   * fixture rather than making every spec pay for a restart.
   */
  daemon: Daemon;
}

interface TestFixtures {
  /**
   * Appends events straight to the store file, the way any other writer
   * would. The daemon is the thing under test, so the events it streams
   * should not come from the daemon.
   */
  events: EventWriter;
  /** Auto-used: attaches the daemon's own log to any test that failed. */
  daemonLogs: void;
}

export const test = base.extend<TestFixtures, WorkerFixtures>({
  store: [
    async ({}, use) => {
      const store = await createStore();
      try {
        await use(store);
      } finally {
        await store.drop();
      }
    },
    { scope: 'worker' },
  ],

  daemon: [
    async ({ store }, use) => {
      const daemon = await startDaemon({
        dataDir: store.dir,
        token: `e2e-${randomBytes(16).toString('hex')}`,
      });
      try {
        await use(daemon);
      } finally {
        await daemon.stop();
      }
    },
    { scope: 'worker' },
  ],

  events: async ({ store, daemon }, use) => {
    // Depends on daemon so the root actor exists: the daemon bootstraps it.
    void daemon;
    const writer = await EventWriter.connect(store);
    try {
      await use(writer);
    } finally {
      await writer.close();
    }
  },

  daemonLogs: [
    async ({ daemon }, use, testInfo) => {
      const before = daemon.logs().length;
      await use();
      if (testInfo.status === testInfo.expectedStatus) {
        return;
      }
      // A 500 explains itself in the daemon's log, not in the assertion.
      await testInfo.attach('daemon.log', {
        body: daemon.logs().slice(before),
        contentType: 'text/plain',
      });
    },
    { auto: true },
  ],
});

export { expect } from '@playwright/test';
