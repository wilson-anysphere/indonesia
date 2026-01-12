import * as fs from 'node:fs/promises';
import * as ts from 'typescript';

export async function readTsSourceFile(filePath: string): Promise<ts.SourceFile> {
  const contents = await fs.readFile(filePath, 'utf8');
  return ts.createSourceFile(filePath, contents, ts.ScriptTarget.ESNext, true);
}

/**
 * Best-effort helper for stripping expression wrappers (type assertions, parens, etc) so tests can
 * match on the underlying call / identifier more reliably.
 *
 * Note: This is intentionally conservative and only unwraps nodes that are expected to preserve
 * runtime semantics.
 */
export function unwrapExpression(expr: ts.Expression): ts.Expression {
  let out: ts.Expression = expr;
  while (true) {
    if (ts.isParenthesizedExpression(out)) {
      out = out.expression;
      continue;
    }
    if (ts.isAsExpression(out) || ts.isTypeAssertionExpression(out)) {
      out = out.expression;
      continue;
    }
    if (ts.isNonNullExpression(out)) {
      out = out.expression;
      continue;
    }
    if (ts.isVoidExpression(out)) {
      out = out.expression;
      continue;
    }
    if (ts.isAwaitExpression(out)) {
      out = out.expression;
      continue;
    }
    // TypeScript 4.9+: `expr satisfies Type`
    // (use a runtime check so this file works across TS versions).
    if (typeof (ts as unknown as { isSatisfiesExpression?: unknown }).isSatisfiesExpression === 'function') {
      const fn = (ts as unknown as { isSatisfiesExpression: (n: ts.Node) => n is ts.Expression }).isSatisfiesExpression;
      if (fn(out)) {
        out = (out as unknown as { expression: ts.Expression }).expression;
        continue;
      }
    }
    break;
  }
  return out;
}

