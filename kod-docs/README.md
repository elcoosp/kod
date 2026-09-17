# KOD Documentation Site

Documentation for [KOD](https://github.com/elcoosp/kod) — the local-first AI coding agent harness for the terminal, written in Rust.

Built with **Astro 7** and **@astrojs/starlight 0.42.0**.

## Commands

| Command | Action |
| --- | --- |
| `npm install` | Install dependencies |
| `npm run dev` | Start the dev server at `localhost:4321` |
| `npm run build` | Build the production site to `./dist/` |
| `npm run preview` | Preview the production build locally |

Search (Pagefind) is generated automatically during `npm run build`.

## Project structure

```
kod-docs/
├── astro.config.mjs        # Starlight config: branding, sidebar, social
├── src/
│   ├── content.config.ts   # Starlight docs collection
│   ├── content/docs/       # All documentation pages (MDX)
│   │   ├── index.mdx               # Landing page (splash + hero)
│   │   ├── overview.mdx
│   │   ├── getting-started/        # Installation, quickstart, first session
│   │   ├── guides/                 # TUI, CLI, config, skills, memory, FAQ, …
│   │   ├── reference/              # Commands, keys, tools, config, changelog
│   │   ├── concepts/               # Architecture, agent loop, router, providers
│   │   └── developers/             # Testing, contributing
│   ├── assets/logo.svg     # Header logo
│   ├── styles/custom.css   # “Rust Terminal” theme (tokens + components)
│   └── components/         # (reserved for overrides)
├── public/
│   ├── favicon.svg
│   └── og.png              # Social preview image
└── package.json
```

## Content conventions

These docs describe KOD **as it exists**, verified against the source:

- Every CLI command, flag, and default on the reference pages is checked against the clap definitions in `crates/kod-cli/src/commands.rs`.
- Every config key and default on the config reference is checked against `crates/kod-config`.
- Every keybinding and slash command is checked against `crates/kod-tui/src/keybindings.rs` and `crates/kod-tui/src/app.rs` (`SLASH_COMMANDS`).
- User-facing pages avoid contributor internals (test counts, crate test suites) — that belongs in the Developers section.
- Aspirational text belongs in issues or `docs/SPEC.md`, never in these pages.

## Theming

The visual identity lives in `src/styles/custom.css`:

- **Palette** — warm charcoal surfaces with a rust-orange accent, over Starlight's `--sl-color-*` custom properties (dark default, light via `:root[data-theme='light']`).
- **Typography** — Inter Variable (UI) and JetBrains Mono Variable (code), bundled locally via Fontsource — no runtime font CDN.
- **Hero terminal** — the landing page's animated terminal mock is plain HTML/CSS (frontmatter `hero.image.html`), with `prefers-reduced-motion` respected.

Fonts are imported at the top of `custom.css`; adjust tokens in the `:root` blocks.
