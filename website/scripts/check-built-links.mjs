import { readFile, readdir, stat } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const websiteRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const distRoot = path.join(websiteRoot, 'dist');
const basePath = '/bazel-mcp/';

async function listHtmlFiles(directory) {
  const entries = await readdir(directory, { withFileTypes: true });
  const files = await Promise.all(
    entries.map(async (entry) => {
      const entryPath = path.join(directory, entry.name);
      if (entry.isDirectory()) return listHtmlFiles(entryPath);
      return entry.name.endsWith('.html') ? [entryPath] : [];
    }),
  );
  return files.flat();
}

async function exists(targetPath) {
  try {
    return (await stat(targetPath)).isFile();
  } catch {
    return false;
  }
}

function publicPathForFile(filePath) {
  const relative = path.relative(distRoot, filePath).split(path.sep).join('/');
  if (relative === 'index.html') return basePath;
  if (relative.endsWith('/index.html')) {
    return `${basePath}${relative.slice(0, -'index.html'.length)}`;
  }
  return `${basePath}${relative}`;
}

const broken = [];
const htmlFiles = await listHtmlFiles(distRoot);

for (const htmlFile of htmlFiles) {
  const html = await readFile(htmlFile, 'utf8');
  const references = [...html.matchAll(/\b(?:href|src)=(?:"([^"]+)"|'([^']+)')/g)].map(
    (match) => (match[1] ?? match[2]).replaceAll('&amp;', '&'),
  );
  const pageUrl = new URL(publicPathForFile(htmlFile), 'https://docs.example');

  for (const reference of references) {
    if (
      reference.startsWith('#') ||
      reference.startsWith('//') ||
      /^(?:data|javascript|mailto|tel):/.test(reference)
    ) {
      continue;
    }

    const url = new URL(reference, pageUrl);
    if (url.origin !== pageUrl.origin) continue;
    if (!url.pathname.startsWith(basePath)) {
      broken.push(`${publicPathForFile(htmlFile)} -> ${reference} (outside ${basePath})`);
      continue;
    }

    const relativeTarget = decodeURIComponent(url.pathname.slice(basePath.length));
    const candidates = [];
    if (!relativeTarget || relativeTarget.endsWith('/')) {
      candidates.push(path.join(distRoot, relativeTarget, 'index.html'));
    } else {
      candidates.push(path.join(distRoot, relativeTarget));
      if (!path.extname(relativeTarget)) {
        candidates.push(path.join(distRoot, relativeTarget, 'index.html'));
        candidates.push(path.join(distRoot, `${relativeTarget}.html`));
      }
    }

    if (!(await Promise.all(candidates.map(exists))).some(Boolean)) {
      broken.push(`${publicPathForFile(htmlFile)} -> ${reference}`);
    }
  }
}

if (broken.length > 0) {
  console.error(`Found ${broken.length} broken local link(s):\n${broken.join('\n')}`);
  process.exitCode = 1;
} else {
  console.log(`Checked ${htmlFiles.length} HTML files; all local links resolve.`);
}
