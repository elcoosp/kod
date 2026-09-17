// @ts-check
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

// https://astro.build/config
export default defineConfig({
  site: 'https://docs.kod.dev',
  integrations: [
    starlight({
      title: 'KOD',
      description:
        'Documentation for KOD — the local-first AI coding agent harness for the terminal. Your code never leaves your machine.',
      logo: {
        src: './src/assets/logo.png',
        alt: 'KOD logo',
      },
      favicon: '/favicon.svg',
      social: [
        {
          icon: 'github',
          label: 'KOD on GitHub',
          href: 'https://github.com/elcoosp/kod',
        },
      ],
      lastUpdated: false,
      customCss: ['./src/styles/custom.css'],
      head: [
        {
          tag: 'meta',
          attrs: { property: 'og:image', content: '/og.png' },
        },
        {
          tag: 'meta',
          attrs: { name: 'twitter:card', content: 'summary_large_image' },
        },
        {
          tag: 'meta',
          attrs: { name: 'twitter:image', content: '/og.png' },
        },
      ],
      tableOfContents: {
        minHeadingLevel: 2,
        maxHeadingLevel: 3,
      },
      pagefind: true,
      sidebar: [
        {
          label: 'Get Started',
          items: [
            { label: 'Overview', slug: 'overview' },
            { label: 'Installation', slug: 'getting-started/installation' },
            { label: 'Quickstart', slug: 'getting-started/quickstart' },
            { label: 'Your First Session', slug: 'getting-started/first-session' },
          ],
        },
        {
          label: 'Guides',
          items: [
            { label: 'Terminal UI (TUI)', slug: 'guides/tui' },
            { label: 'Scriptable CLI', slug: 'guides/cli' },
            { label: 'Configuration', slug: 'guides/configuration' },
            { label: 'Model Profiles', slug: 'guides/profiles' },
            { label: 'Skills', slug: 'guides/skills' },
            { label: 'Memory', slug: 'guides/memory' },
            { label: 'Tools', slug: 'guides/tools' },
            { label: 'Policy & Permissions', slug: 'guides/policy' },
            { label: 'Sandboxing', slug: 'guides/sandboxing' },
            { label: 'Agent Swarms', slug: 'guides/swarm' },
            { label: 'Sessions, Replay & Checkpoints', slug: 'guides/sessions' },
            { label: 'Hooks', slug: 'guides/hooks' },
            { label: 'Troubleshooting', slug: 'guides/troubleshooting' },
          ],
        },
        {
          label: 'Reference',
          items: [
            { label: 'CLI Commands', slug: 'reference/cli' },
            { label: 'TUI Keybindings', slug: 'reference/tui-keys' },
            { label: 'Tools Reference', slug: 'reference/tools' },
            { label: 'Config Reference', slug: 'reference/config' },
            { label: 'Skill File Format', slug: 'reference/skill-format' },
            { label: 'Changelog', slug: 'reference/changelog' },
          ],
        },
        {
          label: 'Concepts',
          items: [
            { label: 'Architecture', slug: 'concepts/architecture' },
            { label: 'The Agent Loop', slug: 'concepts/agent-loop' },
            { label: 'Task Router', slug: 'concepts/router' },
            { label: 'LLM Providers', slug: 'concepts/providers' },
          ],
        },
        {
          label: 'Developers',
          items: [
            { label: 'Testing', slug: 'developers/testing' },
            { label: 'Contributing', slug: 'developers/contributing' },
          ],
        },
      ],
    }),
  ],
});
