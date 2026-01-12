import assert from 'node:assert/strict';
import * as fs from 'node:fs/promises';
import * as path from 'node:path';
import test from 'node:test';
import * as ts from 'typescript';

async function collectTypeScriptFiles(dir: string): Promise<string[]> {
  const out: string[] = [];
  const entries = await fs.readdir(dir, { withFileTypes: true });
  for (const entry of entries) {
    const resolved = path.join(dir, entry.name);
    if (entry.isDirectory()) {
      out.push(...(await collectTypeScriptFiles(resolved)));
    } else if (entry.isFile() && resolved.endsWith('.ts')) {
      out.push(resolved);
    }
  }
  return out;
}

type FileOperationKind = 'create' | 'delete' | 'rename';

const FILE_OPERATION_LISTENERS: ReadonlyArray<{
  kind: FileOperationKind;
  listenerMethod: string;
  notificationMethod: string;
}> = [
  { kind: 'create', listenerMethod: 'onDidCreateFiles', notificationMethod: 'workspace/didCreateFiles' },
  { kind: 'delete', listenerMethod: 'onDidDeleteFiles', notificationMethod: 'workspace/didDeleteFiles' },
  { kind: 'rename', listenerMethod: 'onDidRenameFiles', notificationMethod: 'workspace/didRenameFiles' },
];

const FILE_OPERATION_NOTIFICATION_TYPES: Readonly<Record<string, string>> = {
  DidCreateFilesNotification: 'workspace/didCreateFiles',
  DidDeleteFilesNotification: 'workspace/didDeleteFiles',
  DidRenameFilesNotification: 'workspace/didRenameFiles',
};

function getCalledMethodName(expr: ts.Expression, env: Map<string, string>): string | undefined {
  if (ts.isPropertyAccessExpression(expr)) {
    return expr.name.text;
  }

  if (ts.isElementAccessExpression(expr)) {
    const argument = expr.argumentExpression;
    if (!argument) {
      return undefined;
    }
    return evalConstString(argument, env);
  }

  return undefined;
}

function evalConstString(expr: ts.Expression, env: Map<string, string>): string | undefined {
  if (ts.isStringLiteral(expr) || ts.isNoSubstitutionTemplateLiteral(expr)) {
    return expr.text;
  }

  if (ts.isTemplateExpression(expr)) {
    let text = expr.head.text;
    for (const span of expr.templateSpans) {
      const value = evalConstString(span.expression, env);
      if (typeof value === 'undefined') {
        return undefined;
      }
      text += value + span.literal.text;
    }
    return text;
  }

  if (ts.isBinaryExpression(expr) && expr.operatorToken.kind === ts.SyntaxKind.PlusToken) {
    const left = evalConstString(expr.left, env);
    const right = evalConstString(expr.right, env);
    if (typeof left === 'undefined' || typeof right === 'undefined') {
      return undefined;
    }
    return left + right;
  }

  if (ts.isParenthesizedExpression(expr)) {
    return evalConstString(expr.expression, env);
  }

  if (ts.isAsExpression(expr) || ts.isTypeAssertionExpression(expr)) {
    return evalConstString(expr.expression, env);
  }

  if (ts.isIdentifier(expr)) {
    return env.get(expr.text);
  }

  return undefined;
}

function buildImportAliasMap(sourceFile: ts.SourceFile): Map<string, string> {
  const aliases = new Map<string, string>();
  for (const statement of sourceFile.statements) {
    if (!ts.isImportDeclaration(statement)) {
      continue;
    }
    const clause = statement.importClause;
    if (!clause?.namedBindings) {
      continue;
    }
    if (!ts.isNamedImports(clause.namedBindings)) {
      continue;
    }
    for (const element of clause.namedBindings.elements) {
      const imported = element.propertyName ? element.propertyName.text : element.name.text;
      const local = element.name.text;
      aliases.set(local, imported);
    }
  }
  return aliases;
}

function lastPropertyName(expr: ts.Expression): string | undefined {
  if (ts.isIdentifier(expr)) {
    return expr.text;
  }
  if (ts.isPropertyAccessExpression(expr)) {
    return expr.name.text;
  }
  return undefined;
}

function resolveNotificationMethod(
  expr: ts.Expression,
  env: Map<string, string>,
  imports: Map<string, string>,
): string | undefined {
  const asString = evalConstString(expr, env);
  if (typeof asString === 'string') {
    return asString;
  }

  // Handle common LSP constant patterns, e.g.
  //   client.sendNotification(DidRenameFilesNotification.type, ...)
  //   client.sendNotification(DidRenameFilesNotification.type.method, ...)
  if (ts.isPropertyAccessExpression(expr)) {
    if (expr.name.text === 'method') {
      return resolveNotificationMethod(expr.expression, env, imports);
    }

    if (expr.name.text === 'type') {
      const localName = lastPropertyName(expr.expression);
      const importedName = localName ? (imports.get(localName) ?? localName) : undefined;
      if (importedName && importedName in FILE_OPERATION_NOTIFICATION_TYPES) {
        return FILE_OPERATION_NOTIFICATION_TYPES[importedName];
      }
    }
  }

  return undefined;
}

function buildConstStringEnvFromVariableStatements(
  statements: readonly ts.Statement[],
  imports: Map<string, string>,
  baseEnv: Map<string, string>,
): Map<string, string> {
  const declarations: Array<{ name: string; initializer: ts.Expression }> = [];

  for (const statement of statements) {
    if (!ts.isVariableStatement(statement)) {
      continue;
    }
    const declList = statement.declarationList;
    if ((declList.flags & ts.NodeFlags.Const) === 0) {
      continue;
    }
    for (const decl of declList.declarations) {
      if (!ts.isIdentifier(decl.name) || !decl.initializer) {
        continue;
      }
      declarations.push({ name: decl.name.text, initializer: decl.initializer });
    }
  }

  const env = new Map<string, string>(baseEnv);
  const resolved = new Map<string, string>();
  let changed = true;
  while (changed) {
    changed = false;
    for (const decl of declarations) {
      if (env.has(decl.name)) {
        continue;
      }
      const value = resolveNotificationMethod(decl.initializer, env, imports);
      if (typeof value !== 'undefined') {
        env.set(decl.name, value);
        resolved.set(decl.name, value);
        changed = true;
      }
    }
  }
  return resolved;
}

test('extension does not manually forward workspace file operations (vscode-languageclient handles workspace/fileOperations)', async () => {
  const srcRoot = path.resolve(__dirname, '../../src');
  const tsFiles = await collectTypeScriptFiles(srcRoot);

  const bannedNotificationMethods = new Set(FILE_OPERATION_LISTENERS.map((entry) => entry.notificationMethod));
  const violations = new Set<string>();

  for (const filePath of tsFiles) {
    const raw = await fs.readFile(filePath, 'utf8');
    const sourceFile = ts.createSourceFile(filePath, raw, ts.ScriptTarget.ESNext, true);
    const importAliases = buildImportAliasMap(sourceFile);
    const fileEnv = new Map<string, string>(
      buildConstStringEnvFromVariableStatements(sourceFile.statements, importAliases, new Map<string, string>()),
    );

    const scanForBannedSendNotifications = (node: ts.Node, env: Map<string, string>) => {
      if (ts.isCallExpression(node) && getCalledMethodName(node.expression, env) === 'sendNotification') {
        const arg0 = node.arguments[0];
        const method = arg0 ? resolveNotificationMethod(arg0, env, importAliases) : undefined;
        if (method && bannedNotificationMethods.has(method)) {
          const loc = sourceFile.getLineAndCharacterOfPosition(node.getStart(sourceFile));
          violations.add(`${path.relative(srcRoot, filePath)}:${loc.line + 1}:${loc.character + 1} sendNotification ${method}`);
        }
      }

      let nextEnv = env;
      if (ts.isBlock(node)) {
        const blockEnv = buildConstStringEnvFromVariableStatements(node.statements, importAliases, env);
        if (blockEnv.size > 0) {
          nextEnv = new Map<string, string>([...env, ...blockEnv]);
        }
      }

      ts.forEachChild(node, (child) => {
        scanForBannedSendNotifications(child, nextEnv);
      });
    };

    scanForBannedSendNotifications(sourceFile, fileEnv);

    const visit = (node: ts.Node) => {
      if (!ts.isCallExpression(node)) {
        ts.forEachChild(node, visit);
        return;
      }

      const callee = node.expression;
      if (!ts.isPropertyAccessExpression(callee)) {
        ts.forEachChild(node, visit);
        return;
      }

      const listenerMethod = callee.name.text;
      const listener = FILE_OPERATION_LISTENERS.find((entry) => entry.listenerMethod === listenerMethod);
      if (!listener) {
        ts.forEachChild(node, visit);
        return;
      }

      const handlerArg = node.arguments[0];
      if (!handlerArg || (!ts.isArrowFunction(handlerArg) && !ts.isFunctionExpression(handlerArg))) {
        ts.forEachChild(node, visit);
        return;
      }

      const handlerStatements = ts.isBlock(handlerArg.body) ? handlerArg.body.statements : undefined;
      const handlerEnv = handlerStatements
        ? buildConstStringEnvFromVariableStatements(handlerStatements, importAliases, fileEnv)
        : new Map<string, string>();
      const env = new Map<string, string>([...fileEnv, ...handlerEnv]);

      const checkHandler = (handlerNode: ts.Node) => {
        if (ts.isCallExpression(handlerNode) && getCalledMethodName(handlerNode.expression, env) === 'sendNotification') {
          const arg0 = handlerNode.arguments[0];
          if (arg0) {
            const resolved = resolveNotificationMethod(arg0, env, importAliases);
            if (resolved === listener.notificationMethod) {
              const loc = sourceFile.getLineAndCharacterOfPosition(handlerNode.getStart(sourceFile));
              violations.add(
                `${path.relative(srcRoot, filePath)}:${loc.line + 1}:${loc.character + 1} ${listenerMethod} -> ${listener.notificationMethod}`,
              );
            }
          }
        }
        ts.forEachChild(handlerNode, checkHandler);
      };

      checkHandler(handlerArg.body);

      ts.forEachChild(node, visit);
    };

    visit(sourceFile);
  }

  assert.deepEqual(
    Array.from(violations).sort(),
    [],
    `Manual forwarding of workspace file operations detected.\n` +
      `vscode-languageclient should handle LSP workspace/fileOperations automatically.\n\n` +
      Array.from(violations).sort().join('\n'),
  );
});
