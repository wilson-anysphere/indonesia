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

const BANNED_FILE_OPERATION_NOTIFICATION_METHODS = new Set<string>(
  FILE_OPERATION_LISTENERS.map((entry) => entry.notificationMethod),
);

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

  if (ts.isPropertyAccessExpression(expr)) {
    const base = expr.expression;
    if (ts.isIdentifier(base)) {
      const value = env.get(`${base.text}.${expr.name.text}`);
      if (typeof value === 'string') {
        return value;
      }
    }
  }

  if (ts.isElementAccessExpression(expr)) {
    const base = expr.expression;
    if (ts.isIdentifier(base)) {
      const key = expr.argumentExpression ? evalConstString(expr.argumentExpression, env) : undefined;
      if (typeof key === 'string') {
        const value = env.get(`${base.text}.${key}`);
        if (typeof value === 'string') {
          return value;
        }
      }
    }
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

  // Handle direct uses of the Did*FilesNotification constants, e.g.
  //   client.sendNotification(DidRenameFilesNotification as any, ...)
  if (ts.isIdentifier(expr)) {
    const importedName = imports.get(expr.text) ?? expr.text;
    if (importedName in FILE_OPERATION_NOTIFICATION_TYPES) {
      return FILE_OPERATION_NOTIFICATION_TYPES[importedName];
    }
  }

  // Handle ad-hoc NotificationType-like objects, e.g.
  //   client.sendNotification({ method: 'workspace/didRenameFiles', ... } as any, ...)
  if (ts.isObjectLiteralExpression(expr)) {
    for (const entry of expr.properties) {
      if (!ts.isPropertyAssignment(entry)) {
        continue;
      }

      let key: string | undefined;
      if (ts.isIdentifier(entry.name)) {
        key = entry.name.text;
      } else if (ts.isStringLiteral(entry.name) || ts.isNoSubstitutionTemplateLiteral(entry.name)) {
        key = entry.name.text;
      } else if (ts.isComputedPropertyName(entry.name)) {
        key = evalConstString(entry.name.expression, env);
      }

      if (key !== 'method') {
        continue;
      }

      const method = evalConstString(entry.initializer, env);
      if (typeof method === 'string') {
        return method;
      }
    }
  }

  // Handle bracket access on protocol constants, e.g.
  //   DidRenameFilesNotification['method']
  //   DidRenameFilesNotification['type']
  //   DidRenameFilesNotification['type'].method
  // by rewriting to a normal property access and reusing the logic below.
  if (ts.isElementAccessExpression(expr)) {
    const key = expr.argumentExpression ? evalConstString(expr.argumentExpression, env) : undefined;
    if (key && /^[A-Za-z_$][A-Za-z0-9_$]*$/.test(key)) {
      return resolveNotificationMethod(ts.factory.createPropertyAccessExpression(expr.expression, key), env, imports);
    }
  }

  // Handle manually constructed NotificationType instances, e.g.
  //   new NotificationType('workspace/didRenameFiles')
  // so forwarding can't bypass this check by avoiding the predefined
  // Did*FilesNotification constants.
  if (ts.isNewExpression(expr)) {
    const ctorName = lastPropertyName(expr.expression);
    const importedName = ctorName ? (imports.get(ctorName) ?? ctorName) : undefined;
    if (importedName && (/^NotificationType\d*$/.test(importedName) || /^ProtocolNotificationType\d*$/.test(importedName))) {
      const arg0 = expr.arguments?.[0];
      const method = arg0 ? evalConstString(arg0, env) : undefined;
      if (typeof method === 'string') {
        return method;
      }
    }
  }

  // Handle common LSP constant patterns, e.g.
  //   client.sendNotification(DidRenameFilesNotification.type, ...)
  //   client.sendNotification(DidRenameFilesNotification.type.method, ...)
  if (ts.isPropertyAccessExpression(expr)) {
    if (expr.name.text === 'method') {
      const localName = lastPropertyName(expr.expression);
      const importedName = localName ? (imports.get(localName) ?? localName) : undefined;
      if (importedName && importedName in FILE_OPERATION_NOTIFICATION_TYPES) {
        return FILE_OPERATION_NOTIFICATION_TYPES[importedName];
      }
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

function unwrapExpression(expr: ts.Expression): ts.Expression {
  if (ts.isParenthesizedExpression(expr)) {
    return unwrapExpression(expr.expression);
  }
  if (ts.isAsExpression(expr) || ts.isTypeAssertionExpression(expr)) {
    return unwrapExpression(expr.expression);
  }
  return expr;
}

function buildFileOperationNotificationConstantAliases(
  statements: readonly ts.Statement[],
  imports: Map<string, string>,
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

  const aliasMap = new Map<string, string>();
  let changed = true;
  while (changed) {
    changed = false;
    for (const decl of declarations) {
      if (aliasMap.has(decl.name)) {
        continue;
      }

      const init = unwrapExpression(decl.initializer);
      if (!ts.isIdentifier(init)) {
        continue;
      }

      const localName = init.text;
      const importedName = aliasMap.get(localName) ?? imports.get(localName) ?? localName;
      if (importedName in FILE_OPERATION_NOTIFICATION_TYPES) {
        aliasMap.set(decl.name, importedName);
        changed = true;
      }
    }
  }
  return aliasMap;
}

function isSendNotificationReference(expr: ts.Expression, env: Map<string, string>, aliases: Set<string>): boolean {
  const unwrapped = unwrapExpression(expr);

  if (ts.isIdentifier(unwrapped)) {
    return aliases.has(unwrapped.text);
  }

  // `client.sendNotification` / `client['sendNotification']`
  if (ts.isPropertyAccessExpression(unwrapped) || ts.isElementAccessExpression(unwrapped)) {
    return getCalledMethodName(unwrapped, env) === 'sendNotification';
  }

  // `client.sendNotification.bind(client)` / `client['sendNotification'].bind(client)`
  if (ts.isCallExpression(unwrapped)) {
    const callee = unwrapped.expression;
    if (getCalledMethodName(callee, env) !== 'bind') {
      return false;
    }
    if (!ts.isPropertyAccessExpression(callee) && !ts.isElementAccessExpression(callee)) {
      return false;
    }
    return isSendNotificationReference(callee.expression, env, aliases);
  }

  return false;
}

function getSendNotificationMethodArg(
  node: ts.CallExpression,
  env: Map<string, string>,
  aliases: Set<string>,
): ts.Expression | undefined {
  const callee = unwrapExpression(node.expression);

  // Direct alias call: `sendNotification(...)` where `sendNotification` is an alias
  // to the client's sendNotification method.
  if (ts.isIdentifier(callee) && aliases.has(callee.text)) {
    return node.arguments[0];
  }

  // Direct call: `client.sendNotification(...)` / `client['sendNotification'](...)`
  if (getCalledMethodName(callee, env) === 'sendNotification') {
    return node.arguments[0];
  }

  // `.call` forwarding: `client.sendNotification.call(client, method, ...)`
  // or `sendNotification.call(client, method, ...)`
  if (getCalledMethodName(callee, env) === 'call' && (ts.isPropertyAccessExpression(callee) || ts.isElementAccessExpression(callee))) {
    if (isSendNotificationReference(callee.expression, env, aliases)) {
      return node.arguments[1];
    }
  }

  // `.apply` forwarding: `client.sendNotification.apply(client, [method, ...])`
  if (getCalledMethodName(callee, env) === 'apply' && (ts.isPropertyAccessExpression(callee) || ts.isElementAccessExpression(callee))) {
    if (isSendNotificationReference(callee.expression, env, aliases)) {
      const arrayArg = node.arguments[1];
      if (arrayArg) {
        const unwrappedArray = unwrapExpression(arrayArg);
        if (ts.isArrayLiteralExpression(unwrappedArray)) {
          const first = unwrappedArray.elements[0];
          return first && ts.isExpression(first) ? first : undefined;
        }
      }
    }
  }

  // `Reflect.apply` forwarding: `Reflect.apply(client.sendNotification, client, [method, ...])`
  if (getCalledMethodName(callee, env) === 'apply' && (ts.isPropertyAccessExpression(callee) || ts.isElementAccessExpression(callee))) {
    if (ts.isIdentifier(callee.expression) && callee.expression.text === 'Reflect') {
      const fnArg = node.arguments[0];
      const argsArg = node.arguments[2];
      if (fnArg && isSendNotificationReference(fnArg, env, aliases) && argsArg) {
        const unwrappedArgs = unwrapExpression(argsArg);
        if (ts.isArrayLiteralExpression(unwrappedArgs)) {
          const first = unwrappedArgs.elements[0];
          return first && ts.isExpression(first) ? first : undefined;
        }
      }
    }
  }

  // Inline bind + immediate call: `client.sendNotification.bind(client)(method, ...)`
  // or `sendNotification.bind(client)(method, ...)`
  if (ts.isCallExpression(callee) && isSendNotificationReference(callee, env, aliases)) {
    const bindArgs = callee.arguments;
    // `.bind(thisArg, method)` pre-binds the method argument.
    if (bindArgs.length >= 2) {
      return bindArgs[1];
    }
    return node.arguments[0];
  }

  return undefined;
}

function buildSendNotificationAliasesFromVariableStatements(
  statements: readonly ts.Statement[],
  env: Map<string, string>,
  baseAliases: Set<string>,
): Set<string> {
  const bindingAliases = new Set<string>();
  const candidateDecls: Array<{ name: string; initializer: ts.Expression }> = [];

  for (const statement of statements) {
    if (!ts.isVariableStatement(statement)) {
      continue;
    }
    const declList = statement.declarationList;
    if ((declList.flags & ts.NodeFlags.Const) === 0) {
      continue;
    }

    for (const decl of declList.declarations) {
      if (!decl.initializer) {
        continue;
      }

      if (ts.isIdentifier(decl.name)) {
        candidateDecls.push({ name: decl.name.text, initializer: decl.initializer });
        continue;
      }

      if (ts.isObjectBindingPattern(decl.name)) {
        for (const element of decl.name.elements) {
          const localName = ts.isIdentifier(element.name) ? element.name.text : undefined;
          if (!localName) {
            continue;
          }

          let propertyName: string | undefined;
          if (!element.propertyName) {
            propertyName = localName;
          } else if (ts.isIdentifier(element.propertyName)) {
            propertyName = element.propertyName.text;
          } else if (ts.isStringLiteral(element.propertyName) || ts.isNoSubstitutionTemplateLiteral(element.propertyName)) {
            propertyName = element.propertyName.text;
          } else if (ts.isComputedPropertyName(element.propertyName)) {
            propertyName = evalConstString(element.propertyName.expression, env);
          }

          if (propertyName === 'sendNotification') {
            bindingAliases.add(localName);
          }
        }
      }
    }
  }

  const aliases = new Set<string>(baseAliases);
  const resolved = new Set<string>();
  for (const name of bindingAliases) {
    if (!aliases.has(name)) {
      aliases.add(name);
      resolved.add(name);
    }
  }

  let changed = true;
  while (changed) {
    changed = false;
    for (const decl of candidateDecls) {
      if (aliases.has(decl.name)) {
        continue;
      }
      if (isSendNotificationReference(decl.initializer, env, aliases)) {
        aliases.add(decl.name);
        resolved.add(decl.name);
        changed = true;
      }
    }
  }

  return resolved;
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
      if (!decl.initializer) {
        continue;
      }

      if (ts.isIdentifier(decl.name)) {
        declarations.push({ name: decl.name.text, initializer: decl.initializer });

        const unwrappedInitializer = unwrapExpression(decl.initializer);
        if (ts.isObjectLiteralExpression(unwrappedInitializer)) {
          for (const element of unwrappedInitializer.properties) {
            if (ts.isSpreadAssignment(element)) {
              continue;
            }

            let propertyNameText: string | undefined;
            if (ts.isPropertyAssignment(element)) {
              const name = element.name;
              if (ts.isIdentifier(name)) {
                propertyNameText = name.text;
              } else if (ts.isStringLiteral(name) || ts.isNoSubstitutionTemplateLiteral(name)) {
                propertyNameText = name.text;
              } else if (ts.isComputedPropertyName(name)) {
                propertyNameText = evalConstString(name.expression, baseEnv);
              }
              if (!propertyNameText) {
                continue;
              }
              declarations.push({
                name: `${decl.name.text}.${propertyNameText}`,
                initializer: element.initializer,
              });
              continue;
            }

            if (ts.isShorthandPropertyAssignment(element)) {
              propertyNameText = element.name.text;
              declarations.push({
                name: `${decl.name.text}.${propertyNameText}`,
                initializer: element.name,
              });
              continue;
            }

            if (ts.isMethodDeclaration(element)) {
              // Ignore methods; they can't be constant strings for our purposes.
              continue;
            }

            if (ts.isGetAccessorDeclaration(element) || ts.isSetAccessorDeclaration(element)) {
              continue;
            }
          }
        }
        continue;
      }

      // Handle simple object destructuring, e.g.
      //   const { type } = DidRenameFilesNotification;
      //   const { method } = DidRenameFilesNotification.type;
      if (ts.isObjectBindingPattern(decl.name)) {
        for (const element of decl.name.elements) {
          if (element.dotDotDotToken) {
            continue;
          }
          if (!ts.isIdentifier(element.name)) {
            continue;
          }
          const localName = element.name.text;

          let propertyNameText: string | undefined;
          if (!element.propertyName) {
            propertyNameText = localName;
          } else if (ts.isIdentifier(element.propertyName)) {
            propertyNameText = element.propertyName.text;
          } else if (ts.isStringLiteral(element.propertyName) || ts.isNoSubstitutionTemplateLiteral(element.propertyName)) {
            propertyNameText = element.propertyName.text;
          } else if (ts.isComputedPropertyName(element.propertyName)) {
            propertyNameText = evalConstString(element.propertyName.expression, baseEnv);
          }

          if (!propertyNameText) {
            continue;
          }

          // Keep this intentionally simple: only support identifier property names.
          // (file-operation notifications are keyed by identifier properties like `type` and `method`.)
          if (!/^[A-Za-z_$][A-Za-z0-9_$]*$/.test(propertyNameText)) {
            continue;
          }

          const synthetic = ts.factory.createPropertyAccessExpression(decl.initializer, propertyNameText);
          declarations.push({ name: localName, initializer: synthetic });
        }
      }
    }
  }

  const env = new Map<string, string>(baseEnv);
  const resolved = new Map<string, string>();
  let changed = true;
  while (changed) {
    changed = false;
    for (const decl of declarations) {
      if (resolved.has(decl.name)) {
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

  const violations = new Set<string>();

  for (const filePath of tsFiles) {
    const raw = await fs.readFile(filePath, 'utf8');
    const sourceFile = ts.createSourceFile(filePath, raw, ts.ScriptTarget.ESNext, true);
    const fileViolations = scanSourceFileForManualFileOperationForwarding(sourceFile, { filePath, srcRoot });
    for (const entry of fileViolations) {
      violations.add(entry);
    }
  }

  assert.deepEqual(
    Array.from(violations).sort(),
    [],
    `Manual forwarding of workspace file operations detected.\n` +
      `vscode-languageclient should handle LSP workspace/fileOperations automatically.\n\n` +
      Array.from(violations).sort().join('\n'),
  );
});

function scanSourceFileForManualFileOperationForwarding(
  sourceFile: ts.SourceFile,
  opts: { filePath: string; srcRoot: string },
): Set<string> {
  const { filePath, srcRoot } = opts;
  const violations = new Set<string>();

  const importAliases = buildImportAliasMap(sourceFile);
  const fileOpConstantAliases = buildFileOperationNotificationConstantAliases(sourceFile.statements, importAliases);
  const allAliases = new Map(importAliases);
  for (const [local, imported] of fileOpConstantAliases) {
    if (!allAliases.has(local)) {
      allAliases.set(local, imported);
    }
  }
  const fileEnv = new Map<string, string>(
    buildConstStringEnvFromVariableStatements(sourceFile.statements, allAliases, new Map<string, string>()),
  );
  const fileAliases = new Set<string>(
    buildSendNotificationAliasesFromVariableStatements(sourceFile.statements, fileEnv, new Set<string>()),
  );

  const scanForBannedSendNotifications = (node: ts.Node, env: Map<string, string>, aliases: Set<string>) => {
    if (ts.isCallExpression(node)) {
      const methodExpr = getSendNotificationMethodArg(node, env, aliases);
      if (methodExpr) {
        const method = resolveNotificationMethod(methodExpr, env, allAliases);
        if (method && BANNED_FILE_OPERATION_NOTIFICATION_METHODS.has(method)) {
          const loc = sourceFile.getLineAndCharacterOfPosition(node.getStart(sourceFile));
          violations.add(`${path.relative(srcRoot, filePath)}:${loc.line + 1}:${loc.character + 1} sendNotification ${method}`);
        }
      }

      // Pre-bound sendNotification helpers, e.g.
      //   const sendRename = client.sendNotification.bind(client, 'workspace/didRenameFiles');
      // should also be treated as manual forwarding.
      const callee = unwrapExpression(node.expression);
      if (
        getCalledMethodName(callee, env) === 'bind' &&
        (ts.isPropertyAccessExpression(callee) || ts.isElementAccessExpression(callee))
      ) {
        if (isSendNotificationReference(callee.expression, env, aliases)) {
          const boundMethodExpr = node.arguments[1];
          if (boundMethodExpr) {
            const method = resolveNotificationMethod(boundMethodExpr, env, allAliases);
            if (method && BANNED_FILE_OPERATION_NOTIFICATION_METHODS.has(method)) {
              const loc = sourceFile.getLineAndCharacterOfPosition(node.getStart(sourceFile));
              violations.add(`${path.relative(srcRoot, filePath)}:${loc.line + 1}:${loc.character + 1} bind ${method}`);
            }
          }
        }
      }
    }

    let nextEnv = env;
    let nextAliases = aliases;
    if (ts.isBlock(node)) {
      const blockEnv = buildConstStringEnvFromVariableStatements(node.statements, allAliases, env);
      const combinedEnv = blockEnv.size > 0 ? new Map<string, string>([...env, ...blockEnv]) : env;
      nextEnv = combinedEnv;

      const blockAliases = buildSendNotificationAliasesFromVariableStatements(node.statements, combinedEnv, aliases);
      if (blockAliases.size > 0) {
        nextAliases = new Set<string>([...aliases, ...blockAliases]);
      }
    }

    ts.forEachChild(node, (child) => {
      scanForBannedSendNotifications(child, nextEnv, nextAliases);
    });
  };

  scanForBannedSendNotifications(sourceFile, fileEnv, fileAliases);
  return violations;
}

test('noManualFileOperationsForwarding scan flags workspace/didRenameFiles sendNotification (fixtures)', () => {
  const srcRoot = '/';
  const filePath = '/fixture.ts';

  const source = `
    client.sendNotification('workspace/didRenameFiles', { files: [] });
  `;

  const sourceFile = ts.createSourceFile(filePath, source, ts.ScriptTarget.ESNext, true);
  const violations = scanSourceFileForManualFileOperationForwarding(sourceFile, { filePath, srcRoot });
  assert.ok(Array.from(violations).some((entry) => entry.includes('workspace/didRenameFiles')));
});

test('noManualFileOperationsForwarding scan does not flag unrelated notifications (fixtures)', () => {
  const srcRoot = '/';
  const filePath = '/fixture.ts';

  const source = `
    client.sendNotification('workspace/didChangeConfiguration', { settings: null });
  `;

  const sourceFile = ts.createSourceFile(filePath, source, ts.ScriptTarget.ESNext, true);
  const violations = scanSourceFileForManualFileOperationForwarding(sourceFile, { filePath, srcRoot });
  assert.deepEqual(Array.from(violations), []);
});

test('noManualFileOperationsForwarding scan flags file operation NotificationType constants (fixtures)', () => {
  const srcRoot = '/';
  const filePath = '/fixture.ts';

  const source = `
    import { DidRenameFilesNotification } from 'vscode-languageserver-protocol';
    client.sendNotification(DidRenameFilesNotification.type, { files: [] });
  `;

  const sourceFile = ts.createSourceFile(filePath, source, ts.ScriptTarget.ESNext, true);
  const violations = scanSourceFileForManualFileOperationForwarding(sourceFile, { filePath, srcRoot });
  assert.ok(Array.from(violations).some((entry) => entry.includes('workspace/didRenameFiles')));
});

test('noManualFileOperationsForwarding scan flags sendNotification prebind helpers (fixtures)', () => {
  const srcRoot = '/';
  const filePath = '/fixture.ts';

  const source = `
    const sendRename = client.sendNotification.bind(client, 'workspace/didRenameFiles');
    sendRename({ files: [] });
  `;

  const sourceFile = ts.createSourceFile(filePath, source, ts.ScriptTarget.ESNext, true);
  const violations = scanSourceFileForManualFileOperationForwarding(sourceFile, { filePath, srcRoot });
  assert.ok(Array.from(violations).some((entry) => entry.includes('workspace/didRenameFiles')));
});

test('noManualFileOperationsForwarding scan flags Reflect.apply sendNotification wrappers (fixtures)', () => {
  const srcRoot = '/';
  const filePath = '/fixture.ts';

  const source = `
    Reflect.apply(client.sendNotification, client, ['workspace/didRenameFiles', { files: [] }]);
  `;

  const sourceFile = ts.createSourceFile(filePath, source, ts.ScriptTarget.ESNext, true);
  const violations = scanSourceFileForManualFileOperationForwarding(sourceFile, { filePath, srcRoot });
  assert.ok(Array.from(violations).some((entry) => entry.includes('workspace/didRenameFiles')));
});

test('noManualFileOperationsForwarding scan resolves const object property values (fixtures)', () => {
  const srcRoot = '/';
  const filePath = '/fixture.ts';

  const source = `
    const METHODS = { rename: 'workspace/didRenameFiles' } as const;
    client.sendNotification(METHODS.rename, { files: [] });
  `;

  const sourceFile = ts.createSourceFile(filePath, source, ts.ScriptTarget.ESNext, true);
  const violations = scanSourceFileForManualFileOperationForwarding(sourceFile, { filePath, srcRoot });
  assert.ok(Array.from(violations).some((entry) => entry.includes('workspace/didRenameFiles')));
});

test('noManualFileOperationsForwarding scan resolves protocol constants nested inside objects (fixtures)', () => {
  const srcRoot = '/';
  const filePath = '/fixture.ts';

  const source = `
    import { DidRenameFilesNotification } from 'vscode-languageserver-protocol';
    const METHODS = { rename: DidRenameFilesNotification.method } as const;
    client.sendNotification(METHODS.rename, { files: [] });
  `;

  const sourceFile = ts.createSourceFile(filePath, source, ts.ScriptTarget.ESNext, true);
  const violations = scanSourceFileForManualFileOperationForwarding(sourceFile, { filePath, srcRoot });
  assert.ok(Array.from(violations).some((entry) => entry.includes('workspace/didRenameFiles')));
});
