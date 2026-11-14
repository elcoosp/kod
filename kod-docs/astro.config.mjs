import { defineDocsConfig } from '@elcoosp/docs-toolkit/config';
import starlightChangelogs, { makeChangelogsSidebarLinks } from 'starlight-changelogs';
export default await defineDocsConfig({
  plugins: [starlightChangelogs()],
  llmsTxt: {},
  site: 'https://kod.vercel.app',
  domain: 'kod.vercel.app',
  title: "KOD",
  description: "Documentation for KOD — the local-first AI coding agent harness for the terminal. Your code never leaves your machine.",
  extraStarlight: {
      expressiveCode: {
      	themes: ['github-light', 'github-dark']
      },
      title: 'KOD',
      description:
        'Documentation for KOD — the local-first AI coding agent harness for the terminal. Your code never leaves your machine.',
      favicon: '/favicon.png',
      social: [
        {
          icon: 'github',
          label: 'KOD on GitHub', translations: { fr: "KOD sur GitHub" },
          href: 'https://github.com/elcoosp/kod'
        },
      ],
      lastUpdated: false,
      tableOfContents: {
        minHeadingLevel: 2,
        maxHeadingLevel: 3
      },
      pagefind: true,
      sidebar: [
        {
          label: 'Get Started', translations: { fr: "Démarrage" },
          items: [
            { label: 'Overview', slug: 'overview' },
            { label: 'Installation', slug: 'guides/installation' },
            { label: 'Quickstart', translations: { fr: "Démarrage rapide" }, slug: 'tutorials/quickstart' },
            { label: 'Your First Session', translations: { fr: "Votre première session" }, slug: 'tutorials/first-session' },
          ]
        },
            {
          label: 'Guides',
          items: [
            { label: 'Terminal UI (TUI)', translations: { fr: "Interface terminal (TUI)" }, slug: 'guides/tui' },
            { label: 'Scriptable CLI', translations: { fr: "CLI scriptable" }, slug: 'guides/cli' },
            { label: 'Configuration', translations: { fr: "Configuration" }, slug: 'guides/configuration' },
            { label: 'Model Profiles', translations: { fr: "Profils de modèle" }, slug: 'guides/profiles' },
            { label: 'Skills', translations: { fr: "Compétences" }, slug: 'guides/skills' },
            { label: 'Memory', translations: { fr: "Mémoire" }, slug: 'guides/memory' },
            { label: 'Tools', translations: { fr: "Outils" }, slug: 'guides/tools' },
            { label: 'Policy & Permissions', translations: { fr: "Politique et permissions" }, slug: 'guides/policy' },
            { label: 'Sandboxing', translations: { fr: "Bac à sable" }, slug: 'guides/sandboxing' },
            { label: 'Agent Swarms', translations: { fr: "Essaims d'agents" }, slug: 'guides/swarm' },
            { label: 'Sessions, Replay & Checkpoints',
              translations: { fr: "Sessions, relecture et points de contrôle" }, slug: 'guides/sessions' },
            { label: 'Hooks', translations: { fr: "Hooks" }, slug: 'guides/hooks' },
            { label: 'MCP Servers', translations: { fr: "Serveurs MCP" }, slug: 'guides/mcp' },
            { label: 'Language Servers', translations: { fr: "Serveurs de langage" }, slug: 'guides/lsp' },
            { label: 'Telemetry', translations: { fr: "Télémétrie" }, slug: 'guides/telemetry' },
            { label: 'Troubleshooting', translations: { fr: "Dépannage" }, slug: 'guides/troubleshooting' },
            { label: 'FAQ', translations: { fr: "FAQ" }, slug: 'guides/faq' },
          ]
        },
            {
          label: 'Reference',
          items: [
            { label: 'CLI Commands', translations: { fr: "Commandes CLI" }, slug: 'reference/cli' },
            { label: 'TUI Keybindings', translations: { fr: "Raccourcis TUI" }, slug: 'reference/tui-keys' },
            { label: 'Tools Reference', translations: { fr: "Référence des outils" }, slug: 'reference/tools' },
            { label: 'Config Reference', translations: { fr: "Référence de configuration" }, slug: 'reference/config' },
            { label: 'Skill File Format', translations: { fr: "Format de fichier de compétence" }, slug: 'reference/skill-format' },
            { label: 'Crates', slug: 'reference/crates' },
            { label: 'Changelog', slug: 'reference/changelog' },
          ]
        },
            {
          label: 'Concepts',
          items: [
            { label: 'Core crates', translations: { fr: "Crates de base" }, slug: 'explanation/core-crates' },
            { label: 'Architecture', slug: 'explanation/architecture' },
            { label: 'The Agent Loop', translations: { fr: "La boucle de l'agent" }, slug: 'explanation/agent-loop' },
            { label: 'Internal URLs', translations: { fr: "URL internes" }, slug: 'explanation/internal-urls' },
            { label: 'Minimized Output', translations: { fr: "Sortie minimisée" }, slug: 'explanation/output-minimization' },
            { label: 'Goals & Decisions', translations: { fr: "Objectifs et décisions" }, slug: 'explanation/session-state' },
            { label: 'The Repository Map', translations: { fr: "La carte du référentiel" }, slug: 'explanation/repo-map' },
            { label: 'Compaction', translations: { fr: "Compactage" }, slug: 'explanation/compaction' },
            { label: 'Task Router', translations: { fr: "Routeur de tâches" }, slug: 'explanation/router' },
            { label: 'LLM Providers', translations: { fr: "Fournisseurs de LLM" }, slug: 'explanation/providers' },
          ]
        },
            {
          label: 'Developers', translations: { fr: "Développeurs" },
          items: [
            { label: 'Testing', translations: { fr: "Tests" }, slug: 'guides/testing' },
            { label: 'Contributing', translations: { fr: "Contribution" }, slug: 'guides/contributing' },
          ]
        },
        {
          label: 'Changelog',
          items: makeChangelogsSidebarLinks([
            { type: 'all', base: 'changelog', label: 'All versions' },
          ]),
        },
      
        {
          label: 'Changelog',
          items: makeChangelogsSidebarLinks([
            { type: 'all', base: 'changelog', label: 'All versions' },
          ]),
        },
      
        {
          label: 'Changelog',
          items: makeChangelogsSidebarLinks([
            { type: 'all', base: 'changelog', label: 'All versions' },
          ]),
        },
      
        {
          label: 'Changelog',
          items: makeChangelogsSidebarLinks([
            { type: 'all', base: 'changelog', label: 'All versions' },
          ]),
        },
      
        {
          label: 'Changelog',
          items: makeChangelogsSidebarLinks([
            { type: 'all', base: 'changelog', label: 'All versions' },
          ]),
        },
      
        {
          label: 'Changelog',
          items: makeChangelogsSidebarLinks([
            { type: 'all', base: 'changelog', label: 'All versions' },
          ]),
        },
      ]
    },
});
