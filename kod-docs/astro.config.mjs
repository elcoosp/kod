// @ts-check
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';
import astroTakumi from 'astro-takumi';
import fs from 'node:fs';
import { makeRender } from './src/og-render.mjs';
// https://astro.build/config
const __logoB64 = fs.readFileSync("./src/assets/logo.png").toString("base64");
const __render = makeRender({
  logoDataUrl: "data:image/png;base64," + __logoB64,
  accentHex: "#2d8686",
  accentHexA12: "#2d86861a",
  domain: "kod.vercel.app",
  projectName: "KOD",
});

export default defineConfig({
	vite: {
		ssr: { external: ["jsdom", "astro-takumi"] },
	},
  site: 'https://kod.vercel.app',
  integrations: [
    starlight({
			routeMiddleware: "./src/routeData.ts",
      expressiveCode: {
      	themes: ['github-light', 'github-dark'],
      },
      title: 'KOD',
      description:
        'Documentation for KOD — the local-first AI coding agent harness for the terminal. Your code never leaves your machine.',


      logo: { src: './src/assets/logo.png' },
      favicon: '/favicon.png',
      social: [
        {
          icon: 'github',
          label: 'KOD on GitHub',
          href: 'https://github.com/elcoosp/kod',
        },
      ],
      lastUpdated: false,
      customCss: ['./src/styles/custom.css'],
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
            { label: 'MCP Servers', slug: 'guides/mcp' },
            { label: 'Language Servers', slug: 'guides/lsp' },
            { label: 'Telemetry', slug: 'guides/telemetry' },
            { label: 'Troubleshooting', slug: 'guides/troubleshooting' },
            { label: 'FAQ', slug: 'guides/faq' },
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
            { label: 'Crates', slug: 'reference/crates' },
            { label: 'Changelog', slug: 'reference/changelog' },
          ],
        },
            {
          label: 'Concepts',
          items: [
            { label: 'Core crates', slug: 'concepts/core-crates' },
            { label: 'Architecture', slug: 'concepts/architecture' },
            { label: 'The Agent Loop', slug: 'concepts/agent-loop' },
            { label: 'Internal URLs', slug: 'concepts/internal-urls' },
            { label: 'Minimized Output', slug: 'concepts/output-minimization' },
            { label: 'Goals & Decisions', slug: 'concepts/session-state' },
            { label: 'The Repository Map', slug: 'concepts/repo-map' },
            { label: 'Compaction', slug: 'concepts/compaction' },
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
  
		astroTakumi({
			options: {
				domBackend: "linkedom",
				fonts: [fs.readFileSync("./node_modules/@fontsource/roboto/files/roboto-latin-400-normal.woff2")],
				width: 1200,
				height: 630,
				format: "png",
			},
			render: __render,
		}),
	],
});
