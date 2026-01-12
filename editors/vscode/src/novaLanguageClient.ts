import { ExecuteCommandRequest } from 'vscode-languageserver-protocol';
import { LanguageClient } from 'vscode-languageclient/node';

/**
 * A `LanguageClient` that skips vscode-languageclient's built-in ExecuteCommand feature.
 *
 * Nova registers local VS Code command handlers for the server-advertised command IDs
 * (`nova.runTest`, `nova.runMain`, etc.) so code lenses can provide a richer UX than
 * forwarding raw `workspace/executeCommand` results to VS Code.
 *
 * vscode-languageclient's default `ExecuteCommandFeature` also tries to register those command
 * IDs, which can crash on startup due to duplicate registrations. Skipping the feature avoids
 * the conflict and leaves command ownership to the extension.
 */
export class NovaLanguageClient extends LanguageClient {
  override registerFeature(feature: any): void {
    const method = feature?.registrationType?.method;
    if (method === ExecuteCommandRequest.type.method) {
      return;
    }

    super.registerFeature(feature);
  }
}

