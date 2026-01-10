import * as vscode from 'vscode';
import { LanguageClient, type LanguageClientOptions, type ServerOptions } from 'vscode-languageclient/node';

let client: LanguageClient | undefined;
let clientStart: Promise<void> | undefined;

export function activate(context: vscode.ExtensionContext) {
  const serverOptions: ServerOptions = {
    command: 'nova-lsp',
    args: ['--stdio'],
  };

  const clientOptions: LanguageClientOptions = {
    documentSelector: [
      { scheme: 'file', language: 'java' },
      { scheme: 'untitled', language: 'java' },
    ],
    synchronize: {
      fileEvents: vscode.workspace.createFileSystemWatcher('**/*.java'),
    },
  };

  client = new LanguageClient('nova', 'Nova Java Language Server', serverOptions, clientOptions);
  // vscode-languageclient v9+ starts asynchronously.
  clientStart = client.start();
  clientStart.catch((err) => {
    const message = err instanceof Error ? err.message : String(err);
    void vscode.window.showErrorMessage(`Nova: failed to start nova-lsp: ${message}`);
  });

  // Ensure the client is stopped when the extension is deactivated.
  context.subscriptions.push(client);

  context.subscriptions.push(
    vscode.commands.registerCommand('nova.organizeImports', async () => {
      const editor = vscode.window.activeTextEditor;
      if (!editor || editor.document.languageId !== 'java') {
        vscode.window.showInformationMessage('Nova: Open a Java file to organize imports.');
        return;
      }

      if (!client) {
        vscode.window.showErrorMessage('Nova: language client is not running.');
        return;
      }

      try {
        await clientStart;
        await client.sendRequest('nova/java/organizeImports', {
          uri: editor.document.uri.toString(),
        });
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        vscode.window.showErrorMessage(`Nova: organize imports failed: ${message}`);
      }
    }),
  );
}

export function deactivate(): Thenable<void> | undefined {
  if (!client) {
    return undefined;
  }

  return client.stop();
}
