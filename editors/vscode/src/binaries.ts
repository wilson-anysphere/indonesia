import * as vscode from 'vscode';
import * as fs from 'node:fs';
import * as path from 'node:path';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';

const execFileAsync = promisify(execFile);

export type DownloadMode = 'auto' | 'prompt' | 'off';

const VERSION_REGEX = /\b\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?\b/;

export function deriveReleaseUrlFromBaseUrl(downloadBaseUrl: string, fallbackReleaseUrl: string): string {
  const trimmed = downloadBaseUrl.trim().replace(/\/+$/, '');
  if (!trimmed) {
    return fallbackReleaseUrl;
  }

  try {
    const url = new URL(trimmed);
    const suffix = '/releases/download';
    if (url.pathname.endsWith(suffix)) {
      const repoPath = url.pathname.slice(0, -suffix.length);
      return `${url.origin}${repoPath}`;
    }
  } catch {
    // Not a URL; fall through.
  }

  return trimmed;
}

export function getExtensionVersion(context: vscode.ExtensionContext): string {
  try {
    const pkgPath = path.join(context.extensionPath, 'package.json');
    const pkg = JSON.parse(fs.readFileSync(pkgPath, 'utf8')) as { version?: string };
    return pkg.version ?? '0.0.0';
  } catch {
    return '0.0.0';
  }
}

export async function getBinaryVersion(executablePath: string): Promise<string> {
  const { stdout, stderr } = await execFileAsync(executablePath, ['--version'], {
    windowsHide: true,
    timeout: 10_000,
  });

  const output = [stdout, stderr].filter((value) => value && value.trim() !== '').join('\n').trim();
  const match = output.match(VERSION_REGEX);
  if (!match) {
    throw new Error(`unable to parse --version output: ${output}`);
  }
  return match[0];
}

export async function findOnPath(command: string): Promise<string | undefined> {
  const pathEnv = process.env.PATH ?? '';
  if (pathEnv.trim() === '') {
    return undefined;
  }

  const entries = pathEnv.split(path.delimiter).filter(Boolean);
  const extensions =
    process.platform === 'win32'
      ? (process.env.PATHEXT ?? '.EXE;.CMD;.BAT;.COM')
          .split(';')
          .filter(Boolean)
          .map((ext) => ext.toLowerCase())
      : [''];

  for (const entry of entries) {
    for (const ext of extensions) {
      const candidate = path.join(entry, process.platform === 'win32' ? `${command}${ext}` : command);
      try {
        const stat = await fs.promises.stat(candidate);
        if (stat.isFile()) {
          return candidate;
        }
      } catch {
        // continue
      }
    }
  }
  return undefined;
}

export async function openInstallDocs(context: vscode.ExtensionContext): Promise<void> {
  const readmeUri = vscode.Uri.joinPath(context.extensionUri, 'README.md');
  try {
    await vscode.commands.executeCommand('markdown.showPreview', readmeUri);
  } catch {
    await vscode.commands.executeCommand('vscode.open', readmeUri);
  }
}
