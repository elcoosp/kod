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
            { label: 'Overview', translations: { fr: 'Aperçu' }, slug: 'overview' },
            { label: 'Installation', translations: { fr: "Installation" }, slug: 'getting-started/installation' },
            { label: 'Quickstart', translations: { fr: "Démarrage rapide" }, slug: 'getting-started/quickstart' },
            { label: 'Your First Session', translations: { fr: "Votre première session" }, slug: 'getting-started/first-session' },
          ]
        },
            {
          label: 'Guides', translations: { fr: 'Guides' },
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
          label: 'Reference', translations: { fr: 'Référence' },
          items: [
            { label: 'CLI Commands', translations: { fr: "Commandes CLI" }, slug: 'reference/cli' },
            { label: 'TUI Keybindings', translations: { fr: "Raccourcis TUI" }, slug: 'reference/tui-keys' },
            { label: 'Tools Reference', translations: { fr: "Référence des outils" }, slug: 'reference/tools' },
            { label: 'Config Reference', translations: { fr: "Référence de configuration" }, slug: 'reference/config' },
            { label: 'Skill File Format', translations: { fr: "Format de fichier de compétence" }, slug: 'reference/skill-format' },
            { label: 'Crates', translations: { fr: "Crates" }, slug: 'reference/crates' },
            { label: 'Changelog', translations: { fr: 'Journal des modifications' }, slug: 'reference/changelog' },
          ]
        },
            {
          label: 'Concepts', translations: { fr: "Concepts" },
          items: [
            { label: 'Core crates', translations: { fr: "Crates de base" }, slug: 'concepts/core-crates' },
            { label: 'Architecture', translations: { fr: "Architecture" }, slug: 'concepts/architecture' },
            { label: 'The Agent Loop', translations: { fr: "La boucle de l'agent" }, slug: 'concepts/agent-loop' },
            { label: 'Internal URLs', translations: { fr: "URL internes" }, slug: 'concepts/internal-urls' },
            { label: 'Minimized Output', translations: { fr: "Sortie minimisée" }, slug: 'concepts/output-minimization' },
            { label: 'Goals & Decisions', translations: { fr: "Objectifs et décisions" }, slug: 'concepts/session-state' },
            { label: 'The Repository Map', translations: { fr: "La carte du référentiel" }, slug: 'concepts/repo-map' },
            { label: 'Compaction', translations: { fr: "Compactage" }, slug: 'concepts/compaction' },
            { label: 'Task Router', translations: { fr: "Routeur de tâches" }, slug: 'concepts/router' },
            { label: 'LLM Providers', translations: { fr: "Fournisseurs de LLM" }, slug: 'concepts/providers' },
          ]
        },
            {
          label: 'Developers', translations: { fr: "Développeurs" },
          items: [
            { label: 'Testing', translations: { fr: "Tests" }, slug: 'developers/testing' },
            { label: 'Contributing', translations: { fr: "Contribution" }, slug: 'developers/contributing' },
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
