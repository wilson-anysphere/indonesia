import assert from 'node:assert/strict';
import test from 'node:test';
import { requestMoreCompletions, type CancellationTokenLike, type MoreCompletionsClient } from '../aiCompletionMore';

function createCancellationTokenSource(): { token: CancellationTokenLike; cancel: () => void } {
  let isCancelled = false;
  const listeners = new Set<() => void>();

  const token: CancellationTokenLike = {
    get isCancellationRequested() {
      return isCancelled;
    },
    onCancellationRequested(listener: () => void) {
      listeners.add(listener);
      return {
        dispose() {
          listeners.delete(listener);
        },
      };
    },
  };

  return {
    token,
    cancel() {
      if (isCancelled) {
        return;
      }
      isCancelled = true;
      for (const listener of Array.from(listeners)) {
        listener();
      }
    },
  };
}

type FakeCompletionItem = { label: string };

function createFakeClient(
  responses: Array<{ items: Array<{ label: string }>; is_incomplete: boolean }>,
): { client: MoreCompletionsClient<FakeCompletionItem>; getCallCount: () => number } {
  let calls = 0;

  const client: MoreCompletionsClient<FakeCompletionItem> = {
    async sendRequest<R>() {
      const resp = responses[Math.min(calls, responses.length - 1)];
      calls += 1;
      return resp as unknown as R;
    },
    protocol2CodeConverter: {
      asCompletionItem(item) {
        return { label: item.label };
      },
    },
  };

  return { client, getCallCount: () => calls };
}

const baseCompletionItems = [{ data: { nova: { completion_context_id: 'ctx-1' } } }];

test('requestMoreCompletions polls again when response is incomplete and empty', async () => {
  const { client, getCallCount } = createFakeClient([
    { items: [], is_incomplete: true },
    { items: [{ label: 'AI-1' }], is_incomplete: false },
  ]);

  let nowMs = 0;
  const delays: number[] = [];

  const result = await requestMoreCompletions(client, baseCompletionItems, {
    enabled: true,
    maxItems: 5,
    requestTimeoutMs: 100,
    pollIntervalMs: 50,
    now: () => nowMs,
    sleep: async (ms) => {
      delays.push(ms);
      nowMs += ms;
    },
  });

  assert.equal(getCallCount(), 2);
  assert.deepEqual(delays, [25]);
  assert.deepEqual(result, [{ label: 'AI-1' }]);
});

test('requestMoreCompletions stops polling once items arrive (even if incomplete)', async () => {
  const { client, getCallCount } = createFakeClient([
    { items: [], is_incomplete: true },
    { items: [{ label: 'AI-2' }], is_incomplete: true },
    { items: [{ label: 'AI-3' }], is_incomplete: false },
  ]);

  let nowMs = 0;
  const delays: number[] = [];

  const result = await requestMoreCompletions(client, baseCompletionItems, {
    enabled: true,
    maxItems: 5,
    requestTimeoutMs: 100,
    pollIntervalMs: 50,
    now: () => nowMs,
    sleep: async (ms) => {
      delays.push(ms);
      nowMs += ms;
    },
  });

  assert.equal(getCallCount(), 2);
  assert.deepEqual(delays, [25]);
  assert.deepEqual(result, [{ label: 'AI-2' }]);
});

test('requestMoreCompletions stops polling when requestTimeoutMs is reached', async () => {
  const { client, getCallCount } = createFakeClient([{ items: [], is_incomplete: true }]);

  let nowMs = 0;
  const delays: number[] = [];

  const result = await requestMoreCompletions(client, baseCompletionItems, {
    enabled: true,
    maxItems: 5,
    requestTimeoutMs: 100,
    pollIntervalMs: 50,
    now: () => nowMs,
    sleep: async (ms) => {
      delays.push(ms);
      nowMs += ms;
    },
  });

  assert.equal(result, undefined);
  assert.equal(getCallCount(), 3);
  assert.deepEqual(delays, [25, 50, 25]);
});

test('requestMoreCompletions stops polling on cancellation', async () => {
  const { client, getCallCount } = createFakeClient([{ items: [], is_incomplete: true }]);
  const { token, cancel } = createCancellationTokenSource();

  let nowMs = 0;
  const delays: number[] = [];

  const result = await requestMoreCompletions(client, baseCompletionItems, {
    token,
    enabled: true,
    maxItems: 5,
    requestTimeoutMs: 100,
    pollIntervalMs: 50,
    now: () => nowMs,
    sleep: async (ms) => {
      delays.push(ms);
      cancel();
      nowMs += ms;
    },
  });

  assert.equal(result, undefined);
  assert.equal(getCallCount(), 1);
  assert.deepEqual(delays, [25]);
});

