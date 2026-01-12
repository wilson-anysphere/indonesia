import * as vscode from 'vscode';

export type NovaRequest = <R>(method: string, params?: unknown) => Promise<R | undefined>;

const SHOW_METRICS_COMMAND = 'nova.showRequestMetrics';
const RESET_METRICS_COMMAND = 'nova.resetRequestMetrics';

export function registerNovaMetricsCommands(context: vscode.ExtensionContext, request: NovaRequest): void {
  const output = vscode.window.createOutputChannel('Nova Metrics');
  context.subscriptions.push(output);

  context.subscriptions.push(
    vscode.commands.registerCommand(SHOW_METRICS_COMMAND, async () => {
      try {
        const metrics = await request<unknown>('nova/metrics');
        if (typeof metrics === 'undefined') {
          // Request was gated (unsupported method) and the shared request helper already displayed
          // a user-facing message.
          return;
        }

        const metricsJson = jsonStringifyBestEffort(metrics);
        output.clear();
        output.appendLine(`[${new Date().toISOString()}] nova/metrics`);
        output.appendLine(metricsJson);
        output.show(true);

        const choice = await vscode.window.showInformationMessage('Nova: Request metrics captured.', 'Copy JSON to Clipboard');
        if (choice === 'Copy JSON to Clipboard') {
          try {
            await vscode.env.clipboard.writeText(metricsJson);
            void vscode.window.showInformationMessage('Nova: Request metrics copied to clipboard.');
          } catch {
            // Best-effort: clipboard might be unavailable in some remote contexts.
            void vscode.window.showWarningMessage('Nova: Failed to copy request metrics to clipboard.');
          }
        }
      } catch (err) {
        const message = formatError(err);
        void vscode.window.showErrorMessage(`Nova: failed to fetch request metrics: ${message}`);
      }
    }),
  );

  context.subscriptions.push(
    vscode.commands.registerCommand(RESET_METRICS_COMMAND, async () => {
      try {
        const resp = await request<unknown>('nova/resetMetrics');
        if (typeof resp === 'undefined') {
          // Request was gated (unsupported method) and the shared request helper already displayed
          // a user-facing message.
          return;
        }
        void vscode.window.showInformationMessage('Nova: Request metrics reset.');
      } catch (err) {
        const message = formatError(err);
        void vscode.window.showErrorMessage(`Nova: failed to reset request metrics: ${message}`);
      }
    }),
  );
}

function jsonStringifyBestEffort(value: unknown): string {
  try {
    const serialized = JSON.stringify(
      value,
      (_key, v) => {
        if (typeof v === 'bigint') {
          return v.toString();
        }
        return v;
      },
      2,
    );
    return typeof serialized === 'string' ? serialized : String(serialized);
  } catch (err) {
    const message = formatError(err);
    return `<< Failed to JSON.stringify metrics: ${message} >>\n${String(value)}`;
  }
}

function formatError(err: unknown): string {
  if (err instanceof Error) {
    return err.message;
  }
  if (typeof err === 'string') {
    return err;
  }
  if (err && typeof err === 'object' && 'message' in err && typeof (err as { message: unknown }).message === 'string') {
    return (err as { message: string }).message;
  }
  try {
    return JSON.stringify(err);
  } catch {
    return String(err);
  }
}
