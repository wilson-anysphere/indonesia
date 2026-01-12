export interface CancellationTokenLike {
  readonly isCancellationRequested: boolean;
}

export interface SendRequestClient {
  // Use `any` for broad compatibility with vscode-languageclient's overloaded `sendRequest` signature.
  sendRequest: <R>(method: any, params?: any, token?: any) => Promise<R>;
}

export function isRequestCancelledError(err: unknown): boolean {
  const matchesCancellationMessage = (value: string): boolean => {
    const lower = value.toLowerCase();
    return (
      lower.includes('requestcancelled') ||
      lower.includes('request cancelled') ||
      lower.includes('request canceled')
    );
  };

  if (typeof err === 'string') {
    return matchesCancellationMessage(err);
  }

  if (!err || typeof err !== 'object') {
    return false;
  }

  const code = (err as { code?: unknown }).code;
  if (code === -32800) {
    return true;
  }

  const message = (err as { message?: unknown }).message;
  return typeof message === 'string' && matchesCancellationMessage(message);
}

/**
 * Calls a `sendRequest`-shaped client, forwarding an optional cancellation token as the third
 * argument (matching vscode-languageclient's `sendRequest(method, params, token)` overload).
 *
 * This helper intentionally has **no** top-level dependency on the `vscode` module so it can be
 * unit tested in plain Node.
 */
export async function sendRequestWithOptionalToken<R>(
  client: SendRequestClient,
  method: string,
  params?: unknown,
  token?: unknown,
): Promise<R> {
  if (typeof params === 'undefined') {
    if (typeof token === 'undefined') {
      return await client.sendRequest<R>(method);
    }
    // Pass `undefined` explicitly to avoid treating the token as the `params` argument.
    return await client.sendRequest<R>(method, undefined, token);
  }

  if (typeof token === 'undefined') {
    return await client.sendRequest<R>(method, params);
  }

  return await client.sendRequest<R>(method, params, token);
}
