import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

export default defineConfig({
  site: 'https://ewhauser.github.io',
  base: '/bazel-mcp',
  trailingSlash: 'always',
  integrations: [
    starlight({
      title: 'bazel-mcp',
      description:
        'Run Bazel from MCP-compatible coding agents without filling their context windows with build logs.',
      favicon: '/og.png',
      lastUpdated: true,
      customCss: ['./src/styles/custom.css'],
      social: [
        {
          icon: 'github',
          label: 'GitHub',
          href: 'https://github.com/ewhauser/bazel-mcp',
        },
      ],
      head: [
        {
          tag: 'meta',
          attrs: {
            property: 'og:image',
            content: 'https://ewhauser.github.io/bazel-mcp/og.png',
          },
        },
        {
          tag: 'meta',
          attrs: {
            name: 'twitter:card',
            content: 'summary_large_image',
          },
        },
        {
          tag: 'meta',
          attrs: {
            name: 'theme-color',
            content: '#07100d',
          },
        },
      ],
      sidebar: [
        {
          label: 'Start',
          items: [
            { label: 'Overview', slug: '' },
            { label: 'Get started', slug: 'getting-started' },
          ],
        },
        {
          label: 'Use bazel-mcp',
          items: [
            { label: 'How it works', slug: 'concepts/how-it-works' },
            { label: 'Tools', slug: 'tools' },
            { label: 'Debug a failure', slug: 'guides/debugging-failures' },
            { label: 'Long-running builds', slug: 'guides/long-running-builds' },
            {
              label: 'Write a Starlark reducer',
              slug: 'guides/writing-starlark-reducers',
            },
          ],
        },
        {
          label: 'Reference',
          items: [
            { label: 'Configuration', slug: 'reference/generated/configuration' },
            { label: 'Custom reducers', slug: 'reference/generated/custom-reducers' },
            { label: 'Benchmarks', slug: 'reference/generated/benchmarks' },
            {
              label: 'Agentic benchmark',
              slug: 'reference/generated/agentic-benchmark-report',
            },
            {
              label: 'TOON benchmark',
              slug: 'reference/generated/toon-agentic-benchmark-report',
            },
            {
              label: 'BEP transport',
              slug: 'reference/generated/bep-transport-performance',
            },
            {
              label: 'Storage performance',
              slug: 'reference/generated/storage-performance',
            },
            {
              label: 'Starlark reducer performance',
              slug: 'reference/generated/starlark-reducer-performance',
            },
          ],
        },
        {
          label: 'Project',
          items: [
            { label: 'Architecture', slug: 'project/architecture' },
            {
              label: 'Reducer integration testing',
              slug: 'reference/generated/reducer-integration-testing',
            },
            { label: 'Security', slug: 'reference/generated/security' },
            { label: 'Contributing', slug: 'reference/generated/contributing' },
            { label: 'Changelog', slug: 'reference/generated/changelog' },
          ],
        },
      ],
    }),
  ],
});
