import { describe, expect, it, vi } from 'vitest';

import { sendRequestWithOptionalToken } from '../novaRequest';

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

    expect(sendRequest).toHaveBeenCalledWith('nova/test/discover', undefined, token);
  });

  it('omits the token argument when not provided', async () => {
    const sendRequest = vi.fn(async () => 'ok');

    await sendRequestWithOptionalToken({ sendRequest }, 'nova/test/discover');

    expect(sendRequest).toHaveBeenCalledWith('nova/test/discover');
  });
});

