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

function buildConstStringEnvFromVariableStatements(statements: readonly ts.Statement[]): Map<string, string> {
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

  const env = new Map<string, string>();
  let changed = true;
  while (changed) {
    changed = false;
    for (const decl of declarations) {
      if (env.has(decl.name)) {
        continue;
      }
      const value = evalConstString(decl.initializer, env);
      if (typeof value !== 'undefined') {
        env.set(decl.name, value);
        changed = true;
      }
    }
  }
  return env;
}

test('extension does not manually forward workspace file operations (vscode-languageclient handles workspace/fileOperations)', async () => {
  const srcRoot = path.resolve(__dirname, '../../src');
  const tsFiles = await collectTypeScriptFiles(srcRoot);

  const violations: string[] = [];

  for (const filePath of tsFiles) {
    const raw = await fs.readFile(filePath, 'utf8');
    const sourceFile = ts.createSourceFile(filePath, raw, ts.ScriptTarget.ESNext, true);
    const fileEnv = buildConstStringEnvFromVariableStatements(sourceFile.statements);

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
      const handlerEnv = handlerStatements ? buildConstStringEnvFromVariableStatements(handlerStatements) : new Map<string, string>();
      const env = new Map<string, string>([...fileEnv, ...handlerEnv]);

      const checkHandler = (handlerNode: ts.Node) => {
        if (ts.isCallExpression(handlerNode) && ts.isPropertyAccessExpression(handlerNode.expression)) {
          if (handlerNode.expression.name.text === 'sendNotification') {
            const arg0 = handlerNode.arguments[0];
            if (arg0) {
              const resolved = evalConstString(arg0, env);
              if (resolved === listener.notificationMethod) {
                const loc = sourceFile.getLineAndCharacterOfPosition(handlerNode.getStart(sourceFile));
                violations.push(
                  `${path.relative(srcRoot, filePath)}:${loc.line + 1}:${loc.character + 1} ${listenerMethod} -> ${listener.notificationMethod}`,
                );
              }
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
    violations,
    [],
    `Manual forwarding of workspace file operations detected.\n` +
      `vscode-languageclient should handle LSP workspace/fileOperations automatically.\n\n` +
      violations.join('\n'),
  );
});
