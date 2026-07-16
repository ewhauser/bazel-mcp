import { mkdir, readFile, rm, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const websiteRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const repositoryRoot = path.resolve(websiteRoot, '..');
const generatedRoot = path.join(
  websiteRoot,
  'src',
  'content',
  'docs',
  'reference',
  'generated',
);

const documents = [
  {
    source: 'docs/configuration.md',
    destination: 'configuration.md',
    title: 'Configuration',
    description: 'Configure workspace policy, Bazel discovery, retention, result encoding, and task execution.',
  },
  {
    source: 'docs/benchmarks.md',
    destination: 'benchmarks.md',
    title: 'Token benchmark',
    description: 'Methodology, acceptance gates, and reproducible context-reduction results.',
  },
  {
    source: 'docs/agentic-benchmark-report.md',
    destination: 'agentic-benchmark-report.md',
    title: 'Agentic Bazel benchmark',
    description: 'An end-to-end comparison of direct shell Bazel and bazel-mcp coding attempts.',
  },
  {
    source: 'docs/toon-agentic-benchmark-report.md',
    destination: 'toon-agentic-benchmark-report.md',
    title: 'TOON agentic benchmark',
    description: 'A provider-token and MCP-payload comparison of compact JSON and TOON results.',
  },
  {
    source: 'docs/bep-transport-performance.md',
    destination: 'bep-transport-performance.md',
    title: 'BEP transport performance',
    description: 'Measurement and results for file-tail and loopback Build Event Service ingestion.',
  },
  {
    source: 'docs/storage-performance.md',
    destination: 'storage-performance.md',
    title: 'Storage design and performance',
    description: 'The filesystem storage decision, data flow, durability behavior, and controlled comparison.',
  },
  {
    source: 'docs/ledger-performance-report.md',
    destination: 'ledger-performance-report.md',
    title: 'Ledger performance report',
    description: 'Feature coverage, failure cases, latency measurements, and improvement opportunities.',
  },
  {
    source: 'SECURITY.md',
    destination: 'security.md',
    title: 'Security',
    description: 'Supported versions, vulnerability reporting, and the local execution threat model.',
  },
  {
    source: 'CONTRIBUTING.md',
    destination: 'contributing.md',
    title: 'Contributing',
    description: 'Development prerequisites, validation targets, fixtures, and contribution conventions.',
  },
  {
    source: 'CHANGELOG.md',
    destination: 'changelog.md',
    title: 'Changelog',
    description: 'Release Please-managed history of user-visible project changes.',
  },
];

await rm(generatedRoot, { recursive: true, force: true });
await mkdir(generatedRoot, { recursive: true });

for (const document of documents) {
  const sourcePath = path.join(repositoryRoot, document.source);
  const destinationPath = path.join(generatedRoot, document.destination);
  let body = await readFile(sourcePath, 'utf8');

  body = body.replace(/^# .+\r?\n(?:\r?\n)?/, '');
  body = body
    .replaceAll('../CONTRIBUTING.md', './contributing.md')
    .replace(
      /\.\.\/examples\/config\.toml/g,
      'https://github.com/ewhauser/bazel-mcp/blob/main/examples/config.toml',
    )
    .replace(
      /\.\.\/scripts\/benchmarks\/run-mcp-inspect-latency\.py/g,
      'https://github.com/ewhauser/bazel-mcp/blob/main/scripts/benchmarks/run-mcp-inspect-latency.py',
    )
    .replace(
      /\]\((?:\.\/)?([a-z0-9-]+)\.md(#[^)]+)?\)/gi,
      (_match, slug, anchor = '') => `](../${slug}/${anchor})`,
    );

  const editUrl = `https://github.com/ewhauser/bazel-mcp/edit/main/${document.source}`;
  const frontmatter = [
    '---',
    `title: ${JSON.stringify(document.title)}`,
    `description: ${JSON.stringify(document.description)}`,
    `editUrl: ${JSON.stringify(editUrl)}`,
    '---',
    '',
  ].join('\n');

  await writeFile(destinationPath, frontmatter + body, 'utf8');
}
