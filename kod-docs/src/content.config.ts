import { glob } from 'astro/loaders';
import { defineCollection, z } from 'astro:content';
import { docsLoader, i18nLoader } from '@astrojs/starlight/loaders';
import { changelogsLoader } from 'starlight-changelogs/loader';
import { blogSchema } from 'starlight-blog/schema';
import { docsSchema, i18nSchema } from '@astrojs/starlight/schema';
import { autoSidebarLoader } from 'starlight-auto-sidebar/loader';
import { autoSidebarSchema } from 'starlight-auto-sidebar/schema';
import { makeDocsCollection } from '@elcoosp/docs-toolkit';


export const collections = {
  docs: makeDocsCollection({ blogSchema }),
  autoSidebar: defineCollection({ loader: autoSidebarLoader(), schema: autoSidebarSchema() }),
  changelogs: defineCollection({
    loader: changelogsLoader([
      {
        provider: 'keep-a-changelog',
        base: 'changelog',
        changelog: '../CHANGELOG.md',
        title: 'Changelog',
      },
    ]),
  }),

  i18n: defineCollection({ loader: i18nLoader(), schema: i18nSchema() }),
};
