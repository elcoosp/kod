import { defineCollection } from 'astro:content';
import { docsLoader, i18nLoader } from '@astrojs/starlight/loaders';
import { docsSchema, i18nSchema } from '@astrojs/starlight/schema';
import { fileURLToPath } from 'node:url';

// Correct path: from src/content.config.ts up to <docs>/node_modules
const toolkitI18nPath = fileURLToPath(new URL('../node_modules/@elcoosp/docs-toolkit/i18n/', import.meta.url));

export const collections = {
  docs: defineCollection({ loader: docsLoader(), schema: docsSchema() }),
  i18n: defineCollection({ loader: i18nLoader({ base: toolkitI18nPath }), schema: i18nSchema() }),
};
