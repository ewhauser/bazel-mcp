import { readFile } from 'node:fs/promises';
import path from 'node:path';

export const prerender = true;

const sources = [
  ['README', 'README.md'],
  ['Configuration', 'docs/configuration.md'],
  ['Token benchmark', 'docs/benchmarks.md'],
  ['Security', 'SECURITY.md'],
  ['Contributing', 'CONTRIBUTING.md'],
] as const;

export async function GET() {
  const sections = await Promise.all(
    sources.map(async ([title, relativePath]) => {
      const content = await readFile(path.resolve(process.cwd(), '..', relativePath), 'utf8');
      return `# ${title}\n\n${content.replace(/^# .+\r?\n/, '')}`;
    }),
  );

  const body = `# bazel-mcp documentation context\n\n${sections.join('\n\n---\n\n')}`;
  return new Response(body, {
    headers: { 'Content-Type': 'text/plain; charset=utf-8' },
  });
}
