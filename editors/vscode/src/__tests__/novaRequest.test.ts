import { describe, expect, it, vi } from 'vitest';

import { isRequestCancelledError, sendRequestWithOptionalToken } from '../novaRequest';

describe('sendRequestWithOptionalToken', () => {
  it('forwards the token when params are provided', async () => {
    const token = { isCancellationRequested: false };
    const sendRequest = vi.fn(async () => 'ok');

    await sendRequestWithOptionalToken({ sendRequest }, 'nova/test/run', { projectRoot: '/tmp' }, token);

    expect(sendRequest).toHaveBeenCalledWith('nova/test/run', { projectRoot: '/tmp' }, token);
  });

  it('forwards the token as the third arg when params are undefined', async () => {
    const token = { isCancellationRequested: false };
    const sendRequest = vi.fn(async () => 'ok');

    await sendRequestWithOptionalToken({ sendRequest }, 'nova/test/discover', undefined, token);

    expect(sendRequest).toHaveBeenCalledWith('nova/test/discover', token);
  });

  it('omits the token argument when not provided', async () => {
    const sendRequest = vi.fn(async () => 'ok');

    await sendRequestWithOptionalToken({ sendRequest }, 'nova/test/discover');

    expect(sendRequest).toHaveBeenCalledWith('nova/test/discover');
  });
});

describe('isRequestCancelledError', () => {
  it('detects JSON-RPC RequestCancelled (-32800)', () => {
    expect(isRequestCancelledError({ code: -32800, message: 'RequestCancelled' })).toBe(true);
  });

  it('detects a RequestCancelled message', () => {
    expect(isRequestCancelledError({ message: 'RequestCancelled' })).toBe(true);
  });

  it('detects \"Request cancelled\" message variants', () => {
    expect(isRequestCancelledError({ message: 'Request cancelled' })).toBe(true);
    expect(isRequestCancelledError({ message: 'Request canceled' })).toBe(true);
  });
});
